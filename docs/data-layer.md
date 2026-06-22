# データ層

## インメモリ状態

`Backend` が保持する主要ストア。

| ストア | 型 | 内容 | 用途 |
|---|---|---|---|
| `text` | `DashMap<String, Vec<LineData>>` | URI → 行テキスト + Lindera トークンキャッシュ | 補完・ハイライト・更新トリガ |
| `character_cache` | `CharacterCache` | キャラ MD のパース結果 | `CharacterInfoTool` / 更新適用 |
| `update_states` | `DashMap<String, Arc<Mutex<UpdateState>>>` | URI ごとの編集蓄積状態 | キャラ更新の発火判定 |
| `workspace` | `Arc<Mutex<Vec<WorkspaceFolder>>>` | ワークスペースフォルダ | キャラファイル探索 |
| `llm` / `background_llm` | `Arc<Mutex<Option<Box<dyn LlmClient>>>>` | LLM クライアント | 補完 / キャラ更新 |

## LineData

```rust
pub struct LineData {
    pub text: String,
    pub tokens: Vec<CachedLinderaToken>,  // Lindera 解析結果（遅延填充）
}
```

## CachedLinderaToken

```rust
pub struct CachedLinderaToken {
    pub details: Vec<String>,   // 品詞情報
    pub byte_start: usize,
    pub byte_end: usize,
    pub tag: TokenStatus,     // Normal / InBracket
}
```

## SQLite (debug ビルドのみ)

`FlightRecorder` が `lsp/migrations/` のスキーマで管理。補完とキャラ更新の Flight Recorder として動作。

### completions / completion_candidates

```sql
-- V1__create_completions.sql
completions (id, created_at, document_uri, cursor_line, cursor_character, model_name, prompt)
completion_candidates (id, completion_id, rank, candidate, selected)
```

補完リクエストごとにプロンプトを記録し、生成された候補とユーザーの選択を追跡する。

### character_updates / character_update_sections

```sql
-- V2__character_updates.sql
character_updates (id, started_at, completed_at, document_uri, model_name, prompt, response)
character_update_sections (id, update_id, character_name, attribute, old_text, new_text, applied, skip_reason)
```

キャラ更新タスクの実行履歴と、セクション単位の適用結果を記録する。

## 埋め込みプロンプト

`data/` 配下の Markdown は `rust-embed` でバイナリに埋め込まれる（`Asset` 構造体）。

`load_prompt(name)` の処理:

1. 埋め込みファイルを読み込み
2. `gray_matter` で YAML frontmatter を分離
3. 本文 + frontmatter オプション（`max_tokens`, `temperature`, `schema` 等）を返却

frontmatter のオプションは `use_llm_with_option` が LLM 呼び出し前に適用する。
