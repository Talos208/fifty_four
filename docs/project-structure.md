# プロジェクト構成

## ディレクトリツリー

```
fifty_four/
├── Cargo.toml              # workspace (lsp + extension + prepare)
├── README.adoc
├── AGENTS.md               # エージェント概要
├── data/                   # プロンプト (実行ファイル隣接を優先、無ければ rust-embed 埋め込みへフォールバック)
│   ├── system.md
│   ├── prompt_completion.md
│   ├── prompt_completion_after_sentence.md
│   ├── prompt_completion_after_bracket.md
│   ├── prompt_completion_before_bracket.md
│   ├── prompt_completion_empty_bracket.md
│   ├── prompt_completion_in_bracket.md
│   └── prompt_character_update.md
├── db/                     # SQLite (fifty_four.db)。実行ファイル隣接の db/ に作成、git管理対象外
├── docs/                   # 設計ドキュメント（本ディレクトリ）
├── extension/              # Zed 拡張
│   ├── Cargo.toml
│   ├── extension.toml
│   ├── src/lib.rs          # LSPバイナリ探索(settings.json → PATH → 作業ディレクトリ再帰探索)・初期化オプション
│   └── languages/fiftyfour/config.toml  # 言語定義（括弧ルール等）
├── prepare/                # 配布用パッケージング補助 (`cargo prepare package`)
│   └── src/main.rs         # lsp/extensionビルド + dist/への集約 + wasm後処理
└── lsp/                    # LSP サーバ本体
    ├── Cargo.toml
    ├── migrations/         # SQLite スキーマ (debug ビルド)
    │   ├── V1__create_completions.sql
    │   └── V2__character_updates.sql
    └── src/
        ├── main.rs         # Backend, LSP ハンドラ, キャラ MD パース
        ├── highlight.rs    # Lindera トークン化 → セマンティックトークン
        ├── cursor_context.rs  # 補完モード分類
        ├── llm.rs          # LlmInterface トレイト / LlmClient 実装 / LlmClientBuilder
        ├── character_updater.rs  # バックグラウンド更新タスク
        ├── tools.rs        # CharacterInfoTool / PlotInfoTool (LlmTool実装) / parse_plot_md
        ├── types.rs        # LineData, CachedLinderaToken, CursorContext
        └── logging.rs      # OpenTelemetry / tracing 初期化(現状 main() からは未呼び出し。実際は env_logger)
```

## クレート

| クレート | パス | 説明 |
|---|---|---|
| `fifty_four_lsp` | `lsp/` | tower-lsp ベースの LSP サーバ |
| Zed 拡張 | `extension/` | `zed_extension_api` で LSP を起動 |
| `prepare` | `prepare/` | 開発用タスクランナー。`cargo prepare package` で他PC配布用の `dist/` を生成 |

## 言語定義

`extension/languages/fiftyfour/config.toml` で FiftyFour 言語を定義。

- 対象: `.txt`, `.md`
- 括弧: `「」` `『』` `《》` `｜《》` `（）`（各ペアに close / newline 設定）

## 主要依存関係 (lsp)

| クレート | 用途 |
|---|---|
| `tower-lsp` | LSP サーバフレームワーク |
| `lindera` | 日本語形態素解析 (IPADIC) |
| `genai` | LLM API 抽象化 |
| `comrak` | キャラクター MD パース |
| `gray_matter` | プロンプト frontmatter 解析 |
| `rust-embed` | `data/` プロンプトのバイナリ埋め込み |
| `dashmap` | URI ごとのテキスト・状態管理 |
| `rusqlite` + `refinery` | debug ビルドの Flight Recorder |
| `opentelemetry` 系 | トレース / メトリクス / ログ |
