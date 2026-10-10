# shinasadame プロンプト試行機能 設計仕様

**日付:** 2026-10-09

## 概要

shinasadame のレコード詳細画面に、記録されたプロンプトを編集して LLM に試しに投げ、元の応答と見比べる機能を追加する。
LLM は切り替えられ、fifty_four(LSP)が対応するすべての provider を対象にする。

目的は、プロンプト改善を手動で試す場を作ること。DSPy による自動最適化(次のマイルストーン)の前段にあたる。

### 前提・制約

- shinasadame は「FlightRecorder の DB を読むだけ」のアプリだった(`2026-10-06-shinasadame-design.md`)。本機能は shinasadame が外部 LLM を呼ぶ最初の機能である(DB には書かない)。
- LLM の provider ごとの差異(reasoning、構造化出力、未対応パラメータの自動除去、LM Studio の能力判定など)は `lsp/src/llm.rs` が吸収している。Python 側で再実装せず、**LSP の Rust コードを再利用**する。
- shinasadame は Docker Desktop の kind 上で動き、ホストのワークスペース(`characters.md`、`plot.md`)は見えない。
- 公開先はローカルのみ(既存の前提のまま)。

### スコープ

含む:
- `prompt` 列を持つテーブルの詳細画面: `completions` / `code_actions` / `character_updates` / `quality_reviews`
- プロンプトの編集と再実行、LLM(provider/model)の切り替え、元の応答との並列表示
- LSP バイナリの常駐 `--replay` モードと、shinasadame 側のクライアント層

含まない(後続の課題):
- **tool(`character_info` / `plot_info`)の再現**。元の実行は tool を使っているため、本機能の再実行は元と条件が異なる。画面に「tool なしで実行」と明記する。扱い方は未決(候補: 実行時の tool 結果を LSP が記録して再生 / 画面で貼り付け)
- 試行結果の保存(画面上の一時表示のみ。ページを閉じると消える)
- DSPy による評価・最適化
- Rust の LLM 層を `fifty_four_llm` crate に切り出し、PyO3 で Python に直接リンクする案(下記「将来の課題」)

## アーキテクチャ

```
ブラウザ ──htmx POST──▶ shinasadame web (FastAPI)
                          └─ ReplayClient ──stdin/stdout(JSON Lines)──▶ fifty_four_lsp --replay (常駐)
                                                                         └─ llm.rs ──▶ Google / OpenAI / Anthropic / xAI / LM Studio / Cloudflare
```

- 子プロセスは web コンテナ内で、初回の試行時に遅延起動して使い回す。
- 辞書ロードなどの初期化は子プロセスの起動時に1回だけ行う。

## LSP 側: `--replay` モード

`lsp/src/main.rs` の引数処理(`--acp` と同じ階層)に `--replay` を追加する。LSP プロトコルは話さない。

### Cargo feature で分離する

`--replay` は feature `replay` の裏に置く。**既定では有効にしない**(`default = ["otel"]` のまま)。

- 実装は `lsp/src/replay.rs` に閉じ込め、`main.rs` の `mod replay;` と引数分岐を `#[cfg(feature = "replay")]` で囲む。
- Zed 拡張として配布する通常の LSP ビルドには、`--replay` のコードが一切入らない。
- shinasadame のイメージ用ビルドだけが `cargo build --release -p fifty_four_lsp --features replay` で有効にする。
- `replay` 自体は新しい依存を必要としない(`tokio`・`serde_json` は既存)。ただし後述の HTTP 公開(未決)を選ぶ場合は、`axum` などを `replay` の optional 依存にして、通常ビルドに混ぜない。
- feature 無効時に `--replay` が渡された場合は、「この実行ファイルは replay feature なしでビルドされている」と stderr に出して、終了コード 2 で終了する(黙って LSP として起動しない)。
- CI(`cargo test -p fifty_four_lsp`)は既定 feature のテストに加え、`--features replay` のテストも走らせる。

### プロトコル(JSON Lines)

標準入力から1行1リクエストを読み、標準出力へ1行1応答を返す。1行は改行を含まない JSON。

リクエスト:

```json
{"id": 7, "model": "google/gemini-3.1-pro", "prompt": "...", "timeout_secs": 120}
```

応答(成功):

```json
{"id": 7, "ok": true, "response": "...", "model": "google/gemini-3.1-pro", "elapsed_ms": 4210}
```

応答(失敗):

```json
{"id": 7, "ok": false, "error": {"kind": "timeout", "message": "..."}}
```

