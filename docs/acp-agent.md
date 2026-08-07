# ACP エージェント

`fifty_four_lsp --acp` で起動する ACP (Agent Client Protocol) エージェント。Zed の
Agent Panel から作者の相談相手として応答し、その会話の要約を **LSP の短文生成の
コンテキストとして渡す**。

> **debug ビルド限定。** LLM アクセスは作者自身の `claude` CLI のログイン
> (= サブスクリプション枠) をそのまま使うため、配布物(`--release`)に載せて
> 第三者へ提供することは Anthropic の規約上できない。これは注意書きではなく
> **バイナリの性質**として実装している — `--release` ビルドで `--acp` を渡すと
> 1 行メッセージを出して終了する(`lsp/src/main.rs`)。詳細は「認証」節を参照。

## なぜ必要か

LSP の補完が見ているのは、カーソル直前の 10 文（`cursor_context::before_sentences_upto`）と、
必要に応じてツールで引くプロット・キャラクター設定だけである。「この場面はこう書きたい」
「この人物の口調は変えたい」といった、**まだ本文に書かれていない作者の意図**を渡す経路が無かった。

## 構成

```
Zed ──stdio──> fifty_four_lsp --acp
                 └─ ClaudeAgent          … writing_agent.rs
                      └─ anthropic-agent-sdk
                           └─ claude CLI （サブスクリプション認証をそのまま継承）
                                └── <workspace>/.fifty_four/chat_context.md
                                        ↑ LSP サーバ（別プロセス）が補完時に読む
```

Zed は LSP サーバ（拡張経由）と ACP エージェント（`agent_servers` 経由）を**別プロセス**
として起動する。Zed の拡張 API には ACP を登録する口が無い（language server /
MCP context server / DAP のみ）ため、この分離は避けられない。受け渡しは
`<workspace>/.fifty_four/chat_context.md` の 1 ファイルで、`plot.md` を毎回読み直す
`tools.rs` と同じ方式。書き込みは一時ファイル + `rename` で原子的に行う
（`lsp/src/chat_context.rs`）。

> `.fifty_four/` は原稿を置いているワークスペース側に作られる。原稿を git 管理しているなら、
> そちらの `.gitignore` に `.fifty_four/` を足しておくとよい。

バイナリは 1 つで、`--acp` の有無でモードが変わる。ただし `acp`/`writing_agent` モジュールは
`#[cfg(debug_assertions)]` で release ビルドから丸ごと落ちる。`cargo prepare package`
(`--release` 無し) は debug バイナリを吐くので作者の手元の `dist/` では動くが、
`--release` を付けた配布物では `--acp` は起動を拒否する。

## 依存している SDK について

`anthropic-agent-sdk` (MIT) は **Anthropic 公式ではなく第三者製**である。公式の
Claude Agent SDK は Python と TypeScript のみで、他言語には「`claude` CLI を
サブプロセスとして駆動せよ」と案内されている。

ただし**公式 SDK も中身は同じで `claude` CLI のラッパー**なので、このクレートを使うことは
アーキテクチャ上の妥協ではなく、CLI 駆動を型付きで書くための省力化にすぎない。壊れたときに
自前実装へ移れるよう、クレートへの依存は `lsp/src/writing_agent.rs` の
`WritingAgent` トレイトの内側だけに閉じ込めてある。`acp.rs` はトレイト越しにしか触らない。

## 認証 — サブスクリプション枠

LLM アクセスは `claude` CLI の認証をそのまま使う。**このエージェントは API キーを要求しないし、
自前で保持もしない。** 以前は「`ANTHROPIC_API_KEY` を設定しなければサブスク枠で動く」という
ドキュメント上の但し書きに過ぎず、実際にはコード側で何も担保していなかった
（`.env` の provider キーがそのまま使われる／子プロセスが親の環境を継承する、の 2 経路で
API キー課金になり得た）。現在は次の 2 点を**コードで**担保している(`lsp/src/main.rs`)。

