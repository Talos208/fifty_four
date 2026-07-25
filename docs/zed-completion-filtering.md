# Zed の補完フィルタと「括弧内で候補が出ない」問題

LLM 補完（`completion` ハンドラ）が返した候補が、**括弧内などカーソル直前に日本語がある状況で
Zed のメニューに表示されない**問題の調査記録。Zed 本体（`zed-industries/zed`、`main` ブランチ時点）の
実ソースで根本原因を確定した。行番号は Zed main のもので、バージョンにより前後する。

## 症状

- Ctrl+Space で明示起動し、カーソルを動かさず待っても、括弧内だと候補が出ない（タイムアウトではない）。
- LSP のログ上は候補を正常に3件返している（`Ok completions`）のにメニューに出ない。
- 句点・閉じ括弧・改行の直後では出る（「たまに出る」の実態）。括弧内で多発。

## 結論（最重要）

**Zed は「表示フィルタ」と「挿入位置(replace_range)」を別レイヤで扱う。**
我々が張る `textEdit` の range は *挿入時にどこを置換するか* だけを決め、*候補を表示するかのフィルタには一切関与しない*。
フィルタは Zed がカーソル周辺のバッファから独自に作る「クエリ」で行い、**クエリ（＝カーソル直前の語）が
候補の表示ラベルの部分文字列でなければ候補は除外される**。LLM の「続き文」候補は打った語で始まらないため、
括弧内（直前に語がある）では全滅する。

## コールチェーン（呼び元付き）

### エントリポイント
- `[Ctrl+Space]` → `Editor::show_completions`（`crates/editor/src/completions.rs:40`）
- `[トリガ文字入力]` → `Editor::trigger_completion_on_input`（`completions.rs:~99`）
- どちらも → `Editor::open_or_update_completions_menu`（`completions.rs:246`）に合流。

### ① クエリ算出（表示フィルタ用）
```
Editor::open_or_update_completions_menu            completions.rs:246
  └ Editor::completion_query(snapshot, position)   completions.rs:316（定義 :758）
       └ MultiBufferSnapshot::surrounding_word(offset, CharScopeContext::Completion)
     query = word_range.start..offset のテキスト（カーソル直前の「語」／直前が非語なら None）
```
- 語判定は `CharClassifier`（語文字＝`is_alphanumeric() || '_'` 等）。`is_alphanumeric()` は Unicode の
  Letter/Number を見るので**かな漢字も語文字**。括弧「起きたら|」→ query =「起きたら」。
- 句点/閉じ括弧/改行の直後 → 直前が非語 → query = None（空）。
- **この offset はカーソル位置で、我々の textEdit range は無関係。**

### ② LSP 要求 → レスポンス受信 → `CoreCompletion` 化（Project 層）
```
Editor::open_or_update_completions_menu            completions.rs:246
  └ provider.completions(buffer, position, ctx)    completions.rs:463
       └ <Editor as CompletionProvider>::completions completions.rs:1388
            └ Project::completions(...)             crates/project/src/lsp_store.rs:6702
                 └ GetCompletions {...} を各 LSP へ  lsp_store.rs:6739
                      └ GetCompletions::response_from_lsp(...)  crates/project/src/lsp_command.rs
                         ├ 各 lsp::CompletionItem の text_edit を parse_completion_text_edit で解釈
                         │   → replace_range / new_text を確定
                         └ CoreCompletion { replace_range, new_text,
                               source: Lsp { lsp_completion: Box<CompletionItem>, ... } }
```
- ここでは生の `lsp::CompletionItem`（label・filter_text 等）を Box で保持するだけ。CodeLabel はまだ作らない。
- `replace_range` は「確定時の置換範囲」用で、③④のフィルタには渡らない。