- `model` の書式は既存の `Provider::from_name`(`provider/model`、`lmstudio@host:port/model`)をそのまま使う。
- `error.kind`: `bad_request`(JSON 不正・モデル名不正) / `auth`(認証失敗) / `timeout` / `llm`(LLM 側のエラー) / `internal`。
- リクエストごとに `tokio::spawn` で並行処理する。応答の順序は保証せず、`id` で突き合わせる。
- タイムアウトは `tokio::time::timeout` で `id` 単位に打ち切る。プロセスは落とさない。
- 標準入力が閉じたら、処理中のリクエストを待ってから正常終了する。
- **標準出力はこのプロトコル専用**。ログは既存どおり stderr / OTel のみに出す。`--replay` 中は標準出力に何も書く経路を作らない。
- LLM 呼び出しは既存の `build_client` / `use_llm_with_option` を通し、`crate::metrics` の計装もそのまま働く。
- tool は登録しない。記録されている `prompt` を、そのままユーザープロンプトとして渡す。
- **未確認**: LSP は `build_client` にシステムプロンプト名(`sys_prompt_name`)を渡す経路があるが、記録される `prompt` にそれが含まれるかは未確認。含まれない場合、再実行は元と条件が異なる。実装計画の最初に確認し、必要ならリクエストに `system` を足す。
- `cfg(debug_assertions)` で囲わない。イメージには release ビルドを入れるため(FlightRecorder だけが debug 専用)。

## shinasadame 側

### ReplayClient(`shinasadame/replay.py`、新規)

- 設定の `REPLAY_BIN`(既定 `/usr/local/bin/fifty_four_lsp`)の子プロセスを `asyncio.create_subprocess_exec` で起動する。シェルは介さない。
- `submit(model, prompt)` が `id` を採番して1行書き込み、`id` ごとの `asyncio.Future` で応答を待つ。標準出力の読み取りタスクが `id` で Future を解決する。
- 子プロセスが終了した場合は、待機中の Future をすべて `error.kind = "internal"` で失敗させ、次の `submit` で再起動する。
- stderr は読み捨てずに、ログへ流す(子プロセスの詰まり防止)。
- 同時実行数は `asyncio.Semaphore` で制限する(既定 2)。
- Python 側の待機にも上限(`timeout_secs + 10` 秒)を置き、Rust 側が応答しない場合に備える。
- 入力の `prompt` は 200,000 文字、応答は 1,000,000 文字を上限とし、超過は `bad_request` / 切り詰めとして扱う。

### エンドポイント

`POST /s/{sid}/records/{table}/{id}/try`(フォーム: `prompt`, `model`)

- `table` が対象外、またはレコードが無ければ 404。
- 成功時は htmx の部分テンプレート(`_trial_result.html`)を返す。元の応答と試行結果を左右に並べる。`elapsed_ms` と使用モデルを添える。
- 失敗時は `error.kind` ごとの文言を同じ部分テンプレートで表示する(HTTP ステータスは 200。htmx がスワップするため)。

### 画面(`record_detail.html`)

- `prompt` を持つテーブルの詳細に「試す」パネルを追加する。
  - `prompt` の編集欄(元のプロンプトを初期値にする)
  - `model` の選択欄: 記録の `model_name` を既定値にし、`SHINASADAME_MODELS`(カンマ区切りの `provider/model`)の一覧を選択肢に足す。自由入力も許す。
  - 「試す」ボタン(`hx-post`、実行中は無効化してスピナー表示)
  - 注記: 「tool なしで実行します。元の実行とは条件が異なります」
- 試行結果は部分テンプレートの差し込み先に表示する。

### 設定(`config.py`)

| 環境変数 | 内容 | 既定 |
|---|---|---|
| `REPLAY_BIN` | `fifty_four_lsp` のパス | `/usr/local/bin/fifty_four_lsp` |
| `SHINASADAME_MODELS` | 選択肢にする `provider/model`(カンマ区切り) | 空 |
| `REPLAY_MAX_CONCURRENCY` | 同時実行数 | 2 |
| `REPLAY_DEFAULT_TIMEOUT_SECS` | LLM 呼び出しの上限 | 120 |

## API キー・ネットワーク

- API キーは k8s の Secret から web コンテナの環境変数で渡す。キー名は genai(Rust 側)が読むものに合わせる。画面・ログ・応答にキーを出さない。
- LM Studio(ホストの `localhost:1234`)は、Pod からは `lmstudio@host.docker.internal:1234/<model>` で指定する。ホスト側の到達性は実装時に検証する。
- 子プロセスの環境変数は、必要な API キーと `PATH` 程度に絞る(web プロセスの全環境を引き継がない)。

## ビルド・配備

