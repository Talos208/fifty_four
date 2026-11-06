# E2E 環境(Zed + IME + fifty_four_lsp)

キー入力 → 画面変化の対応を確かめるための実行環境。まずはコンテナとして作り、
同じイメージを microVM(krunvm / smolvm / Firecracker など)でも起動できるようにする方針。

確かめたいこと(テスト項目)の一覧は [TEST-ITEMS.md](TEST-ITEMS.md)。

## 中身

| 層 | 使っているもの | 備考 |
|---|---|---|
| 表示 | X11: Xvfb (`:99`) / Wayland: sway の headless バックエンド(pixman で CPU 合成) | `E2E_DISPLAY=x11\|wayland` で切り替える(既定 x11)。画面は 1600x1000 |
| 描画 | Mesa lavapipe + `ZED_ALLOW_EMULATED_GPU=1` | GPU なしで Zed の Vulkan 描画を動かす |
| IME | fcitx5 + fcitx5-mozc | X11 は XIM、Wayland は text-input-v3 ⇔ input-method-v2 でつながる |
| エディタ | Zed(`ZED_VERSION` で固定、既定 `v1.26.2`) | GitHub Releases の Linux 版 tarball |
| LSP | `fifty_four_lsp`(debug ビルド) | `/usr/local/bin`。settings.json の `binary.path` で指定 |
| 拡張 | `extension/` を wasm32-wasip2 でビルドし、`installed/fifty-four` に配置 | |
| 入力・撮影 | `e2e-*` コマンド(下記) | 中身は X11 なら xdotool / ImageMagick、Wayland なら wtype / grim |
| フォント | Noto CJK | 画面比較のため、テーマ・フォント・サイズを `docker/zed-settings.json` で固定 |

## ビルドと起動

```bash
# リポジトリルートで
docker build -f e2e/Dockerfile -t fifty-four-e2e:latest .
# Zed のバージョンを変える
docker build -f e2e/Dockerfile --build-arg ZED_VERSION=v1.25.3 -t fifty-four-e2e:v1.25.3 .

mkdir -p e2e/out
docker run --rm -d --name ff-e2e -v "$PWD/e2e/out:/out" fifty-four-e2e:latest
# Wayland で動かす
docker run --rm -d --name ff-e2e -e E2E_DISPLAY=wayland -v "$PWD/e2e/out:/out" fifty-four-e2e:latest
```

起動時に `entrypoint.sh` が、表示サーバ → D-Bus → fcitx5 → Zed(`~/workspace/test.txt` を開く)の順で立ち上げ、
Zed のウィンドウが出て IME とつながるまで待ってから `e2e: ready` を出す。
ログは `/out/{xvfb,sway,fcitx5,zed-cli}.log` に出る(Zed 本体のログは `/out/Zed.log`。Windows のフォルダでは大文字小文字が区別されず、`zed.log` と `Zed.log` が同じファイルになるので、名前を分けてある)。

## テストの実行

試験は Rust(`e2e/` の独立したクレート)で書き、**ホストからコンテナを `exec` で操作**する。
ルートのワークスペースには入れていないので、`cargo test` がコンテナを要求することはない。

```bash
# リポジトリのルートから(.cargo/config.toml の alias。引数は試験の絞り込みになる)
cargo e2e                          # Wayland で、全試験を実行(X11 は既定の対象外)
cargo e2e A04                      # ID で絞る(使う表示サーバのコンテナだけ起動する)
E2E_MODES=x11 cargo e2e            # X11 で実行する(明示したときだけ)。`x11,wayland` で両方
E2E_PARALLEL=6 cargo e2e A06       # 6 本を同時に起動して、全部が通るか(TEST-ITEMS.md の A06)
E2E_REPEAT=5 cargo e2e E04         # 5 本を続けて起動して、毎回 ready になるか(E04)
```

先にイメージをビルドしておく(`docker build -f e2e/Dockerfile -t fifty-four-e2e:latest .`、wslc なら `wslc build`)。
コンテナは表示サーバごとに 1 つ、最初に使う試験が起動して、全試験で共有する(終わると止める)。
名前は `A04::wayland` の形。絞り込みは 1 つだけ指定できる(`cargo e2e E0` で E 群、`cargo e2e A04` で A04)。
試験は `e2e/tests/` のファイルごと(A 群・E 群)に別のプロセスで実行され、コンテナもファイルごとに起動する。