### ③ ラベル解決（`CodeLabel` 生成 ＝ フィルタ対象が決まる場所）
```
populate_labels_for_completions(new_completions, language, ...)  lsp_store.rs:14380
  └ adapter.labels_for_completions(items, language)              lsp_store.rs:14396
  │    FiftyFour は独自ラベルを持たない → None
  └ CodeLabel::fallback_for_completion(&lsp_completion, language) lsp_store.rs:14414
       （定義 crates/language/src/language.rs:1394）
          let text = /* lsp label (+detail) */
          let filter_range = item.filter_text
              .and_then(|f| text.find(f).map(|ix| ix..ix + f.len()))
              .unwrap_or(0..label_length)   // ← find 失敗なら label 全体
  └ Completion { label, replace_range, new_text, source, ... }   lsp_store.rs:14416
```
- `CodeLabel::filter_text()` は `&self.text[filter_range]`（`crates/language_core/src/code_label.rs:139`）
  ＝**必ずラベル文字列の部分文字列**。
- **LSP の `filterText` は、それがラベル文字列に `find` できる場合のみ反映**。できなければラベル全体が対象。

### ④ メニュー構築 → ファジーフィルタ（ここで消える）
```
Editor::open_or_update_completions_menu            completions.rs:246
  └ CompletionsMenu::new(id, source, ..., query, ..., completions, ...) completions.rs:630
       （定義 crates/editor/src/code_context_menus.rs:320）
       └ 各候補で StringMatchCandidate::new(id, completion.label.filter_text())  ccm.rs:340
  └ menu.do_async_filtering(query.unwrap_or_default(), position, &buffer, cx)   completions.rs:657
       （定義 code_context_menus.rs:1297）
       └ fuzzy::match_strings(candidates, query, ...)   ccm.rs:1327
         query を候補文字列にサブシーケンス照合。一致しない候補は除外＝非表示。
```

### （参考）確定時
```
[Enter/Tab] → Editor::confirm_completion            completions.rs:49
  └ Editor::do_completion(item_ix, Complete, ...)    completions.rs:772
       └ 選択候補の replace_range を new_text で置換
```

---

全体像：Zed は「表示フィルタ」と「挿入位置」を別物として扱う

これが混乱の元です。我々が張った `textEdit`（range）は **挿入時にどこを置換するか** だけを決め、**候補を表示するかどうか（フィルタ）には一切関与しません**。フィルタは Zed が独自にカーソル周辺のバッファから作る「クエリ」で行われます。

以下、5段階で追います。

---

### ① LSP レスポンス受信 → `CoreCompletion` 化
`crates/project/src/lsp_command.rs` の `GetCompletions::response_from_lsp` 内、`completions.retain(...)` ループ

- サーバの `Vec<lsp::CompletionItem>` を1件ずつ処理。
- 各アイテムの `text_edit`（＝我々のゼロ幅 TextEdit）を `parse_completion_text_edit` で解釈し、`replace_range`（バッファのアンカー範囲）と `new_text` を確定。
- 生成物：`CoreCompletion { replace_range, new_text, source: Lsp { lsp_completion: Box<CompletionItem>, … } }`。
- **生の `lsp::CompletionItem`（`label` / `filter_text` 等）はそのまま Box で保持**。この時点で CodeLabel はまだ作らない。

`★ Insight ─────────────────────────────────────`
ここで `replace_range` は「確定時の置換範囲」として保存されるだけ。**この後のフィルタ工程はこの range を参照しません**。だからゼロ幅にしても表示は変わらなかった。
`─────────────────────────────────────────────────`

---

### ② ラベル解決（`CodeLabel` の構築）
`CodeLabel::fallback_for_completion`（`crates/language/src/language.rs:1394`）

- 言語アダプタの `label_for_completion` はデフォルト `None`（同 1544 付近）。FiftyFour は独自ラベルを持たないので `None` → フォールバックが呼ばれる。
- その中身（1426-1451 付近）：
```rust
let label = &item.label;                 // LSP の label 文字列
let text  = /* label (+ detail 等) */;   // 表示テキスト
let filter_range = item.filter_text
    .and_then(|filter| text.find(filter).map(|ix| ix..ix + filter.len()))
    .unwrap_or(0..label_length);         // ← filter_text が text 内に無ければ label 全体
CodeLabel { text, runs, filter_range }
```
- `CodeLabel::filter_text()`（`crates/language_core/src/code_label.rs:139`）は `&self.text[self.filter_range]` ＝ **必ずラベル文字列の部分文字列**。