- `shinasadame/Dockerfile` を多段ビルドにする。ビルドステージで `cargo build --release -p fifty_four_lsp --features replay` を実行し、成果物を実行ステージの `/usr/local/bin/` へコピーする。
- Rust のビルドにはリポジトリルートが必要なので、ビルドコンテキストをリポジトリルートに変更し、Dockerfile のパスを調整する(`COPY` のパス、k8s のビルド手順)。
- `lindera` の辞書埋め込み(`embed-ipadic`)と `rusqlite`(bundled)で、初回ビルドは重い。Rust ステージを依存だけ先にビルドしてレイヤーキャッシュを効かせる(cargo-chef 等)かは、実装計画で判断する。

## 配備形態の選択肢(未決)

上記は「web コンテナ内の子プロセス(stdio)」を基本形として書いている。別案として、`fifty_four_lsp` を**独立した Pod(またはサイドカーコンテナ)にして HTTP で呼ぶ**形も検討する。

| 観点 | A: web 内の子プロセス(stdio) | B: 独立コンテナ/Pod(HTTP) |
|---|---|---|
| Python イメージ | Rust ビルドステージが必要(Dockerfile が重くなる) | Python のイメージは今のまま。Rust は別イメージ |
| API キー | web コンテナが持つ | replay コンテナだけが持つ(web から分離できる) |
| 障害・再起動 | 子プロセスの再起動を自前で実装 | k8s の再起動に任せられる |
| 通信 | パイプ(設定不要) | HTTP(ポート、ヘルスチェック、Service が必要) |
| 構成の重さ | 軽い | k8s マニフェストが増える |

B の実現方法は2つ考えられる。
1. **既存の `--replay`(stdio)を、CLI を HTTP で公開する汎用ラッパで包む**。ユーザーから `webify` や `http-cmd…` といった名前が挙がっているが、**ツールの実体・仕様は未確認**。常駐プロセスの標準入出力を HTTP に橋渡しできるか(リクエストごとに起動する方式だと、常駐の利点が失われる)を、採用前に調べる。
2. **LSP 自身に HTTP サーバー(`axum` 等)を持たせる**。feature `replay-http` として optional 依存にし、通常ビルドには混ぜない。

どちらでも成立するよう、**JSON のリクエスト/応答の形(上記プロトコル)をトランスポートから独立させる**。`ReplayClient` は `StdioTransport` と `HttpTransport` を差し替えられる作りにする。まず A(stdio)で実装し、B は同じプロトコルのまま後から足せる。

## エラー処理

| 状況 | 挙動 |
|---|---|
| JSON 不正・モデル名不正 | `bad_request`。画面にメッセージを表示 |
| 認証エラー(キー未設定を含む) | `auth`。どの provider のキーかを示す(キー値は出さない) |
| LLM のタイムアウト | `timeout`。子プロセスは生き続ける |
| LLM 側のエラー | `llm`。メッセージを表示 |
| 子プロセスの異常終了 | `internal`。次の試行で自動再起動 |
| 同時実行数の超過 | セマフォで待機(待機が長いときはタイムアウト) |

## テスト

Rust(`lsp`):
- `--replay` の入出力処理を関数に切り出して単体テストする: 正常、`bad_request`(JSON 不正・モデル名不正)、`timeout`、LLM エラー、標準入力クローズ時の終了、`id` の取り違えがないこと。
- LLM はテスト用スタブ(`LlmInterface` の偽実装)を使い、ネットワークに出ない。
- 標準出力にプロトコル以外が出ないことを、出力バッファを注入する形で検証する。

Python(`shinasadame`):
- 偽の `--replay` スクリプトを `REPLAY_BIN` で差し替えて、`ReplayClient` を検証する: 正常、エラー、並行リクエストの `id` 突き合わせ、子プロセス終了後の再起動、タイムアウト。
- エンドポイントのテスト: 対象外テーブル、存在しないレコード、成功時の部分テンプレートに元の応答と試行結果が並ぶこと、`tool なし` の注記があること。
- CI(`ubuntu-slim`)で Rust バイナリが無くても通るよう、実バイナリを使うテストは含めない。

## 将来の課題

- tool の再現(上記)。
- 独立 Pod 化(上記「配備形態の選択肢」)。ラッパの調査結果で A/B を判断する。
- `llm.rs` を `fifty_four_llm` crate に切り出し、PyO3 で Python から直接呼ぶ構成。DSPy で大量の呼び出しを行う段階で、起動コストと IPC の置き換えを判断する。`llm.rs` が `crate::` 経由で依存するのは `metrics` / `assets` / `text` の3つだけなので、切り出しの見通しは立っている。
- 試行結果の保存(履歴・比較)。必要になった時点で、app.db にテーブルを追加する。
