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

## 既知の制約

- `--acp`(debug ビルド限定、claude CLI 依存)は対象外。
- LLM を使う機能は、今の settings では無効(`initialization_options` が空)。偽の LLM サーバを追加して決定的にする予定。
- 待ち合わせの条件(LSP の span)と画面比較のハーネスは未実装。
