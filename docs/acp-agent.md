# ACP プロキシ

`fifty_four_lsp --acp` で起動する ACP (Agent Client Protocol) プロキシ。Zed と、作者が
普段使っている ACP エージェント（Claude Code、Gemini CLI など）の**あいだに挟まり**、
会話を素通しさせながら覗き見て、**LSP の短文生成のコンテキストとして渡す**。

## なぜ必要か

LSP の補完が見ているのは、カーソル直前の 10 文（`cursor_context::before_sentences_upto`）と、
必要に応じてツールで引くプロット・キャラクター設定だけである。「この場面はこう書きたい」
「この人物の口調は変えたい」といった、**まだ本文に書かれていない作者の意図**を渡す経路が無かった。

チャットの応答そのものは上流エージェントが作る。このプロセスは応答を生成しないので、
作者は普段どおりツール実行やファイル閲覧のできるエージェントと話しながら、その内容が
自動的に補完へ効く、という形になる。

## 構成

```
Zed ──stdio──> fifty_four_lsp --acp
                 └─ ConductorImpl
                      ├─ FiftyFourProxy   ← 会話を覗いて要約を書く
                      └─ AcpAgent         ← 上流エージェントのプロセスを起動
                           │
                           └── <workspace>/.fifty_four/chat_context.md
                                   ↑ LSP サーバ（別プロセス）が補完時に読む
```

### なぜ conductor が要るのか

プロキシは上流エージェントへ**直結できない**。プロキシが agent 方向へ送るメッセージは
`SuccessorMessage`（`_proxy/successor`）エンベロープに包まれ、素のエージェントはそれを
解釈できないため。包み・解きは conductor の役目なので、`ConductorImpl` をライブラリとして
このプロセスに埋め込んでいる。Zed から見れば `agent_servers` エントリは 1 つのままで、
外部の conductor バイナリを別途動かす必要は無い。

### なぜファイル渡しなのか

Zed は LSP サーバ（拡張経由）と ACP プロキシ（`agent_servers` 経由）を**別プロセス**として
起動する。Zed の拡張 API には ACP を登録する口が無い（language server / MCP context server /
DAP のみ）ため、この分離は避けられない。

受け渡しは `<workspace>/.fifty_four/chat_context.md` の 1 ファイル。プロセス間の同期機構は
持ち込まず、`plot.md` やキャラクター設定を毎回ディスクから読み直す `tools.rs` と同じ方式に
している。書き込みは一時ファイル + `rename` で原子的に行うので、読み手が書きかけを読むことは
ない（`lsp/src/chat_context.rs`）。

> `.fifty_four/` は原稿を置いているワークスペース側に作られる。原稿を git 管理しているなら、
> そちらの `.gitignore` に `.fifty_four/` を足しておくとよい。

バイナリは 1 つで、`--acp` の有無でモードが変わる（`lsp/src/main.rs`）。配布物の構成は
従来と変わらないため、`cargo prepare package` に変更は要らない。

## Zed の設定

### プロキシの登録

`settings.json`（`agent_servers`）に手で書く。

```json
{
  "agent_servers": {
    "fifty-four": {
      "command": "/path/to/fifty_four_lsp",
      "args": ["--acp"],
      "env": {
        "FIFTY_FOUR_ACP_AGENT": "npx -y @agentclientprotocol/claude-agent-acp@latest",
        "FIFTY_FOUR_LLM_CONFIG": "{\"deferred\":{\"provider\":\"google\",\"model\":\"gemini-3.1-flash-preview\"}}",
        "GEMINI_API_KEY": "..."
      }
    }
  }
}
```

`agent_servers` のエントリには `command` / `args` / `env` しか無く、LSP のように
`initialization_options` を受け取れない。そのため設定は環境変数で渡す。

| 変数 | 内容 |
|---|---|
| `FIFTY_FOUR_ACP_AGENT` | 中継先の ACP エージェント。コマンド文字列か、`{"type":"stdio","command":...}` 形式の JSON。`--agent <command>` でも指定できる |
| `FIFTY_FOUR_LLM_CONFIG` | **要約用**の LLM 設定。形式は LSP の `initialization_options.llm` と同じ。`--llm-config <path>` で JSON ファイル指定も可 |

このプロセスが LLM を呼ぶのは**要約のときだけ**なので、`FIFTY_FOUR_LLM_CONFIG` は
`deferred` を優先して見る（無ければ `ondemand`、それも無ければ旧形式のフラット指定）。
チャット応答は上流エージェントが作るため、高価なモデルを指定する必要は無い。

