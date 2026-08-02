# fifty_four ドキュメント

小説・会話テキスト向け Zed 拡張 + Rust LSP サーバの設計・構成ドキュメント。

## 目次

| ドキュメント | 内容 |
|---|---|
| [architecture.md](architecture.md) | 全体アーキテクチャ、Backend コンポーネント |
| [project-structure.md](project-structure.md) | ディレクトリ構成、クレート一覧 |
| [lsp-handlers.md](lsp-handlers.md) | LSP ハンドラと提供機能 |
| [completion.md](completion.md) | 文章補完フロー、CursorContext |
| [zed-completion-filtering.md](zed-completion-filtering.md) | Zed の補完フィルタ機構と「括弧内で候補が出ない」問題の調査記録 |
| [character-updater.md](character-updater.md) | キャラクター設定自動更新 |
| [acp-agent.md](acp-agent.md) | ACP エージェント、チャット内容の補完コンテキスト化 |
| [data-layer.md](data-layer.md) | インメモリ状態、SQLite スキーマ |

## 概要

**fifty_four** は次の 3 層で構成される。

1. **Zed 拡張** (`extension/`) — LSP サーバの起動と初期化オプション
2. **LSP サーバ** (`lsp/`) — テキスト解析、LLM 補完、キャラ設定更新
3. **埋め込みプロンプト** (`data/`) — LLM 向け system / completion / character update プロンプト

中心機能は **Lindera による日本語形態素解析** と **LLM による文章補完・キャラクター設定の自動更新**。

同じバイナリを `--acp` 付きで起動すると **ACP エージェント**として動き、Zed の Agent Panel
から作者の相談相手になる。中身は Claude Agent SDK 経由の `claude` CLI で、原稿ディレクトリの
ファイルを読み書きしながら話す。その会話の要約は補完のコンテキストとして LSP 側へ渡る
（[acp-agent.md](acp-agent.md)）。
