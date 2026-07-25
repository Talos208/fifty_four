# Zed への issue 下書き: `CompletionItem.filterText` が label の部分文字列でない場合、無視されてラベル全体がフィルタ対象になる

`docs/zed-completion-filtering.md` の調査で判明した Zed 本体側の問題。fifty_four 側は token 置換方式
（`main.rs` の `precursor_word`）で回避済みだが、根本原因は Zed 側にあるため upstream へ報告する。

**提出済み: https://github.com/zed-industries/zed/issues/61646**

以下、そのまま GitHub issue として使える形式の下書き（提出時の本文と同一）。

---

## Title

`filterText` is silently ignored (falls back to whole label) when it isn't a literal substring of the label

## Body

### Description

Per the LSP specification, `CompletionItem.filterText` is:

> A string that should be used when filtering a set of completion items. When `falsy` the label is used.

The spec does not require `filterText` to be a substring of `label`. It's meant to let a language
server hand the client a string to match the user's typed prefix against, independently of what is
displayed as `label`. This is exactly the "insert something different from what's displayed, but
match on the typed prefix" pattern — e.g. a snippet-like completion whose label is a full sentence,
autocompleted based on already-typed context that isn't repeated in the label.

Zed's implementation, however, only honors `filterText` when it happens to occur as a literal
substring of the label text. When it doesn't, `filterText` is discarded entirely and the whole label
is used for filtering instead — silently, with no fallback that still respects the server-provided
`filterText` string itself.

### Where (pinned to `a3ac036eb6b73e0a50af4a44c96a43f1abf1b989`)

`CodeLabel::fallback_for_completion`, used whenever a language doesn't provide its own
`label_for_completion`:

https://github.com/zed-industries/zed/blob/a3ac036eb6b73e0a50af4a44c96a43f1abf1b989/crates/language/src/language.rs#L1437-L1453

```rust
let text = if let Some(detail) = item.detail.as_deref().filter(|detail| detail != label) {
    format!("{label} {detail}")
} else if let Some(description) = item
    .label_details
    .as_ref()
    .and_then(|label_details| label_details.description.as_deref())
    .filter(|description| description != label)
{
    format!("{label} {description}")
} else {
    label.clone()
};
let filter_range = item
    .filter_text
    .as_deref()
    .and_then(|filter| text.find(filter).map(|ix| ix..ix + filter.len()))
    .unwrap_or(0..label_length);   // <-- filterText silently discarded here
CodeLabel {
    text,
    runs,
    filter_range,
}
```

`CodeLabel::filter_text()` (the string that actually gets matched against the query) is always a
substring of `text`:

https://github.com/zed-industries/zed/blob/a3ac036eb6b73e0a50af4a44c96a43f1abf1b989/crates/language_core/src/code_label.rs#L139-L141

```rust
pub fn filter_text(&self) -> &str {
    &self.text[self.filter_range.clone()]
}
```

...and that's what's used to build the fuzzy-match candidate:

https://github.com/zed-industries/zed/blob/a3ac036eb6b73e0a50af4a44c96a43f1abf1b989/crates/editor/src/code_context_menus.rs#L340

```rust
.map(|(id, completion)| StringMatchCandidate::new(id, completion.label.filter_text()))
```

So when a server-provided `filterText` isn't found inside `text` (label + detail/description), it is
dropped and the entire label becomes the match target instead. There's no code path that uses
`filterText` verbatim as an independent match string when it can't be located inside the label.

### Reproduction

Implement a language server whose completions look like:

```jsonc
{
  "label": "。すぐに連絡を」",
  "filterText": "起きたら",
  "textEdit": { "range": { "start": {...}, "end": {...} }, "newText": "。すぐに連絡を」" }
}
```

(A concrete real-world case: an LLM-backed "continue the sentence" completion provider, where the
label is the suggested continuation and `filterText` is set to whatever the user already typed, so
the completion still shows up while they keep typing — this is the standard, spec-sanctioned use of
`filterText`.)

With the cursor right after typing `起きたら` (a query Zed derives from
`Editor::completion_query`/`surrounding_word`, unrelated to the `textEdit` range), the completion is
filtered out and never appears in the menu, even though `filterText` was explicitly provided to match
it. Any query that isn't a literal substring of `label` (+ `detail`) has the same problem.

### Expected behavior

When `filterText` is present, it should be used as the match string regardless of whether it can be
located inside `text`. Visual highlighting of the matched query inside the label is a separate,
best-effort concern — if `filterText` isn't found inside `text`, Zed can simply skip highlighting
(or highlight nothing) rather than discarding the provided `filterText` and matching against the
label instead. Concretely, this likely means `CodeLabel` should be able to carry the match string
independently of `text`/`filter_range` (e.g. an owned `filter_text: Option<String>` used directly for
matching, with `filter_range` reserved purely for the "highlight this sub-range of `text`" visual
concern when it happens to apply), rather than deriving the match string exclusively from a range
into `text`.

### Related

- https://github.com/zed-industries/zed/pull/59125 fixed a related but distinct symptom (an off-by-N
  range bug in the *Rust adapter's own* label construction, not the core `fallback_for_completion`
  fallback discussed here) that also stemmed from `filterText`/label misalignment causing garbled
  display. That PR patched the Rust adapter specifically; the core fallback behavior in
  `crates/language/src/language.rs` is unaffected by it and still exhibits this issue for any
  language server that doesn't provide a custom `label_for_completion`.

---

## 補足（fifty_four 側の対応）

fifty_four 自体は Zed の修正を待たず、`lsp/src/main.rs` の `completion` ハンドラで token 置換方式
（打鍵済みのトークンを label 先頭に含める）に対応済み（`docs/zed-completion-filtering.md` 参照）。
本 issue は Zed 側の根本原因を報告し、将来的に `filterText` を仕様どおり独立指定できるようになれば、
その回避策（label にトークンを含める必要）を外せるようにするためのもの。
