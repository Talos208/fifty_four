# ACP エージェント

`fifty_four_lsp --acp` で起動する ACP (Agent Client Protocol) エージェント。Zed の
Agent Panel から作者の相談相手として応答し、その会話の要約を **LSP の短文生成の
コンテキストとして渡す**。

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
`cargo prepare package` に変更は要らない。

## 依存している SDK について

`anthropic-agent-sdk` (MIT) は **Anthropic 公式ではなく第三者製**である。公式の
Claude Agent SDK は Python と TypeScript のみで、他言語には「`claude` CLI を
サブプロセスとして駆動せよ」と案内されている。

ただし**公式 SDK も中身は同じで `claude` CLI のラッパー**なので、このクレートを使うことは
アーキテクチャ上の妥協ではなく、CLI 駆動を型付きで書くための省力化にすぎない。壊れたときに
自前実装へ移れるよう、クレートへの依存は `lsp/src/writing_agent.rs` の
`WritingAgent` トレイトの内側だけに閉じ込めてある。`acp.rs` はトレイト越しにしか触らない。

## 認証 — サブスクリプション枠

LLM アクセスは `claude` CLI の認証をそのまま使う。`ANTHROPIC_API_KEY` を設定しなければ、
**ログイン済み CLI のサブスクリプション枠**で動く。このエージェントは API キーを要求しないし、
自前で保持もしない。

> ⚠️ Anthropic の Agent SDK ドキュメントには次の注意書きがある。
>
> > Unless previously approved, Anthropic does not allow third party developers to offer
> > claude.ai login or rate limits for **their products**, including agents built on the
> > Claude Agent SDK.
>
> これは「claude.ai ログインやレート枠を**自分の製品の機能として第三者に提供する**」ことへの
> 制限で、自分の執筆環境で自分のサブスクリプションを使う分には当てはまらない（Claude Code
> 自身がそう動いている）。ただし `dist/` を他人に配布して使わせる段階になると該当するので、
> そのときは API キー方式へ切り替えること。

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

`settings.json` の `agent_servers` に手で書く。**API キーの環境変数は要らない。**

```json
{
  "agent_servers": {
    "fifty-four": {
      "command": "/path/to/fifty_four_lsp",
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