1. **`.env` を読まない。** `--acp` 時は `load_dev_env()` を呼ばない。ACP 経路は provider の
   API キーを一切必要としないので、そもそも読む理由が無い。
2. **プロセス環境から Anthropic の資格情報を削除する。** `anthropic-agent-sdk` は子プロセス
   (`claude` CLI) へ親プロセスの環境を丸ごと渡す(`env::vars()` → `Command::envs`)。
   `ClaudeAgentOptions::env` には削除の口が無く insert しかできないため、シェルや Zed が
   `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` を export していると打ち消せない。
   Anthropic の認証解決は `ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` → ログイン済み
   プロファイルの順でキーが在る限り先に勝つため、`scrub_anthropic_credentials()` が
   tokio ランタイムを起動する前(シングルスレッドな時点)にこの 2 変数を
   `std::env::remove_var` で実際に消している。

**逃げ道は設けていない。** release ビルドで `--acp` 自体を機能ごと落とす以上
（下記参照）、API キーで動かすユースケースは存在しない。

> ⚠️ Anthropic の Agent SDK ドキュメントには次の注意書きがある。
>
> > Unless previously approved, Anthropic does not allow third party developers to offer
> > claude.ai login or rate limits for **their products**, including agents built on the
> > Claude Agent SDK.
>
> これが、この機能を **debug ビルド限定**にした理由そのものである。「claude.ai ログインや
> レート枠を自分の製品の機能として第三者に提供する」ことへの制限であり、自分の執筆環境で
> 自分のサブスクリプションを使う分には当てはまらない（Claude Code 自身がそう動いている）が、
> `dist/` を他人に配布して使わせる段階になると該当する。API キー方式へ切り替えて配布する
> 選択肢は採らず、**release ビルドでは `--acp` 自体が起動しない**ようにしてこの問題を
> 構造的に回避している。

## CLAUDE.md を一切読ませない

コーディング向けの CLAUDE.md が執筆用エージェントに混ざると邪魔になるので、
**プロジェクトのものも `~/.claude/CLAUDE.md` も含めて全て無効化**している。

| 目的 | 実装 | 実際に `claude` へ渡るフラグ |
|---|---|---|
| システムプロンプト完全置換 | `SystemPrompt::String`（`data/system_chat.md`） | `--system-prompt <本文>` |
| CLAUDE.md / output styles / settings.json 全無効化 | `setting_sources` を**設定しない** | `--setting-sources ""` |

`--setting-sources` は "Comma-separated list of setting sources to load (user, project, local)"
なので、空文字は「どれも読まない」を意味する。この挙動はクレートの既定でもあり
（`src/transport/subprocess.rs`）、公式 TS/Python SDK の既定（`project` と `user` を
**有効化**）より厳しい。

実際に渡っている argv は `data/` のプロンプトを差し替えずに確認できる（後述の「動作確認」）。

## ツールと権限

エージェントに許可するツールは明示的に絞ってある（`writing_agent.rs` の `ALLOWED_TOOLS`）。

```
Read, Write, Edit, Glob, Grep, WebSearch, WebFetch
```

