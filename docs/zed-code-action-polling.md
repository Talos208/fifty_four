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
4. **shortcut(`editor: toggle code actions`)は LSP へ新規リクエストを送らない。** 自動
   ポーリングが `code_actions_for_selection` に置いた結果(`Fetching` ならその Task ごと、
   `Ready` ならその中身)を読んで表示するだけ。つまり同一選択に対してサーバへ届くリクエストは
   実質1回(自動ポーリング分)のみで、「shortcut を押すと2回目のリクエストが飛んでくる」は
   誤り(§4 で詳述、旧実装がこの誤りに基づいていた)。

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

## 4. shortcut は表示専用で、新規リクエストを送らない

`crates/editor/src/code_actions.rs`、`toggle_code_actions`(3-187行、`ToggleCodeActions`
アクションのハンドラ。shortcut や電球クリックから呼ばれる)。要となるのは106-119行:

```rust
let toggle_task = cx.spawn_in(window, async move |editor, cx| {
    let (resolved_tasks, debug_scenarios, task_context) = runnable_task.await?;

    let code_actions = if let Some(CodeActionSource::RunMenu(_)) = &deployed_from {
        None
    } else {
        editor.update(cx, |editor, _cx| match &editor.code_actions_for_selection {
            CodeActionsForSelection::None => None,
            CodeActionsForSelection::Fetching(task) => Some(task.clone()),
            CodeActionsForSelection::Ready(action_fetch_ready) => {
                Some(Task::ready(Some(action_fetch_ready.clone())).shared())
            }
        })?
    };
```

`editor.code_actions_for_selection` を読むだけで、`textDocument/codeAction` を新規に
送る処理はどこにも無い。3状態それぞれの挙動:

- `None`(自動ポーリングがまだ一度も起きていない、または選択変更直後でまだ着手していない)
  → `code_actions = None` → 151行目 `if actions.is_empty() { return }` で**何も起きない**。
- `Fetching(task)`(自動ポーリングのリクエストが進行中)→ その `task` を `.await` する
  (=既存のフェッチに相乗り)。この場合サーバ視点では新しいリクエストは届かない。
- `Ready(..)`(自動ポーリングが完了済み)→ その結果をそのまま使う。これも新しいリクエストは
  届かない。

つまり **shortcut を押した瞬間に何かがサーバへ届くことは無い**。サーバが実際に受け取る
リクエストは、選択が安定してから250ms後に発火する自動ポーリング1回だけ(選択が変わらない
限り増えない)。

## 影響・fifty_four 側の対処

上記1〜3の組み合わせにより:

- `trigger_kind` で「明示的起動」を判別できない → サーバ側は「選択範囲の有無」でしか
  ゲートできない(`code_action` ハンドラの `should_run`)。
- 自動ポーリングを止める設定が無い → 選択中は常に(250ms間隔で)リクエストが飛んでくる
  前提で設計する必要がある。
- 単一スロット差し替え+キャンセルにより、ユーザーの操作テンポが LLM 応答より速いと
  一度も完走できない。

**旧実装その1(デバウンス方式、廃止済み)**: `code_action_last_call: DashMap<uri, Instant>` に
前回 LLM まで進んだ時刻を記録し、`CODE_ACTION_DEBOUNCE`(800ms)未満の後続リクエストを
弾いていた。だがこの方式には根本的な欠陥があった: 自動ポーリング(選択直後の250ms後)が
デバウンス窓を開始させてしまうため、その窓内(〜1050ms)にユーザーが shortcut を押すと
弾かれて反応しない、という報告([選択→shortcut→反応しない]・[LLM始動中→shortcut→反応
しない])につながった。しかも弾いた側には結果を返す手段が無かった(時刻しか記録して
いない)ため、窓を抜けてから押すと LLM を丸ごと呼び直す(コスト2倍)問題もあった。

**旧実装その2(「1回目は記録のみ・2回目で起動」方式、廃止済み)**: デバウンス(時間による
足切り)を「同一の(選択範囲, 対象テキスト)への1回目のリクエストは記録するだけで LLM を
呼ばず、2回目以降で初めて呼ぶ」というリクエスト回数ベースの判定に置き換えた。「shortcut を
押すと Zed は同じキーで改めて問い合わせてくるはず」という前提だったが、これは上記§4の通り
**誤りだった**: shortcut はサーバへ新規リクエストを送らないため、2回目のリクエストが
物理的に発生せず、常に1回目の「記録のみ」で止まって反応しなくなった(実機検証で
`code_action: first request` と `code_action: skip` のログしか出ないことで発覚)。

**現行実装(ジョブ方式、2回目を待たない)**: `code_action_jobs: DashMap<uri, (JobKey,
RunningJob)>`(`code_action::decide_job` / `JobKey` / `RunningJob`)。ゲートを通った
リクエストは**1回目でも即座に** LLM を起動する。その代わり:

- LLM 呼び出しは `tokio::spawn` の detached task に切り出し、`self`(ハンドラの future)
  から独立させる。このリクエストが `$/cancelRequest` で drop されても、task は
  `self.llm` の Mutex を握ったまま生き続け完走する。
- 対象範囲・対象テキストから作る `JobKey` が直前のジョブと一致する場合(同一選択への
  後続リクエスト)は新規に LLM を呼ばず、`tokio::sync::watch::Receiver` を clone して
  同じ task の結果に合流(`Decision::Join`)する。task が完了済みならそのまま即座に
  返す(=キャッシュとして機能する)。
- `JobKey` が変わった(別の範囲が選ばれた)のに前のジョブが実行中だった場合は、新しい
  ジョブを起動する前に古い task を `abort()` する。これが唯一のコスト制御であり、
  「選択が落ち着かないうちに次々と別の範囲を選ぶ」動作を1回分の LLM 呼び出しに収束させる
  (Zed 自身の250msクライアント側デバウンスと組み合わさり、実際に発火する自動ポーリングの
  回数自体もある程度抑えられている)。

副作用として、ガター電球の即時表示は自動ポーリングの完了を待つ(1回目のリクエストの
完了を待つ)ため、旧来の即時表示ではなくなる場合がある。これは Zed 側のフェッチが
完了するまでの時間に依存する挙動で、サーバ側では制御していない。

## 未検証・今後気になったら見る点

- 他の LSP クライアント(VS Code 等)が `trigger_kind: INVOKED` を送ってくるかは未確認。
  今の実装では `trigger_kind` の値に関わらず1回目から即座に LLM を起動するため、
  影響は無いはずだが未検証。
- `codeActionResolve`(lazy resolution)を使えば「電球は即座に出すが、選択されるまで LLM を
  呼ばない」設計にできないか検討したが、`codeAction/resolve` は選ばれた**1件**の `edit` を
  後から埋める仕組みであり、複数候補を後出しすることはできない(LSP仕様上の制約)。今回の
  「候補を複数提示する」設計とは相容れないため採用しなかった。