| 環境変数 | 意味 |
|---|---|
| `E2E_RUNTIME` | 実行系(`wslc` / `docker`)。既定は wslc が動けば wslc |
| `E2E_IMAGE` | イメージ(既定 `fifty-four-e2e:latest`) |
| `E2E_OUT_ROOT` | 結果の置き場(既定 `e2e/out`)。実行ごとに `run-<時刻>-<pid>/<表示サーバ>/` を新しく作り、既存のものは消さない |
| `E2E_MODES` | 実行する表示サーバ。既定は `wayland` だけ。X11 は `x11` と明示したときだけ実行する |
| `E2E_START_ATTEMPTS` | 共有コンテナの起動に失敗したときに、コンテナごと作り直す上限(既定 2)。作り直しは A07 が警告する |
| `E2E_START_TIMEOUT` | コンテナが ready になるまでの待ち(既定 180 秒) |
| `E2E_PARALLEL` / `E2E_REPEAT` | A06(同時起動)/ E04(連続起動)の本数。未設定ならその試験は実行しない |
| `E2E_STRICT_A07` | `1` なら、Zed を起動し直した(A07)ことを失敗にする(既定は警告) |
| `E2E_*`(上記以外) | コンテナにそのまま渡す(`E2E_ZED_WINDOW_TIMEOUT` など。entrypoint の待ち時間) |

結果(`e2e.log`・`e2e-status`・`Zed.log`・`*.png`)は結果の置き場に残る。
コンテナは `run --rm` で起動し、試験が終われば(panic しても)止める。Ctrl-C で中断した場合は
`e2e-run-…` という名前のコンテナが残ることがあるので、`kill` で止める。

**kache を使っている環境で `cargo` が固まるとき**: `RUSTC_WRAPPER= CARGO_BUILD_RUSTC_WRAPPER= CC_KNOWN_WRAPPER_CUSTOM=__none__ cargo e2e …`
のように kache を迂回すると、ビルドできることがある(このクレートの依存は Rust のみで、C は要らない)。

## 手で動かす

```bash
# どちらの表示サーバでも同じコマンドで操作できる(e2e/docker/e2e-tools.sh)
docker exec ff-e2e e2e-shot /out/screen.png        # スクリーンショット

# IME を ON にして、ローマ字で入力・変換・確定
docker exec ff-e2e e2e-ime on
docker exec ff-e2e e2e-type 'nekodearu'
docker exec ff-e2e e2e-key space Return            # キー名は X の keysym 名。修飾キーは ctrl+End のように書く
docker exec ff-e2e e2e-ime off

# 補完を明示的に呼ぶ
docker exec ff-e2e e2e-key ctrl+space
```

## 動作確認済みのこと(Zed v1.26.2)

- 本物の `fifty_four_lsp` を含む完全なイメージのビルドと実行を、WSL のコンテナ(`wslc`)で確認した。
  Docker 固有の機能には依存していない。
- lavapipe で描画できる(Zed のログに `Selected GPU adapter: "llvmpipe …" (Vulkan)`)。日本語は Noto CJK で表示される。
- `installed/fifty-four` に置いた拡張が読み込まれ、`.txt` が `FiftyFour` 言語になり、`binary.path` の LSP が起動する。
- IME: ON → ローマ字 → Space で変換 → Return で確定、の流れでバッファに確定文字列が入る(X11・Wayland とも)。

## 調べてわかったこと

- **信頼確認**: ワークスペースが未信頼だと Restricted Mode になり、**LSP が起動しない**。
  `session.trust_all_worktrees: true` で確認ダイアログを出さないようにしている。
- **変換中の文字(preedit)の出方が X11 と Wayland で違う**:
  - X11(XIM): fcitx5 の小窓に出て、Zed のバッファには確定時に初めて入る。
  - Wayland(text-input-v3): Zed のバッファ内に下線付きでインライン表示される。
  - Wayland では変換候補の窓が出ない。Ubuntu 24.04 の sway 1.9 が input-method のポップアップに未対応のため。
- **X11 では、Zed は最初のキー入力で初めて IME の入力コンテキストを作る**。それより前に打ったキーは IME を素通りする。
  entrypoint は Shift を押して作らせ、fcitx5 の `DebugInfo` に `focus:1` の入力コンテキストが出るまで待つ。
  Wayland では Zed の起動直後にできる。
- **Wayland は Zed 自身の判断で選ばれる**: `WAYLAND_DISPLAY` があり `DISPLAY` がなければ Wayland で起動する。
  sway は `xwayland disable` にしてあるので、誤って X11 で動くことはない。