**`Bash` は許可しない。** 原稿ディレクトリで任意のコマンドを実行できる必要は無く、
許可範囲は狭いほどよい。characters.md / plot.md / memo/*.md の読み書きはファイルツールで足り、
調べ物は `WebSearch` / `WebFetch` で足りる。

権限モードは `acceptEdits`。ACP の権限要求フロー（`session/request_permission`）を
実装していないため、編集のたびに承認を求められると会話が進まなくなる。
`BypassPermissions` ではないので、許可していないツールが勝手に動くことはない。

## 1ターンの流れ

1. `session/new` で、そのワークスペース専用の `claude` プロセスを 1 つ起こす
   （会話の文脈は CLI 側が保持するので、こちらで履歴を組み直す必要は無い）
2. `session/prompt` を受けてエージェントへ中継し、応答テキストを届いた順に
   `AgentMessageChunk` としてクライアントへ流す
3. `PromptResponse(StopReason::EndTurn)` を返す
4. **応答を返したあと**、別セッションの一発問い合わせ（ツール不許可・`max_turns 1`）で
   直近 8 ターンを要約し、`chat_context.md` へ書き出す

要約を対話用クライアントと分けてあるので、要約中でも次のターンを受けられる。
`session/cancel` を受けたら `interrupt()` で進行中のターンを止める。

**切断時の取りこぼし対策**: 要約は応答を返したあとに走るため、直後に Zed が切断すると
書き終える前にランタイムごと落ちる。実行中の要約タスクは `JoinSet` で保持し、接続終了後に
最大 30 秒待ち合わせる（`DIGEST_DRAIN_TIMEOUT`）。

## Zed の設定

### エージェントの登録

`settings.json` の `agent_servers` に手で書く。**API キーの環境変数は要らない
（設定されていても `scrub_anthropic_credentials()` が無視する）。** `--acp` は debug ビルド
限定なので、パスは `target/debug/fifty_four_lsp` を指す（`--release` の成果物では動かない）。

```json
{
  "agent_servers": {
    "fifty-four": {
      "command": "/path/to/fifty_four/target/debug/fifty_four_lsp",
      "args": ["--acp"]
    }
  }
}
```

前提として `claude` CLI がインストールされ、ログイン済みであること
（`anthropic-agent-sdk` は `which claude` で探し、見つからなければ
`~/.npm-global/bin`, `/usr/local/bin`, `~/.local/bin` 等も探す）。

### 補完側の設定

要約をプロンプトへ埋め込む挙動は LSP 側の `initialization_options.chat_context` で調整する。

| キー | 既定値 | 内容 |
|---|---|---|
| `enabled` | `true` | `false` にすると `{{CHAT}}` は常に空になる |
| `max_chars` | `1200` | 埋め込む文字数の上限。超過分は**古い側**から捨てる |

以前は「最終更新から一定時間(TTL)を過ぎた要約は使わない」という時間ベースの
足切りがあったが、`chat_context.owner`(セッションID単位の所有者マーカー)に
置き換えて廃止した。昨日の会話が今日の補完に混ざる心配は、「別のセッションを
開いた/新しい会話を始めた時点で明示的にクリアされる」ことで解消している
(詳細は下の「セッションの再開」節)。

## プロンプト

| ファイル | 用途 |
|---|---|
| `data/system_chat.md` | 会話用システムプロンプト。Claude Code の既定を**置き換える**ので、役割・原稿ディレクトリの約束事（plot.md / キャラ設定 / memo/）・本文 `.txt` を勝手に編集しない旨まで全てここに書く |
| `data/system_chat_digest.md` | 要約用システムプロンプト |
| `data/prompt_chat_digest.md` | 要約の指示。`{{HISTORY}}` |

補完・code action 側のテンプレート（`prompt_completion*.md`、`prompt_fill_mark.md`、
`prompt_rephrase.md`）には `{{CHAT}}` を追加してある。見出しごと Rust 側
（`Backend::chat_digest`）で組み立てているので、要約が無いときは空文字に展開されるだけ。

なお `frontmatter::expand` は未知のプレースホルダをそのまま残す仕様なので、テンプレートに
`{{CHAT}}` を書いたのに変数を渡し忘れるとリテラルがプロンプトへ漏れる。`frontmatter.rs` に
食い違いを検出するテストがあるので、`{{CHAT}}` を使うテンプレートを増やすときは
そちらにも追加すること。

## 動作確認

### API 消費なしで argv とフローを確認する

`claude` の代役スクリプトを `PATH` の先頭に置けば、トークンを使わずに
「実際にどのフラグが渡っているか」と ACP の一連の流れを確認できる。

```bash
mkdir -p /tmp/stubbin
cat > /tmp/stubbin/claude <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > /tmp/claude-argv.txt
emit() {
  printf '{"type":"assistant","message":{"model":"stub","content":[{"type":"text","text":"%s"}]},"session_id":"s"}\n' "$1"
  printf '{"type":"result","subtype":"success","duration_ms":1,"duration_api_ms":1,"is_error":false,"num_turns":1,"session_id":"s"}\n'
}
printf '{"type":"system","subtype":"init","session_id":"s"}\n'
if printf '%s\n' "$@" | grep -qx -- "--input-format"; then
  while IFS= read -r line; do
    case "$line" in *'"type":"control_request"'*) continue ;; esac
    emit 'テスト応答'
  done
else
  cat > /dev/null; emit 'テスト応答'
fi
SH
chmod +x /tmp/stubbin/claude
PATH=/tmp/stubbin:$PATH fifty_four_lsp --acp   # 別端末から ACP を喋る
cat /tmp/claude-argv.txt                        # --setting-sources が空で渡っているか
```

`--setting-sources` の次の行が**空行**になっていれば、CLAUDE.md 類が締め出されている。

### 子プロセスの環境を確認する（API 消費ゼロ）

`ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` が `claude` CLI へ渡っていないことをスタブで
確認できる。上のスタブスクリプトの `printf '%s\n' "$@" > /tmp/claude-argv.txt` の下に
1 行足す:

```bash
printf '%s\n' "${ANTHROPIC_API_KEY-<unset>}" > /tmp/claude-env.txt
```

```bash
ANTHROPIC_API_KEY=dummy PATH=/tmp/stubbin:$PATH fifty_four_lsp --acp
cat /tmp/claude-env.txt   # <unset> になっていること(これが本丸)
```

`.env` に `ANTHROPIC_API_KEY` を置いた場合も同様に `<unset>` のままであることを確認する
（`--acp` は `.env` を読まないため）。

### Zed 上での確認

Agent Panel で数ターン会話したあと `<workspace>/.fifty_four/chat_context.md` が生成される
ことを見て、同じワークスペースの `.txt` で補完を出し、`RUST_LOG=fifty_four_lsp=debug` の
ログでプロンプトに要約が入っていることを確かめる。

```
acp session/new: id=... cwd=...
acp session/prompt: id=... N chars
acp turn finished: id=... turns=N root=...
chat digest updated (N chars)
```

`RUST_LOG=debug` にすると ACP ライブラリのメッセージダンプが大量に混ざるので、
`fifty_four_lsp=debug` に絞るとよい。

**`RUST_LOG` を渡す手段が無い場合(Zed から `agent_servers` 経由で起動している等)。**
Zed はこのバイナリを直接起動するため、ターミナルから環境変数を渡す方法が事実上無い。
そのため `--acp` 起動時、`RUST_LOG` が未設定なら ACP 関連モジュール
(`acp`/`acp_config`/`writing_agent`/`session_log`)に限って自動的に debug 相当になる
(`lsp/src/main.rs` の `default_acp_log_level()`)。上記のようなログは特に何も設定しなくても
Zed のログ(`~/.local/share/zed/logs/Zed.log` 等、stderr を拾う経路)に出るはず。
明示的に別のレベルを見たい場合は `agent_servers` の設定に `env` で `RUST_LOG` を書けば
そちらが優先される:

```json
{
  "agent_servers": {
    "fifty-four": {
      "command": "/path/to/fifty_four/target/debug/fifty_four_lsp",
      "args": ["--acp"],
      "env": { "RUST_LOG": "fifty_four_lsp=trace" }
    }
  }
}
```

**クラッシュ(panic)した場合。** stderr にpanicメッセージが流れるのに加えて、
`<実行ファイルの隣>/logs/acp_panic.log` へタイムスタンプ・panic内容・バックトレースが
追記される(`lsp/src/main.rs` の `install_acp_panic_hook()`)。Zed がstderrを拾わない・
拾っても見つけにくい場合はこのファイルを確認する。TTLや自動削除は無いので、
古いエントリが溜まっていたら手動で消してよい。

## モデル・思考レベルの変更

Agent Panel にモデルと思考レベル(effort)のセレクタが出る。中身は ACP の
`SessionConfigOption`(`category: model` / `thoughtLevel`)で、`session/new` /
`session/load` のレスポンスに載せている(`lsp/src/acp_config.rs`)。

**選択の反映は次のターンの頭から。** `anthropic-agent-sdk` はセッション途中の
モデル/thinking切替を非対応(`set_model()` はローカルに保存するだけで `claude` CLI
へは届かない)なので、`session/set_config_option` では設定を保留するだけに留め、
次に `session/prompt` を受けたタイミングで同じセッションID(UUID)を使って
`--resume` し、`claude` プロセスを起こし直している(「セッションの再開」節と
同じ経路)。会話の文脈は CLI 側の永続化に残っているため引き継がれるが、
プロセスの起こし直しで1〜2秒ほど待ち時間が入る。

モデル候補は `default`(既定・CLI任せ)/ `opus` / `sonnet` / `haiku` / `fable` の
エイリアスに加え、`anthropic-agent-sdk` の `supported_models()` が返す
バージョン固定IDを並べている。エイリアスは `claude` CLI 側で常に最新の
バージョンへ解決されるため、`supported_models()` の静的リストが古くなっても
実用上は困らない。

思考レベルは `off` / `low` / `medium` / `high` / `max` の5段階(既定=CLI任せを含め6つ)
で、`--max-thinking-tokens` の値へ写す。

### サブスク枠の残量メーター

`claude` CLI が流す `rate_limit_event` から使用率を拾い、ACP の
`SessionUpdate::UsageUpdate` として Zed へ通知している(`lsp/src/acp.rs`)。

> ⚠️ `UsageUpdate` は ACP の仕様上「コンテキストウィンドウの使用量」用のフィールド
> であり、ACP には**サブスク枠(5時間枠)の残量に対応するフィールドが無い**。
> ここでは意図的にラベルと中身をずらし、`used`/`size` にサブスク枠の使用率
> (0〜100)を流用している。Zed のパネルには「コンテキスト使用量」として
> 描画されるが、実際に示しているのは枠の消費具合である。
>
> `rate_limit_event` の JSON 形式は SDK/ACP どちらにも型定義が無いため、
> `writing_agent.rs` の `parse_rate_limit_event()` で緩く読んでいる。想定と違う
> 形で来た場合は黙って何もしない(枠の表示が出ないだけで会話は成立する)。

## セッションの再開（`session/load`）

Zed はプロセス再起動後などに ACP エージェントへ再接続する際、以前渡された `sessionId` で
`session/load` を送ってくる。これに対応していないと「Loading or resuming sessions is not
supported by this agent.」というエラーになる。

対応方法: `session/new` で採番するセッションIDに、こちらで生成した乱数の文字列ではなく
**UUID v4 をそのまま使い**、`claude` CLI 自身のセッションID(`--session-id`)として渡す。
`claude` CLI は会話履歴を自分でディスクへ永続化しているので、`session/load` が来たら
同じIDで `--resume` するだけで、対話の文脈(会話履歴)を CLI 側から引き継げる。
こちらのプロセス内でIDのマッピングを別途持つ必要はない。

**過去のメッセージは Agent Panel に再表示される。** ACP の仕様は `session/load` について
「応答を返す前に、会話全体を `session/update` 通知としてリプレイしなければならない
(MUST)」と定めている。`Session::turns`(要約用の直近ターン保持)はプロセスのメモリ上
にしかなくプロセス再起動で失われるため、[`crate::session_log`] が1ターンごとに
`<workspace>/.fifty_four/sessions/<sessionId>.jsonl.gz` へ逐次追記して残しておき、
`session/load` の応答前にそこから読み戻して `UserMessageChunk`/`AgentMessageChunk`
としてリプレイする(`lsp/src/session_log.rs`)。件数の上限は設けていない(ローカルの
テキスト再送でありLLM呼び出しコストが無いため)。

保存ファイルは1ターン=1行のJSONを gzip の「独立したメンバーを連結してよい」性質を
使って追記していく(`flate2` の `write::GzEncoder`/`read::MultiGzDecoder`)。
`chat_context.md` と同じ `.fifty_four/` 配下に増えるので、`.gitignore` へ
`.fifty_four/` を足す既存の勧めがこのディレクトリもカバーする。**保持期間(TTL)による
自動削除は無く、ログは増え続ける。** 気になる場合は手動で
`.fifty_four/sessions/` を削除してよい(次回以降そのセッションの再開時に
過去メッセージが再表示されなくなるだけで、会話自体は `--resume` で継続できる)。

同じ理由(メモリ上にしか無い)で、モデル/思考レベルの選択(前節参照)は
`session/load` では既定(CLI任せ)へ戻る。

**要約(`chat_context.md`)もセッション単位で明示的に切り替わる。** `chat_context.md`
はワークスペースに1ファイルしか無いため、`chat_context.owner`(中身はセッションID
1行)へ「いまその内容を所有しているセッション」を記録している(`lsp/src/chat_context.rs`)。

- `session/new`(新しい会話)を受けると、前の会話の要約を引き継がないよう
  `chat_context.md`/`chat_context.owner` を両方クリアする。
- `session/load` で所有者が**復元先セッションと一致**していれば、直前まで
  使っていたスレッドを開き直しただけなので何もしない(要約は既に正しい内容の
  ままなので、再生成のLLM呼び出しは発生しない)。
- `session/load` で所有者が**別セッション**(間に別の会話をしていた等)なら、
  `.fifty_four/sessions/` から読み戻した過去ターンをもとに要約をバックグラウンドで
  再生成し、所有者を復元先セッションへ書き換える(`session/prompt` の応答後と
  同じ「応答を先に返し、要約は追いつかせる」方針)。過去ターンが無ければ
  再生成はせず、古い所有者の要約を残さないようクリアするだけにする。

**旧形式IDのセッションは再開できない。** セッションIDをUUID v4化する前
(このドキュメントのこの節が書かれる前)に作られたセッションは、Zed側の履歴に
`ff-{pid}-{n}` 形式のIDで残っている。`claude` CLI の `--resume`/`--session-id` は
UUID形式しか受け付けないため、こうした旧IDでの `session/load` は
`session/load` の時点で明示的にエラーを返す(`lsp/src/acp.rs` のUUID形式検証)。
この場合はZed上で手動で新しい会話を開始すること。

**UUID形式でも、`claude` CLI 側に実体が無いセッションは再開できない。** `session/load`
はUUID形式の検証はするが、そのセッションが `claude` CLI 側に実在するかは
接続時点では確認していない(`ClaudeSDKClient::new()` は `--resume` 先が無くても
接続自体は成功してしまう)。実際の失敗は最初の `session/prompt` で
`Message::Result{is_error:true}` として返ってきて初めて分かる。`claude` CLI 自身は
このとき stderr に `No conversation found with session ID: ...` を出す
(Zedの `agent stderr:` ログに出る。こちらのプロセスからは見えないログなので
`RUST_LOG` を上げても出てこない)。この場合、応答は
「セッションの再開に失敗しました。新しい会話を開始してください。」という
分かりやすい文言になる(`--resume` 起動時のみこの文言に寄せる。新規セッションでの
同種のエラーは実際のAPIエラー等の可能性があるため詳細をそのまま返す。
`lsp/src/writing_agent.rs` の `ClaudeAgent::result_error`)。詳細(`errors`/`result`)は
`WARN` ログにだけ残る。

## スコープ外

- **トークン単位のストリーミング**。`anthropic-agent-sdk` は assistant メッセージ単位で
  届くので、`AgentMessageChunk` もその粒度になる。
- ACP の権限要求フロー。
- 拡張からの自動登録（Zed の API に存在しないため不可能）。
