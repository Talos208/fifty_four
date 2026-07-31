# ACP エージェント

`fifty_four_lsp --acp` で起動する ACP (Agent Client Protocol) エージェント。Zed の
Agent Panel から作者とチャットし、その内容を **LSP の短文生成のコンテキストとして渡す**。

## なぜ必要か

LSP の補完が見ているのは、カーソル直前の 10 文（`cursor_context::before_sentences_upto`）と、
必要に応じてツールで引くプロット・キャラクター設定だけである。「この場面はこう書きたい」
「この人物の口調は変えたい」といった、**まだ本文に書かれていない作者の意図**を渡す経路が無かった。

ACP エージェントはその経路になる。作者との会話を 1 ターンごとに要約し、補完・code action の
プロンプトへ `{{CHAT}}` として埋め込む。

## 構成

Zed は LSP サーバと ACP エージェントを**別プロセス**として起動する。Zed の拡張 API には
ACP エージェントを登録する口が無い（language server / MCP context server / DAP のみ）ため、
エージェントの登録はユーザの `settings.json` で行う。

```
Zed
 ├─ (拡張経由)          fifty_four_lsp            ← LSP。補完時に要約を読む
 └─ (agent_servers 経由) fifty_four_lsp --acp     ← ACP。チャットを中継し要約を書く
                          │
                          └── <workspace>/.fifty_four/chat_context.md
```

バイナリは 1 つで、`--acp` の有無でモードが変わる（`lsp/src/main.rs`）。配布物の構成は
従来と変わらないため、`cargo prepare package` に変更は要らない。

受け渡しは `<workspace>/.fifty_four/chat_context.md` の 1 ファイル。プロセス間の同期機構は
持ち込まず、`plot.md` やキャラクター設定を毎回ディスクから読み直す `tools.rs` と同じ方式にしている。
書き込みは一時ファイル + `rename` で原子的に行うので、読み手が書きかけを読むことはない
（`lsp/src/chat_context.rs`）。

> `.fifty_four/` は原稿を置いているワークスペース側に作られる。原稿を git 管理しているなら、
> そちらの `.gitignore` に `.fifty_four/` を足しておくとよい。

## Zed の設定

### エージェントの登録

`settings.json`（`agent_servers`）に手で書く。

```json
{
  "agent_servers": {
    "fifty-four": {
      "command": "/path/to/fifty_four_lsp",
      "args": ["--acp"],
      "env": {
        "FIFTY_FOUR_LLM_CONFIG": "{\"ondemand\":{\"provider\":\"google\",\"model\":\"gemini-3.1-pro-preview\"},\"deferred\":{\"provider\":\"google\",\"model\":\"gemini-3.1-flash-preview\"}}",
        "GEMINI_API_KEY": "..."
      }
    }
  }
}
```

`agent_servers` のエントリには `command` / `args` / `env` しか無く、LSP のように
`initialization_options` を受け取れない。そのため LLM 設定は環境変数
`FIFTY_FOUR_LLM_CONFIG` で渡す。形式は LSP の `initialization_options.llm` と同じ。

- `ondemand` — チャット応答の生成に使う
- `deferred` — 会話要約の生成に使う。省略すると `ondemand` へフォールバックする（警告が出る）

環境変数の代わりに `--llm-config <path>` で JSON ファイルを指定してもよい。API キーは
従来どおり `genai` がプロセス環境変数から読むので、この JSON には含めない。

設定が無い・`provider` が不正な場合は起動時に stderr へ理由を出して終了する（終了コード 1）。

### 補完側の設定

要約をプロンプトへ埋め込む挙動は LSP 側の `initialization_options.chat_context` で調整する。

| キー | 既定値 | 内容 |
|---|---|---|
| `enabled` | `true` | `false` にすると `{{CHAT}}` は常に空になる |
| `max_chars` | `1200` | 埋め込む文字数の上限。超過分は**古い側**から捨てる |
| `ttl_secs` | `43200`（12時間） | 最終更新がこれより古い要約は使わない |