`★ Insight ─────────────────────────────────────`
**#26（filter_text に行プレフィックスを渡した修正）の敗因はここ**。我々の `filterText="「起きたら…"` は `text="。すぐに連絡を」"` の中に `find` されず、`filter_range` が `0..label_length`（＝ラベル全体）にフォールバック。結果、`filter_text()` はラベル全文になり、我々が渡した値は完全に無視された。**LSP の filterText は「ラベルの部分文字列」でない限り効かない**、というのが Zed 実装の仕様。
`─────────────────────────────────────────────────`

---

### ③ フィルタクエリの算出
`crates/editor/src/completions.rs` の `completion_query`

```rust
let (word_range, kind) = buffer.surrounding_word(offset, Some(CharScopeContext::Completion));
if offset > word_range.start && kind == Some(CharKind::Word) {
    Some(text_for_range(word_range.start..offset))  // クエリ = カーソル直前の「語」
} else {
    None                                            // 直前が非語ならクエリ無し
}
```

- `surrounding_word` は `CharClassifier`（語文字＝`is_alphanumeric() || '_'` 等）でカーソル直前の語範囲を求める。Rust の `is_alphanumeric()` は Unicode の Letter/Number を見るので、**かな漢字も「語文字」**。
- 括弧内「起きたら|」→ 直前が語 → **クエリ =「起きたら」**。
- 句点・閉じ括弧・改行の直後 → 直前が非語 → **クエリ = None（空）**。
- ここでも `offset` はカーソル位置で、**我々の textEdit range は無関係**。

---

### ④ マッチ候補構築 → ファジーマッチ
`crates/editor/src/code_context_menus.rs` の `CompletionsMenu::new`（320-）→ `do_async_filtering`

- 候補ごとに `StringMatchCandidate::new(id, completion.label.filter_text())`（340行）。
  → **マッチ対象＝②で決まった `filter_text()`（今回はラベル全体「。すぐに連絡を」）**。
- `fuzzy::match_strings(候補, クエリ, …)`（1327行付近）で、クエリを候補文字列に**サブシーケンス（順序を保った部分列）照合**。マッチしない候補は結果から除外＝**メニューに出ない**。
- クエリが None なら全候補マッチ＝表示。

---

### ⑤ なぜ括弧内で消えたか（結論）

- **括弧内**：クエリ「起きたら」を候補「。すぐに連絡を」にサブシーケンス照合 → 起・き・た・ら はどれも含まれない → マッチ0件 → **全候補が非表示**。
- **括弧外（句点/改行/括弧直後）**：クエリ None → 全候補表示（＝「タイムアウトでないのにたまに出る／出ない」の正体）。
- ゼロ幅 textEdit は①で挿入位置に使われるだけで、③④のフィルタには絡まないので、表示は救えなかった。

`★ Insight ─────────────────────────────────────`
**通常の LSP 補完が成立する理由**との対比が本質です。`pri` と打って `println` が出るのは、クエリ `pri` が候補ラベル `println` の接頭辞（サブシーケンス）だから。標準の補完は「打ちかけの語を置換し、候補はその語で始まる」形になっている。我々の「続き文」候補は**打った語で始まらない**ので、この暗黙の前提を破っていた。
`─────────────────────────────────────────────────`

---

### 修正（token 置換方式）が効いた理由

- token「起きたら」を label 先頭に付ける → ②で `filter_range = 0..label_len`（token 含む）→ `filter_text()` に token が入る。
- ③のクエリ「起きたら」が label の接頭辞になり、④のサブシーケンス照合が成功 → 表示。
- 確定時は `replace_range` が token を覆い、`new_text = token+続き` なので、実質「続きだけ挿入」になる（見た目の結果は不変）。

