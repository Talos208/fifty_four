# ACP エージェント

`fifty_four_lsp --acp` で起動する ACP (Agent Client Protocol) エージェント。Zed の
Agent Panel から作者の相談相手として応答し、その会話の要約を **LSP の短文生成の
コンテキストとして渡す**。

> **debug ビルド限定の機能である。** LLM アクセスに作者自身の `claude` CLI のログイン
> （＝サブスクリプション枠）をそのまま使うため、配布物に載せて第三者へ提供することは
> Anthropic の規約上できない。release ビルドではモジュールごと落としてあり、`--acp` を
> 渡すとメッセージを1行出して終了コード 1 で終わる（後述「認証」）。

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

バイナリは 1 つで、`--acp` の有無でモードが変わる。配布物の構成は変わらないため
`cargo prepare package` に変更は要らない。ただし ACP エージェントが入るのは debug
ビルドだけなので、`cargo prepare package`（`--release` 無し）で作った `dist/` では動き、
`cargo prepare package --release` で作ったものでは動かない。

コード上は `lsp/src/main.rs` で `acp` と `writing_agent` の 2 モジュールを
`#[cfg(debug_assertions)]` で囲っている。`chat_context` は囲っていない — 書き手
（ACP）は debug 限定だが、読み手（補完）は release でも動く必要があるためで、
release バイナリは「要約を書く者が居ないので常に要約なし」として素通りする。

## 依存している SDK について

`anthropic-agent-sdk` (MIT) は **Anthropic 公式ではなく第三者製**である。公式の
Claude Agent SDK は Python と TypeScript のみで、他言語には「`claude` CLI を
サブプロセスとして駆動せよ」と案内されている。

ただし**公式 SDK も中身は同じで `claude` CLI のラッパー**なので、このクレートを使うことは
アーキテクチャ上の妥協ではなく、CLI 駆動を型付きで書くための省力化にすぎない。壊れたときに
自前実装へ移れるよう、クレートへの依存は `lsp/src/writing_agent.rs` の
`WritingAgent` トレイトの内側だけに閉じ込めてある。`acp.rs` はトレイト越しにしか触らない。

## 認証 — サブスクリプション枠

LLM アクセスは `claude` CLI のログイン認証をそのまま使う。**API キーは使わないし、
設定されていても無視する。**

### なぜ debug 限定なのか

Anthropic の Agent SDK ドキュメントには次の注意書きがある。

> Unless previously approved, Anthropic does not allow third party developers to offer
> claude.ai login or rate limits for **their products**, including agents built on the
> Claude Agent SDK.

「claude.ai ログインやレート枠を**自分の製品の機能として第三者に提供する**」ことへの制限で、
自分の執筆環境で自分のサブスクリプションを使う分には当てはまらない（Claude Code 自身が
そう動いている）。一方 `dist/` を他人に配って使わせる段階では該当してしまう。

そこで**この機能を debug ビルド限定にした**。「配布時は API キーへ切り替える」という運用の
約束ではなく、release バイナリに機能が存在しないという形で担保する。

### API 資格情報をプロセス環境から取り除く

以前は「`ANTHROPIC_API_KEY` を設定しなければサブスク枠で動く」とだけ書いていたが、
**それだけでは API クレジット課金になってしまう経路が 2 つあった**（実際に
"Credit balance is too low" が出た）。

| 経路 | 内容 |
|---|---|
| `.env` の自動読み込み | debug ビルドはリポジトリ直下の `.env` を `dotenvx` で読む。そこには `llm.rs` 用の provider キー（`ANTHROPIC_API_KEY` ほか）が入っている |
| 親プロセスの環境の継承 | `anthropic-agent-sdk` は `env::vars()` を丸ごと集めて `Command::envs` に渡すため、シェルや Zed が export したキーが `claude` CLI まで届く |

いまはコード側（`lsp/src/main.rs`）で両方を塞いでいる。

1. **`--acp` では `.env` を読まない。** ACP 経路は provider の API キーを 1 つも必要としない
   （LLM アクセスは `claude` CLI 経由）。`RUST_LOG` を `.env` に書いている場合は
   `--acp` では効かないので、実環境変数で渡すこと。
2. **`ANTHROPIC_API_KEY` と `ANTHROPIC_AUTH_TOKEN` をプロセス環境から削除する**
   （`scrub_anthropic_credentials`）。削除したことは stderr に 1 行出るので、
   「なぜ自分のキーが効かないのか」を追える。

`ClaudeAgentOptions::env` では代用できない。insert しかできず、空文字を入れても
「空のキーで認証」になるだけである。Anthropic の認証解決は
`ANTHROPIC_API_KEY` → `ANTHROPIC_AUTH_TOKEN` → ログイン済みプロファイルの順で、
**キーが在る限り先に勝つ**ため、実際に消すしかない。

削除するのは認証に使われるこの 2 つだけで、`ANTHROPIC_BASE_URL` 等には触らない。

> `std::env::remove_var` は edition 2024 で `unsafe`（他スレッドが環境を読んでいると UB）。
> そのため `main` は素の `fn` にして、Tokio ランタイムを起こす前のシングルスレッドな時点で
> 環境を整えてから `async_main` へ入る構造にしてある。

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

`settings.json` の `agent_servers` に手で書く。**API キーの環境変数は要らない**
（設定してあっても無視される）。**debug ビルドのパスを指すこと** — release バイナリを
指すと起動直後に終了する。

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
| `ttl_secs` | `43200`（12時間） | 最終更新がこれより古い要約は使わない |

`ttl_secs` があるのは、昨日の会話が今日の補完に混ざるのを防ぐため。

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

### API 消費なしで argv・認証情報・フローを確認する

`claude` の代役スクリプトを `PATH` の先頭に置けば、トークンを使わずに
「実際にどのフラグと環境変数が渡っているか」と ACP の一連の流れを確認できる。

```bash
mkdir -p /tmp/stubbin
cat > /tmp/stubbin/claude <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > /tmp/claude-argv.txt
# 親から受け継いだ Anthropic 認証情報を記録する
printf '%s\n' "${ANTHROPIC_API_KEY-<unset>}" > /tmp/claude-env.txt
printf '%s\n' "${ANTHROPIC_AUTH_TOKEN-<unset>}" >> /tmp/claude-env.txt
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

# わざとキーを設定して起動する(別端末から ACP を喋る)
ANTHROPIC_API_KEY=dummy-key ANTHROPIC_AUTH_TOKEN=dummy-token \
  PATH=/tmp/stubbin:$PATH fifty_four_lsp --acp

cat /tmp/claude-argv.txt   # --setting-sources が空で渡っているか
cat /tmp/claude-env.txt    # 2行とも <unset> になっているか
```

- `--setting-sources` の次の行が**空行**になっていれば、CLAUDE.md 類が締め出されている。
- `/tmp/claude-env.txt` が `<unset>` 2 行なら、API 資格情報が子プロセスへ漏れていない。
  親側の stderr にも `--acp: ANTHROPIC_API_KEY を無視します(...)` が出る。

### release ビルドで無効化されていることの確認

```bash
cargo build --release
./target/release/fifty_four_lsp --acp; echo "exit=$?"
# fifty_four_lsp: --acp は debug ビルド限定です(...)
# exit=1
```

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

## スコープ外

- **トークン単位のストリーミング**。`anthropic-agent-sdk` は assistant メッセージ単位で
  届くので、`AgentMessageChunk` もその粒度になる。
- `session/load` によるセッション復元、ACP の権限要求フロー。
- 拡張からの自動登録（Zed の API に存在しないため不可能）。