`ttl_secs` があるのは、昨日の会話が今日の補完に混ざるのを防ぐため。

## 実装

### 対応しているメソッド

| メソッド | 動作 |
|---|---|
| `initialize` | `AgentCapabilities` を返す。テキストの送受信は baseline なので追加宣言は無し |
| `session/new` | `cwd` をワークスペースルートとして保存し、`SessionId` を採番する |
| `session/prompt` | 発話を履歴へ積み、LLM へ中継し、応答を返したあと要約を更新する |

未処理のリクエストは Method not found、未処理の通知（`session/cancel` 等）は無視、が
ライブラリ側の既定動作なのでハンドラは置いていない。

### 1ターンの流れ

1. `PromptRequest` からテキストブロックを取り出して連結する
2. 履歴へ積み、直近 12 ターンを `prompt_chat.md` の `{{HISTORY}}` へレンダリングする
3. `llm.ondemand` で応答を生成し、`AgentMessageChunk` として送る
4. `PromptResponse(StopReason::EndTurn)` を返す
5. **応答を返したあと**、バックグラウンドタスクで `prompt_chat_digest.md` を使って
   直近 8 ターンを要約し、`chat_context.md` へ書き出す

要約が応答をブロックしないよう、LLM のスロットは応答用と要約用で分けてある。要約に失敗しても
ログに残るだけで、会話も補完も止まらない（`{{CHAT}}` が空になるだけ）。

### 既存 API の制約

- **`LlmInterface` にマルチターン会話の概念が無い。** `LlmClient::with_model` は毎回
  `sys_prompt` + キャッシュ + その回のプロンプトから `ChatRequest` を組み直す（会話履歴が
  残るのはツール呼び出しループの中だけ）。そのため会話履歴はテンプレートへ文字列として
  レンダリングして渡している。
- **`chat()` はストリーミングを公開していない。** 完成した `String` を返すだけなので、
  1ターンにつき 1 チャンクを送る。トークン単位のストリーミングには `llm.rs` 側の対応が要る。

### プロンプト

| ファイル | 用途 |
|---|---|
| `data/system_chat.md` | チャット用 system prompt。`system.md` が「人間との対話ではない」と宣言しているので別立てにしている |
| `data/prompt_chat.md` | 応答生成。`{{HISTORY}}` / `{{MESSAGE}}` |
| `data/prompt_chat_digest.md` | 要約生成。`{{HISTORY}}` |

補完・code action 側のテンプレート（`prompt_completion*.md`、`prompt_fill_mark.md`、
`prompt_rephrase.md`）には `{{CHAT}}` を追加してある。見出しごと Rust 側で組み立てているので、
要約が無いときは空文字に展開されるだけで見出しは残らない。

なお `frontmatter::expand` は未知のプレースホルダをそのまま残す仕様なので、テンプレートに
`{{CHAT}}` を書いたのに変数を渡し忘れると、リテラル `{{CHAT}}` がプロンプトへ漏れる。
`{{CHAT}}` を使うテンプレートを増やすときは、渡す側（`backend.rs` の `completion` /
`code_action`）も必ず対応させること。

## 動作確認

ネットワーク無しで疎通だけ見る場合:

```bash
printf '%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/abs/path","mcpServers":[]}}' \
  | FIFTY_FOUR_LLM_CONFIG='{"ondemand":{"provider":"google"}}' fifty_four_lsp --acp
```

ACP は改行区切りの JSON-RPC（LSP と違い `Content-Length` ヘッダは無い）。

Zed 上での確認は、Agent Panel で数ターン会話したあと `<workspace>/.fifty_four/chat_context.md`
が生成されることを見て、同じワークスペースの `.txt` で補完を出し、`RUST_LOG=debug` のログで
プロンプトに要約が入っていることを確かめる。