つまり「表示のためだけに、打った語を候補ラベルにも含めざるを得ない」——これが Zed のフィルタ設計上の制約です。

---

## なぜ括弧内で消えるか

- **括弧内**: ③でフィルタ対象＝ラベル全体「。すぐに連絡を」。④で query「起きたら」をサブシーケンス照合
  → 起・き・た・ら はどれも含まれず一致0件 → 全候補が非表示。
- **括弧外（句点/改行/括弧直後）**: ①の query が None → `unwrap_or_default()` で空文字 → 全候補一致 → 表示。

通常の LSP 補完（`pri`→`println`）が出るのは、query `pri` が候補ラベル `println` の接頭辞だから。
「続き文」候補は打った語で始まらないため、この暗黙の前提を破っていた。

## 失敗した修正（記録）

`filter_text` にカーソル手前の行テキスト全体を設定する案は**無効**。理由は③のとおり、渡した
`filterText` がラベル文字列（続き文）に含まれず `text.find` が失敗し、`filter_range` が
`0..label_length`（ラベル全体）にフォールバックして無視されるため。**任意の hidden な filterText で
表示を制御することは Zed の実装上できない。**

## 有効な修正（token 置換方式）

「カーソル直前の語トークン」を置換対象にし、`new_text` と `label` を『トークン＋続き』にする。

- ③でラベルがトークン始まり → `filter_range = 0..label_length` にトークンが含まれる。
- ①の query（＝そのトークン）がラベルの接頭辞 → ④のサブシーケンス照合が成功 → 表示。
- 確定時は `replace_range` がトークンを覆い、`new_text = トークン+続き` なので実質「続きだけ挿入」。見た目不変。

実装は `lsp/src/main.rs` の `completion` ハンドラ（`precursor_word` ヘルパでトークン算出、置換 range 化、
`label = token + 続き`）。トークンが空（句点/括弧/改行直後）の場合は従来のゼロ幅挿入と等価で回帰なし。

## 残課題（表示改善）

token 置換方式では、各候補ラベルの先頭に打鍵済みトークンが表示されて冗長。④の描画
（`code_context_menus.rs:899-1020` の `split_completion_label`）が `filter_range` を使って候補を
「main 部分」と「suffix 部分」に分けて描く仕組みがあるため、これを利用して**フィルタには効かせつつ
表示だけトークンを目立たせない／隠す**余地がある。

検証の結果、`split_completion_label`（`code_context_menus.rs:1677`）は `text.split_at(filter_range.end)`
で分割するだけで、`main_text`/`suffix_text` は**どちらも必ず描画される**（隠す機能は無い）。したがって
「完全に隠す」ことはアーキテクチャ上不可能。実現できるのは「位置を変える」ことのみ:

- token を `label` ではなく LSP の `detail` フィールドへ（`filterText` も同じ値に）→
  Zed の `text = "{label} {detail}"` 組み立てにより、`label`（続き文そのまま、プレフィックス無し）が
  主表示、`detail`（token）が末尾の補助テキストになる（`completion_detail_alignment` 設定で行末寄せも可）。
  マッチ対象は既存の `precursor_word`（alphanumeric run、Zed のクエリと厳密に同一）のままで正しさを維持できる。

ただし、この制約自体が Zed 側の設計（マッチ文字列＝表示ラベルの部分文字列に限定）に起因するため、
根本原因を upstream へ報告した:

**https://github.com/zed-industries/zed/issues/61646**
（`filterText` が label の部分文字列でない場合に無条件破棄される挙動の修正提案。下書きは
`docs/zed-issue-draft-filter-text.md`）

この issue が対応されれば、`filterText` を任意の文字列として独立指定できるようになり、
label にトークンを含める必要自体が無くなる可能性がある。それまでは上記の `detail` フィールド移動案が
現実的な回避策。
