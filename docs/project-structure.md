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
        ├── tools.rs        # CharacterInfoTool / PlotInfoTool (LlmTool実装)。parse_plot_md は plot.rs に委譲
        ├── plot.rs         # plot.md 解析(front matter + `# 章名` 見出し区切り)。PlotInfoTool と inlay hint ハンドラが共有
        ├── frontmatter.rs  # YAML frontmatter パース(gray_matter)。プロンプトテンプレート・plot.md の両方から使う
        ├── types.rs        # LineData, CachedLinderaToken, CursorContext
        └── logging.rs      # OpenTelemetry / tracing 初期化(現状 main() からは未呼び出し。実際は env_logger)
```

## クレート

| クレート | パス | 説明 |
|---|---|---|
| `fifty_four_lsp` | `lsp/` | tower-lsp ベースの LSP サーバ |
| Zed 拡張 | `extension/` | `zed_extension_api` で LSP を起動 |
| `prepare` | `prepare/` | 開発用タスクランナー。`cargo prepare package` で他PC配布用の `dist/` を生成 |

## クロスビルド (aarch64-pc-windows-msvc)

Windows on ARM 向けにネイティブ ARM64 バイナリを作る場合、x64 Windows 開発機から以下の手順でクロスコンパイルできる。

### 前提条件

1. `rustup target add aarch64-pc-windows-msvc`
2. Visual Studio の「MSVC v143 (または v142) - C++ ARM64/ARM64EC ビルドツール」コンポーネント(link.exe の ARM64 対応、Windows SDK の ARM64 lib)
3. **clang**(aws-lc-sys のビルドに必要。`winget install LLVM.LLVM` 等で導入)
4. **CMake** と **Ninja**(aws-lc-sys のビルドに必要。Visual Studio Installer に同梱されていれば追加コンポーネント不要。同梱の場所の例:
   `<VSインストール先>\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin` と
   `<VSインストール先>\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja`)

上記の clang / cmake / ninja はインストールしただけでは `PATH` に自動追加されない(特に winget の LLVM や VS 同梱の CMake/Ninja)。ユーザー環境変数 `Path` に以下を恒久的に追加しておく(追加後は新しいターミナルから有効):

```
C:\Program Files\LLVM\bin
<VSインストール先>\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin
<VSインストール先>\Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja
```

### ビルド

```bash
cargo prepare package --release --target aarch64-pc-windows-msvc
```

`dist/fifty-four-aarch64/` に ARM64 ネイティブの `fifty_four_lsp.exe` を含む配布一式が生成される(拡張の wasm・`data/` は arch 非依存でホストビルドと共通)。引数なしの `cargo prepare package` は従来どおり host(x64)向けに `dist/fifty-four/` を生成し、動作は変わらない。

### 既知の注意点

- `aws-lc-sys`(rustls の暗号バックエンド、genai/opentelemetry-otlp が推移的に依存)が clang・CMake・Ninja を要求する。これが無いと `cc-rs: failed to find tool "clang"` のようなエラーで `cargo build`/`cargo check` が失敗する。
- `onig_sys`(comrak → syntect)・`libsqlite3-sys`(rusqlite, bundled)・`zstd-sys`(rust-embed) は C ソース同梱ビルドだが、VS の ARM64 ツールセットがあれば追加設定なしで通る。
- `lindera`(embed-ipadic)の辞書はビルド時に生成される arch 非依存データなので、クロスビルドでの追加対応は不要。

## 言語定義

`extension/languages/fiftyfour/config.toml` で FiftyFour 言語を定義。

- 対象: `path_suffixes = ["txt", "plot.md", "characters.md"]`(完全一致サフィックスのみ)。
  `memo/*.md` のようなグロブは `path_suffixes` では表現できないため、ユーザーが Zed の
  `settings.json` の `languages.file_types` で FiftyFour に割り当てる運用になっている
  (`docs/lsp-handlers.md` 参照)。
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
