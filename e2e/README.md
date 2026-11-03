# E2E 環境(Zed + IME + fifty_four_lsp)

キー入力 → 画面変化の対応を確かめるための実行環境。まずはコンテナとして作り、
同じイメージを microVM(krunvm / smolvm / Firecracker など)でも起動できるようにする方針。

## 中身

| 層 | 使っているもの | 備考 |
|---|---|---|
| 表示 | Xvfb (`:99`, 1600x1000x24) | IME と入力注入がやりやすい X11 を選んだ |
| 描画 | Mesa lavapipe + `ZED_ALLOW_EMULATED_GPU=1` | GPU なしで Zed の Vulkan 描画を動かす |
| IME | fcitx5 + fcitx5-mozc(XIM 経由) | `fcitx5-remote -o` / `-c` で ON/OFF |
| エディタ | Zed(`ZED_VERSION` で固定、既定 `v1.26.2`) | GitHub Releases の Linux 版 tarball |
| LSP | `fifty_four_lsp`(debug ビルド) | `/usr/local/bin`。settings.json の `binary.path` で指定 |
| 拡張 | `extension/` を wasm32-wasip2 でビルドし、`installed/fifty-four` に配置 | |
| 入力・撮影 | xdotool / ImageMagick `import` | |
| フォント | Noto CJK | 画面比較のため、テーマ・フォント・サイズを `docker/zed-settings.json` で固定 |

## ビルドと起動

```bash
# リポジトリルートで
docker build -f e2e/Dockerfile -t fifty-four-e2e:latest .
# Zed のバージョンを変える
docker build -f e2e/Dockerfile --build-arg ZED_VERSION=v1.25.3 -t fifty-four-e2e:v1.25.3 .

mkdir -p e2e/out
docker run --rm -d --name ff-e2e -v "$PWD/e2e/out:/out" fifty-four-e2e:latest
```

起動時に `entrypoint.sh` が、Xvfb → D-Bus → fcitx5 → Zed(`~/workspace/test.txt` を開く)の順で立ち上げる。
ログは `/out/{xvfb,fcitx5,zed}.log` に出る。

## 手で動かす

```bash
# スクリーンショット
docker exec ff-e2e import -window root /out/screen.png

# IME を ON にして、ローマ字で入力・変換・確定
docker exec ff-e2e fcitx5-remote -o
docker exec ff-e2e xdotool type --delay 50 'nekodearu'
docker exec ff-e2e xdotool key space Return
docker exec ff-e2e fcitx5-remote -c

# 補完を明示的に呼ぶ
docker exec ff-e2e xdotool key ctrl+space
```

## 動作確認済みのこと(Zed v1.26.2)

- 本物の `fifty_four_lsp` を含む完全なイメージのビルドと実行を、WSL のコンテナ(`wslc`)で確認した。
  Docker 固有の機能には依存していない。
- lavapipe で描画できる(Zed のログに `Selected GPU adapter: "llvmpipe …" (Vulkan)`)。日本語は Noto CJK で表示される。
- `installed/fifty-four` に置いた拡張が読み込まれ、`.txt` が `FiftyFour` 言語になり、`binary.path` の LSP が起動する。
- IME: `fcitx5-remote -o` → ローマ字 → Space で変換 → Return で確定、の流れでバッファに確定文字列が入る。

## 調べてわかったこと

- **信頼確認**: ワークスペースが未信頼だと Restricted Mode になり、**LSP が起動しない**。
  `session.trust_all_worktrees: true` で確認ダイアログを出さないようにしている。
- **X11 の preedit はインライン表示されない**: XIM 経由では、変換中の文字は fcitx5 の小窓に出て、
  Zed のバッファには確定時に初めて入る。つまり変換中は `didChange` が飛ばない。
  Wayland(text-input-v3)ではインライン preedit になるはずなので、挙動を比べたくなったら Wayland 版も要る。
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
