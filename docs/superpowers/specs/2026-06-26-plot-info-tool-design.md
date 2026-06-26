# PlotInfoTool 設計仕様

**日付:** 2026-06-26

## 概要

`plot.md` に記述されたプロット情報を、completion 時に LLM へ tool call 経由で提供する機能を追加する。既存の `CharacterInfoTool` と同じ仕組みで動作する。

あわせて、`CharacterInfoTool` と `PlotInfoTool` を `tools.rs` に集約し、将来のツール追加に備えた構造にする。

## plot.md フォーマット

ワークスペースルートに `plot.md` を1ファイル置く。

```markdown
# 第1章

この章のプロット内容…
伏線・事件・感情の流れなど

# 第2章

この章のプロット内容…
```

- `#`（level-1 見出し）= 章名。`<章名>.txt` と対応。
- 見出し以外のすべてのコンテンツ = その章のプロット本文。

## アーキテクチャ

```
completion()
  ├── CharacterInfoTool  (tools.rs)  ← 既存・移動
  └── PlotInfoTool       (tools.rs)  ← 新規
        │
        ↓  LLM tool call: plot_info(chapter_name?)
        │
        plot.md (ワークスペースルート)
```

## tool スキーマ

```json
{
  "type": "object",
  "properties": {
    "chapter_name": {
      "type": "string",
      "description": "プロットを取得したい章の名前（省略時は全章を返す）"
    }
  },
  "required": []
}
```

| 引数 | 返り値 |
|------|--------|
| `chapter_name: "第1章"` | 第1章のプロット本文のみ |
| `chapter_name` なし | 全章を `# 章名\n本文` 形式で連結したテキスト |

## 変更ファイル

### 新規: `lsp/src/tools.rs`

- `CharacterInfoTool` を `main.rs` から移動
- `PlotInfoTool` を新規実装
- `parse_plot_md(content: &str) -> Vec<(String, String)>` ヘルパ関数

### 修正: `lsp/src/main.rs`

- `mod tools;` を追加
- `CharacterInfoTool` 定義を削除
- 以下の型・関数を `pub(crate)` に昇格（`tools.rs` から参照するため）:
  - `parse_all_content`
  - `CharacterEntry`
  - `CharacterAttribute`
  - `CharacterCache`
  - `find_character_file_path`
- `completion()` 内の `use_llm_with_option` クロージャに下記を追加:
  ```rust
  l.add_tool(tools::PlotInfoTool::new(&workspace));
  ```

### 変更なし

- `lsp/src/character_updater.rs`
- `data/` 配下のプロンプトファイル
- データベーススキーマ

## parse_plot_md の仕様

```
入力: plot.md 全文テキスト
出力: Vec<(章名: String, 本文: String)>

- level-1 見出し (`# ...`) を章の区切りとする
- 見出し行は本文に含まない
- 本文は trim して返す
- 見出しより前のコンテンツは無視する
```

## エラーハンドリング

| 状況 | 動作 |
|------|------|
| `plot.md` が存在しない | `LlmError::GenericError` を返す |
| 指定した章名が見つからない | `LlmError::GenericError` を返す |
| ファイル読み込み失敗 | `LlmError::GenericError` を返す |

## 除外スコープ

- plot.md のキャッシュ機構（CharacterCache 相当）は初期実装では持たない
- plot.md からのキャラクター設定自動更新（character_updater 相当）は対象外
- plot.md の自動書き込み機能は対象外
