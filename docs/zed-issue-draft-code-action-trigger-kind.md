# Zed への issue 下書き: `textDocument/codeAction` の `CodeActionContext.trigger_kind` が常に未送信

`docs/zed-code-action-polling.md` の調査（「※穴埋め/表現改善」code action 実装、および
その後の shortcut 反応不良の調査）で判明した Zed 本体側の問題。fifty_four 側は
「選択範囲の有無」と「同一選択への複数回リクエスト」の組み合わせで回避済みだが、
根本原因は Zed 側にあるため upstream へ報告する。

**未提出。** 以下、そのまま GitHub issue として使える形式の下書き。

---

## Title

`CodeActionContext.trigger_kind` is never sent on `textDocument/codeAction` requests

## Body

### Description

The LSP spec defines `CodeActionTriggerKind`:

> - `Invoked` (1): Code actions were explicitly requested by the user or by an extension.
> - `Automatic` (2): Code actions were requested automatically. This typically happens when
>   current selection in a file changes, but can also be triggered when file content changes.

This lets a language server distinguish "the user asked for this" from "the client is polling
in the background (e.g. to decide whether to show a lightbulb)" — an important signal for
servers whose code actions are expensive to compute (e.g. backed by an LLM call, a build, or a
network request), since those servers need to avoid running that cost on every keystroke/selection
change while still responding promptly to an explicit request.

Zed never populates this field. Every `textDocument/codeAction` request — whether triggered by
the 250ms-debounced background poll that runs on every selection change, or by the user pressing
the "toggle code actions" keybinding / clicking the lightbulb — arrives with
`trigger_kind: None`. A server cannot tell these two situations apart from the request itself.

### Where (pinned to `90b15493109a2e1267cd3a6bc4c24cc0106ad5dc`)

`GetCodeActions`, the single internal type used to build every `textDocument/codeAction`
request Zed sends, doesn't even carry a field for it:

https://github.com/zed-industries/zed/blob/90b15493109a2e1267cd3a6bc4c24cc0106ad5dc/crates/project/src/lsp_command.rs#L255-L258

```rust
pub(crate) struct GetCodeActions {
    pub range: Range<Anchor>,
    pub kinds: Option<Vec<lsp::CodeActionKind>>,
}
```

`GetCodeActions::to_lsp` builds the `CodeActionContext` via `..Default::default()`, which leaves
`trigger_kind: None`:

https://github.com/zed-industries/zed/blob/90b15493109a2e1267cd3a6bc4c24cc0106ad5dc/crates/project/src/lsp_command.rs#L2981-L2990

```rust
Ok(lsp::CodeActionParams {
    text_document: make_text_document_identifier(path)?,
    range: range_to_lsp(self.range.to_point_utf16(buffer))?,
    work_done_progress_params: Default::default(),
    partial_result_params: Default::default(),
    context: lsp::CodeActionContext {
        diagnostics: relevant_diagnostics,
        only,
        ..lsp::CodeActionContext::default()   // <-- trigger_kind: None here
    },
})
```

This is the *only* code path that constructs a `textDocument/codeAction` request in Zed. Both
call sites go through it identically:

- The background poll, `Editor::refresh_code_actions_for_selection`, fired unconditionally on
  every selection change after a 250ms debounce:
  https://github.com/zed-industries/zed/blob/90b15493109a2e1267cd3a6bc4c24cc0106ad5dc/crates/editor/src/code_actions.rs#L379-L388
- The explicit user action, `Editor::toggle_code_actions` (bound to the "toggle code actions"
  keybinding and the lightbulb click) — though note this path usually doesn't even issue a new
  request; it mostly just displays whatever `code_actions_for_selection` already holds from the
  background poll:
  https://github.com/zed-industries/zed/blob/90b15493109a2e1267cd3a6bc4c24cc0106ad5dc/crates/editor/src/code_actions.rs#L106-L119

Neither path threads any notion of "why" into `GetCodeActions`, so there's structurally no way
for `trigger_kind` to ever be anything but `None` today.

### Reproduction

Implement a language server that logs `params.context.trigger_kind` in its
`textDocument/codeAction` handler. Open a file with that language server active, place the
cursor, then select some text. The handler is invoked repeatedly (once per settled selection,
per the 250ms debounce) with `trigger_kind: None` every time — including when explicitly
invoking the code actions menu.

### Expected behavior

Per spec, requests originating from the automatic background poll
(`refresh_code_actions_for_selection`) should set `trigger_kind: Some(CodeActionTriggerKind::AUTOMATIC)`.
This alone would let servers reliably opt out of expensive work on background polls without
resorting to heuristics like "does the request have a non-empty selection" (which cannot
distinguish "user is dragging a selection" from "user explicitly asked for actions on this
exact selection").

Explicit invocation is a secondary, harder ask given `toggle_code_actions` currently reuses the
cached background-poll result rather than issuing a fresh request — but at minimum, populating
`AUTOMATIC` on the polling path would give servers a working signal where today there is none.

### Impact

Without this, a language server that wants to gate expensive code actions on explicit user
intent has no reliable request-level signal and must resort to fragile workarounds (e.g.
inferring intent from selection-range heuristics, or reusing detached background tasks and
hoping a follow-up request arrives).

---

## 補足(fifty_four 側の対応)

fifty_four 自体は Zed の修正を待たず、「選択範囲の有無」でカーソルのみの自動ポーリングを
弾いた上で、LLM 呼び出しを detached task に切り出し・キー単位で single-flight する方式
（`code_action::decide_job`、`docs/zed-code-action-polling.md` 参照）で対応済み。
本 issue は Zed 側の根本原因を報告し、将来的に `trigger_kind` が仕様通り送られるように
なれば、その回避策(選択範囲ヒューリスティクス)を単純化できるようにするためのもの。
