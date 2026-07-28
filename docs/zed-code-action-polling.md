# Zed の codeAction 自動ポーリングと trigger_kind 未送信の調査記録

「※穴埋め/表現改善」code action（`lsp/src/backend.rs` の `code_action` ハンドラ）を実装する過程で
判明した、Zed 本体（`zed-industries/zed`、コミット `90b15493109a2e1267cd3a6bc4c24cc0106ad5dc`
時点。行番号はこのコミットのもので、バージョンにより前後する）側の挙動の調査記録。

サーバ側で LLM 呼び出しのコスト・レイテンシを制御する必要があったため、Zed が実際に何を・
いつ送ってくるかをソースで確認した。upstream への issue 化はしていない（記録のみ）。

## 結論(最重要)

1. **Zed は `textDocument/codeAction` の `CodeActionContext.trigger_kind` を一切送らない。**
   自動ポーリングでも明示的な起動でも常に `None`。サーバ側は「ユーザーが能動的に要求したか」を
   `trigger_kind` では判別できない。
2. **`inline_code_actions: false`(稲妻マークを消す設定)は表示を止めるだけで、自動ポーリング
   そのものは止まらない。** 選択/カーソルが変わるたびに 250ms デバウンスで
   `textDocument/codeAction` が飛び続ける。マークを消してもサーバ負荷は変わらない。
3. Zed は選択が変わるたびに「フェッチ中の1件」を新しいものへ**差し替える**。差し替え時、
   前のリクエストの Future が drop され、Zed は `$/cancelRequest` を送る。ユーザーの操作
   テンポが LLM の応答より速いと、一度も完走しないまま延々とキャンセルされ続け、稲妻マークが
   永久に出ない。

## 1. `trigger_kind` が送られない

`crates/project/src/lsp_command.rs`、`GetCodeActions::to_lsp`(2986-2989行):

```rust
context: lsp::CodeActionContext {
    diagnostics: relevant_diagnostics,
    only,
    ..lsp::CodeActionContext::default()
},
```

`trigger_kind` フィールドに触れていない → 常に `Default::default()` の `None`。これは
自動ポーリング(下記 §3)・明示的な `ToggleCodeActions` 起動のどちらでも共通の1関数を通るため、
**呼び出し経路によらず一律で `None`**。LSP 3.17 の `CodeActionTriggerKind::INVOKED`/`AUTOMATIC`
はプロトコル上存在するが、Zed クライアントはそもそも使っていない。

サーバ側でできる代替判別は「選択範囲があるかどうか」(`params.range.start != params.range.end`)
のみ。カーソルだけの自動ポーリングを弾くにはこれで足りるが、「選択済み状態での自動ポーリング」
と「選択済み状態での明示的起動」は原理的に区別不能。

## 2. 自動ポーリングを止める設定は「表示」しか止めない

`crates/editor/src/element.rs`、`layout_inline_code_actions`(1866-1871行):

```rust
if !snapshot
    .show_code_actions
    .unwrap_or(EditorSettings::get_global(cx).inline_code_actions)
{
    return None;
}
```

これは稲妻マークの**描画**を早期リターンさせるガード。一方、実際にサーバへ問い合わせる
`refresh_code_actions_for_selection`(`crates/editor/src/code_actions.rs:379`、下記§3)は、
この設定はおろかどの設定も見ずに無条件でフェッチタスクを spawn する。

`settings.json` に `"inline_code_actions": false` や `"toolbar": {"code_actions": false}`
を書いても、`textDocument/codeAction` の自動送出そのものを止める手段は無い(調べた範囲では
存在しない)。表示を消しても LLM は裏で呼ばれ続ける。

## 3. 自動ポーリングの実装(250msデバウンス・単一スロット差し替え・キャンセル)

`crates/editor/src/code_actions.rs`、`refresh_code_actions_for_selection`(379-388行):

```rust
self.code_actions_for_selection = CodeActionsForSelection::Fetching(
    cx.spawn_in(window, async move |editor, cx| {
        cx.background_executor()
            .timer(CODE_ACTIONS_DEBOUNCE_TIMEOUT)  // 250ms (editor.rs:301)
            .await;
        ...
    }),
);
```

呼び出し元(`crates/editor/src/selection.rs:1649` 等)は選択変更のたびに無条件でこの関数を叩く。
`code_actions_for_selection` は**単一スロット**のフィールドで、新しい選択変更が来ると
前の `Fetching` タスクごと差し替えられる。GPUI の `Task` が drop されると、対応する LSP
リクエストの Future も drop される。

`crates/lsp/src/lsp.rs`(1500行、`request` の内部):

```rust
let cancel_on_drop = gpui_util::defer(move || {
    ...
    Self::notify_internal::<notification::Cancel>(
        &notification_serializers,
        CancelParams { id: NumberOrString::Number(id) },
    )
    ...
});
```

Future drop → `$/cancelRequest` 送出、が全リクエスト共通の挙動として実装されている
(`fifty_four` 側は `tower-lsp-server` がこれを受けてハンドラの Future を drop する。
`lsp/src/progress.rs` の `CompletionProgress` の Drop ガードはこれを前提にしている)。

稲妻マークの表示条件は `crates/editor/src/element.rs`(8901-8906行)で
`newest_selection_head`(選択の「頭」、ドラッグ中なら現在位置)の行に限定され、かつ
`code_actions_for_selection` が `Ready`(非空)である必要がある(`code_actions.rs:322-328`)。

## 影響・fifty_four 側の対処

上記1〜3の組み合わせにより:

- `trigger_kind` で「明示的起動」を判別できない → サーバ側は「選択範囲の有無」でしか
  ゲートできない(`code_action` ハンドラの `should_run`)。
- 自動ポーリングを止める設定が無い → 選択中は常に(250ms間隔で)リクエストが飛んでくる
  前提で設計する必要がある。
- 単一スロット差し替え+キャンセルにより、ユーザーの操作テンポが LLM 応答より速いと
  一度も完走できない → `backend.rs` の `code_action_last_call` による自前デバウンス
  (前回 LLM まで進んだ呼び出し時刻との比較、`CODE_ACTION_DEBOUNCE`)で緩和している。
  ただし sleep して「最後の1件」を確実に拾う方式ではないため、短時間に選択をいじり続けた
  場合、最後の調整がデバウンス窓に着地して誰にも拾われない(=LLM が呼ばれない)ことがある。
  これは既知のトレードオフとして許容している(`backend.rs` のコメント参照)。

## 未検証・今後気になったら見る点

- 他の LSP クライアント(VS Code 等)が `trigger_kind: INVOKED` を送ってくるかは未確認。
  もし送ってくるなら、`should_run` のゲートは Zed 以外ではより正確に働くはず。
- `codeActionResolve`(lazy resolution)を使えば「電球は即座に出すが、選択されるまで LLM を
  呼ばない」設計にできないか検討したが、`codeAction/resolve` は選ばれた**1件**の `edit` を
  後から埋める仕組みであり、複数候補を後出しすることはできない(LSP仕様上の制約)。今回の
  「候補を複数提示する」設計とは相容れないため採用しなかった。