- **起動順の競合(遅い環境で起きる)**: `fcitx5-remote` が答えても、XIM サーバや mozc はまだ読み込み中のことがある。Zed が先に XIM へ繋ぐと失敗したままになり、「IME connection」待ちでタイムアウトする。そのため entrypoint は fcitx5 の全アドオン(最後は mozc)と、X11 なら XIM サーバの登録、Wayland なら sway の出力(HEADLESS-1)を待ってから Zed を起動する。Wayland で sway の出力が確定する前に Zed が繋ぐと、Zed が `wayland/client.rs` で panic した例もある。待ち時間は `E2E_FCITX5_TIMEOUT`(60 秒)・`E2E_ZED_WINDOW_TIMEOUT`(60 秒)・`E2E_IME_TIMEOUT`(20 秒)で変えられる。
- **debug ビルドの LSP は実行ファイルの隣に `db/` を作る**(FlightRecorder)。権限がないと起動時に panic し、Zed に「Failed to run fifty-four」と出る。`/usr/local/bin/db` を e2e ユーザー所有で作ってある。
- **Wayland で Zed が起動直後に panic することがある(Zed の不具合と思われる。未解決)**:
  `crates/gpui_linux/src/linux/wayland/client.rs:2124` の `state.keymap_state.as_mut().unwrap()`(v1.26.2、
  `wl_keyboard::Event::Modifiers` の処理)。Zed が keymap を受け取る前に Modifiers が届くと落ちる。
  sway の headless には物理キーボードが無く、seat のキーボードは fcitx5 の仮想キーボードなどに頼るため、
  その順序が揺れるのが原因と見ている(確証は無い)。ホストが忙しいとき(イメージのビルド直後、他のコンテナと同時起動)に出やすく、
  6 並列の起動で 36 回中 1 回〜18 回中 9 回ほど(時間帯でばらつく)。**同じコンテナで起動し直しても落ち続けることがある**。
  entrypoint は落ちたら最大 `E2E_ZED_START_ATTEMPTS`(3)回まで起動し直し、回数を `/out/zed-attempts` に残す
  (試験 A07 が WARN にする)。常駐の仮想キーボード(`wtype -`)を先に置く案は、有意な差が出なかったので採っていない。
  ハーネスは、共有コンテナの起動に失敗したら**コンテナごと作り直す**(`E2E_START_ATTEMPTS`、既定 2)。新しいコンテナなら
  通ることが多い。作り直した事実は A07 が警告する。上流への報告の下書きは
  [docs/zed-issue-draft-wayland-keymap-panic.md](../docs/zed-issue-draft-wayland-keymap-panic.md)。
  X11 では起きない(が、X11 には別の不安定さがある。下記)。
- **X11 で、まれに IME に繋がらないまま止まる(原因未特定)**: 長く空けたあとの最初の起動で 2 回出た
  (`timeout waiting for IME connection`。fcitx5・XIM・mozc の起動順は正しく、Zed も `Try connect fcitx` まで進んでいた)。
  そのあと 25 回の連続起動と 6 並列 × 4 回では再現しなかった。待ち続けても繋がらないので、IME の待ちが時間切れになったら
  Zed を起動し直す(落ちたときと同じ経路。最大 `E2E_ZED_START_ATTEMPTS` 回)。起動し直した回数は A07 が警告する。
  起動し直しで回復するかは、まだ確かめられていない。
  失敗したとき(と起動し直す前)は、`e2e.log` の `---- ime diagnostics` に、XIM サーバの登録・X のフォーカス窓・
  fcitx5 の入力コンテキスト(`focus:` が 0 か 1 か)・Zed のプロセス数を出し、`failed-<attempt1|final>.png` に画面を残す。
  「入力コンテキストが無い」のか「あるがフォーカスが無い(`focus:0`)」のかで、原因を切り分けられる。
- **D-Bus のアドレスは固定**(`unix:path=/tmp/e2e-dbus`)。`docker exec` で入ったプロセスからも
  `fcitx5-remote` が効くようにするため。
- fcitx5 の「en / あ」ポップアップは `docker/fcitx5-config` で消している(画面比較のノイズになる)。
- ネットワークに出られない機能(拡張の自動インストール、ACP レジストリ、ログイン)はエラーをログに出すだけで、動作には影響しない。

## 既知の制約

- ビルドには `Lindera.dev`(lindera の IPADIC 辞書の配布元)へのアクセスが要る。
  これに届かない環境(Claude Code のクラウド環境など)では、LSP を偽物に差し替えた版でしか確かめられない。
- `--acp`(debug ビルド限定、claude CLI 依存)は対象外。
- LLM を使う機能は、今の settings では無効(`initialization_options` が空)。偽の LLM サーバを追加して決定的にする予定。
- 待ち合わせの条件(LSP の span)と画面比較のハーネスは未実装。