API キーは従来どおり `genai` がプロセス環境変数から読むので、この JSON には含めない。
上流エージェント・LLM 設定のどちらかが無い、または `provider` が不正な場合は、起動時に
stderr へ理由を出して終了する（終了コード 1）。

### 補完側の設定

要約をプロンプトへ埋め込む挙動は LSP 側の `initialization_options.chat_context` で調整する。

| キー | 既定値 | 内容 |
|---|---|---|
| `enabled` | `true` | `false` にすると `{{CHAT}}` は常に空になる |
| `max_chars` | `1200` | 埋め込む文字数の上限。超過分は**古い側**から捨てる |
| `ttl_secs` | `43200`（12時間） | 最終更新がこれより古い要約は使わない |

`ttl_secs` があるのは、昨日の会話が今日の補完に混ざるのを防ぐため。

## 実装

### 何を覗いて、何を素通しするか

プロキシは**未処理のメッセージを既定で素通しする**ので、扱うのは次の 3 つだけ。
それ以外（`session/cancel`、権限要求、ファイル読み書き、ツール呼び出し等）は
一切触らずに流れる。

| メッセージ | 向き | 動作 |
|---|---|---|
| `session/new` | Client → Agent | `cwd` を控えて転送。上流が採番した `SessionId` と結び付ける |
| `session/prompt` | Client → Agent | 作者の発話を控えて転送。応答が返った時点で 1 ターン確定 → 要約を投げる |
| `session/update` | Agent → Client | `AgentMessageChunk` のテキストを積むだけ。`Handled::No` を返して既定の転送処理へそのまま流す（書き換えない） |

`session/new` と `session/prompt` は `forward_cancellation_from` + `on_receiving_result` で
転送する。`forward_response_to` だと応答を覗けないが、キャンセルの転送は自前で引き継ぐ
必要があるため、この 2 つを組み合わせている。

### 1ターンの流れ

1. `session/prompt` を受けて作者の発話を履歴へ積み、そのまま上流へ転送する
2. 上流から流れてくる `AgentMessageChunk` を `pending_reply` へ連結しつつ、クライアントへ素通しする
3. `PromptResponse` が返ったら `pending_reply` を 1 ターンとして確定する
4. **クライアントへ応答を返したあと**、バックグラウンドタスクで直近 8 ターンを
   `prompt_chat_digest.md` で要約し、`chat_context.md` へ書き出す

要約が作者を待たせないよう、応答の中継が先。要約に失敗してもログに残るだけで、会話も
補完も止まらない（`{{CHAT}}` が空になるだけ）。ツール実行だけで終わって応答本文が無い
ターンでも、作者の発話は履歴に残る。

### プロンプト

| ファイル | 用途 |
|---|---|
| `data/prompt_chat_digest.md` | 要約生成。`{{HISTORY}}` |

補完・code action 側のテンプレート（`prompt_completion*.md`、`prompt_fill_mark.md`、
`prompt_rephrase.md`）には `{{CHAT}}` を追加してある。見出しごと Rust 側
（`Backend::chat_digest`）で組み立てているので、要約が無いときは空文字に展開されるだけで
見出しは残らない。

なお `frontmatter::expand` は未知のプレースホルダをそのまま残す仕様なので、テンプレートに
`{{CHAT}}` を書いたのに変数を渡し忘れると、リテラル `{{CHAT}}` がプロンプトへ漏れる。
`frontmatter.rs` にこの食い違いを検出するテストを置いてあるので、`{{CHAT}}` を使う
テンプレートを増やすときはそちらにも追加すること。

## 動作確認

上流エージェントを実際に立てずに疎通だけ見たい場合は、`initialize` にだけ応答する
ダミーエージェント（`agent-client-protocol` クレートの `examples/simple_agent.rs` 相当）を
`FIFTY_FOUR_ACP_AGENT` に指定すればよい。

Zed 上での確認は、Agent Panel で数ターン会話したあと
`<workspace>/.fifty_four/chat_context.md` が生成されることを見て、同じワークスペースの
`.txt` で補完を出し、`RUST_LOG=debug` のログでプロンプトに要約が入っていることを確かめる。

`RUST_LOG=fifty_four_lsp=debug` にすると、プロキシ自身のログだけが出る（`RUST_LOG=debug`
だと ACP ライブラリのメッセージダンプが大量に混ざる）。観測が効いているかは次のログで分かる。

```
acp session/new: id=... cwd=...
acp session/prompt: id=... N chars
acp turn finished: id=... turns=N root=...
chat digest updated (N chars)
```
