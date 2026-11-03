# E2E 環境(Zed + IME + fifty_four_lsp)

キー入力 → 画面変化の対応を確かめるための実行環境。まずはコンテナとして作り、
同じイメージを microVM(krunvm / smolvm / Firecracker など)でも起動できるようにする方針。

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
ログは `/out/{xvfb,sway,fcitx5,zed}.log` に出る。

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
