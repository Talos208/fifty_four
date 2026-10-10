# shinasadame プロンプト試行機能 設計仕様

**日付:** 2026-10-09(2026-10-10 改訂: stdio の子プロセス方式を、別 Pod の HTTP サーバー方式に変更)

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
- `fifty_four_lsp` の `replay` feature(HTTP サーバーモード)、そのイメージと k8s マニフェスト(別 Pod)
- shinasadame 側の HTTP クライアント層

含まない(後続の課題):
- **tool(`character_info` / `plot_info`)の再現**。元の実行は tool を使っているため、本機能の再実行は元と条件が異なる。画面に「tool なしで実行」と明記する。扱い方は未決(候補: 実行時の tool 結果を LSP が記録して再生 / 画面で貼り付け)
- 試行結果の保存(画面上の一時表示のみ。ページを閉じると消える)
- DSPy による評価・最適化
- Rust の LLM 層を `fifty_four_llm` crate に切り出し、PyO3 で Python に直接リンクする案(下記「将来の課題」)

## アーキテクチャ

```
ブラウザ ──htmx POST──▶ Pod: shinasadame (web + worker)
                          └─ ReplayClient ──HTTP(JSON)──▶ Service: replay (ClusterIP :8080)
                                                            └─ Pod: replay
                                                                └─ fifty_four_lsp --replay (axum)
                                                                    └─ llm.rs ──▶ Google / OpenAI / Anthropic / xAI / LM Studio / Cloudflare
```

- **replay は独立した Pod(別 Deployment)**。shinasadame の Pod(web + worker + PVC)とは、イメージ・ライフサイクル・Secret を分ける。
- 通信は ClusterIP の Service 経由の HTTP のみ。外部には公開しない。
- API キーは replay の Pod だけが持つ。shinasadame の Pod には渡さない。

## LSP 側: `replay` feature

### Cargo feature で分離する

`replay` は feature として分け、**既定では有効にしない**(`default = ["otel"]` のまま)。

- 実装は `lsp/src/replay.rs` に閉じ込め、`main.rs` の `mod replay;` と引数分岐を `#[cfg(feature = "replay")]` で囲む。
- HTTP サーバー用の依存(`axum` など)は `replay` の optional 依存にする。Zed 拡張として配布する通常の LSP ビルドには、サーバーのコードも依存も入らず、ビルド時間も増えない。
- replay 用イメージだけが `cargo build --release -p fifty_four_lsp --features replay` で有効にする。
- feature 無効時に `--replay` が渡された場合は、「この実行ファイルは replay feature なしでビルドされている」と stderr に出して、終了コード 2 で終了する(黙って LSP として起動しない)。
- CI(`cargo test -p fifty_four_lsp`)は既定 feature のテストに加え、`--features replay` のテストも走らせる。

### 起動

```
fifty_four_lsp --replay [--listen 0.0.0.0:8080]
```

- `--replay` のときは LSP プロトコル(stdio)を話さず、HTTP サーバーだけを起動する。
- `--listen` が無ければ環境変数 `REPLAY_LISTEN`、それも無ければ `0.0.0.0:8080`。
- SIGTERM を受けたら、処理中のリクエストの完了を待って終了する(graceful shutdown)。
- ログ・トレースは既存の仕組み(stderr / OTLP)に出す。標準出力は使わない。

### エンドポイント

**`POST /v1/replay`**

リクエスト(`application/json`):

```json
{"model": "google/gemini-3.1-pro", "prompt": "...", "timeout_secs": 120}
```

- `model` の書式は既存の `Provider::from_name`(`provider/model`、`lmstudio@host:port/model`)をそのまま使う。
- `timeout_secs` は省略可(既定 120、上限 600)。

成功(HTTP 200):

```json
{"response": "...", "model": "google/gemini-3.1-pro", "elapsed_ms": 4210, "truncated": false}
```

失敗(`application/json`、`kind` は次の表):

```json
{"error": {"kind": "timeout", "message": "..."}}
```

| `kind` | HTTP | 状況 |
|---|---|---|
| `bad_request` | 400 | JSON 不正・モデル名不正・`prompt` が空または上限超過 |
| `auth` | 502 | provider の認証失敗(キー未設定を含む)。どの provider かを `message` に含め、キー値は出さない |
| `timeout` | 504 | LLM 呼び出しが `timeout_secs` を超えた |
| `llm` | 502 | LLM 側のその他のエラー |
| `internal` | 500 | 予期しないエラー |

**`GET /healthz`**: 200 と `ok`(k8s の readiness / liveness 用)。

### 動作

- LLM 呼び出しは既存の `build_client` / `use_llm_with_option` を通し、`crate::metrics` の計装もそのまま働く。
- tool は登録しない。記録されている `prompt` を、そのままユーザープロンプトとして渡す。
- **未確認**: LSP は `build_client` にシステムプロンプト名(`sys_prompt_name`)を渡す経路があるが、記録される `prompt` にそれが含まれるかは未確認。含まれない場合、再実行は元と条件が異なる。実装計画の最初に確認し、必要ならリクエストに `system` を足す。
- 同時実行数は `tokio::sync::Semaphore` で制限する(環境変数 `REPLAY_MAX_CONCURRENCY`、既定 4)。上限を超えたリクエストは待機する。
- リクエストボディの上限は 1 MiB。応答は 1,000,000 文字で切り詰め、切り詰めたら `truncated: true` を返す。
- `cfg(debug_assertions)` で囲わない。イメージには release ビルドを入れるため(FlightRecorder だけが debug 専用)。

## shinasadame 側

### ReplayClient(`shinasadame/replay.py`、新規)

- `httpx.AsyncClient`(接続の再利用)で `POST {REPLAY_URL}/v1/replay` を呼ぶ。`httpx` は dev 依存から実行時依存に移す。
- クライアント側のタイムアウトは `timeout_secs + 10` 秒。
- 接続できない・タイムアウトした場合は `ReplayUnavailable` とし、画面に「replay サービスに接続できません」と表示する。
- レスポンスの `error.kind` を型にして呼び出し側へ返す。HTTP ステータスの解釈は ReplayClient に閉じ込める。

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
| `REPLAY_URL` | replay サービスの URL | `http://replay:8080` |
| `SHINASADAME_MODELS` | 選択肢にする `provider/model`(カンマ区切り) | 空 |
| `REPLAY_DEFAULT_TIMEOUT_SECS` | LLM 呼び出しの上限 | 120 |

## ビルド・配備

### イメージ

- 新規 `lsp/Dockerfile.replay`(ビルドコンテキストはリポジトリルート)。多段ビルドで、ビルドステージが `cargo build --release -p fifty_four_lsp --features replay` を実行し、実行ステージ(`debian:bookworm-slim` 等)に成果物をコピーする。
- 実行ステージには CA 証明書(`ca-certificates`)を入れる。HTTPS で各 provider に接続するため。
- `lindera` の辞書埋め込み(`embed-ipadic`)と `rusqlite`(bundled)で、初回ビルドは重い。依存だけ先にビルドしてレイヤーキャッシュを効かせる(cargo-chef 等)かは、実装計画で判断する。
- shinasadame の Dockerfile は変更しない(Python のイメージのまま)。

### k8s(`shinasadame/k8s` 配下に追加)

- `replay` Deployment(replicas 1)、`replay` Service(ClusterIP、8080)。
- `replay` Secret: 各 provider の API キー。キー名は genai(Rust 側)が読む環境変数に合わせる。Secret は replay の Deployment だけがマウントする。
- readiness / liveness は `GET /healthz`。
- 環境変数 `REPLAY_MAX_CONCURRENCY`、`OTEL_EXPORTER_OTLP_ENDPOINT`(SigNoz)を設定する。
- LM Studio(ホストの `localhost:1234`)は、Pod からは `lmstudio@host.docker.internal:1234/<model>` で指定する。ホスト側の到達性は実装時に検証する。
- shinasadame の Deployment には `REPLAY_URL` と `SHINASADAME_MODELS` だけを渡す。API キーは渡さない。
- NetworkPolicy は、ローカル専用の前提なので初期は設けない。

## エラー処理

| 状況 | 挙動 |
|---|---|
| JSON 不正・モデル名不正 | `bad_request`。画面にメッセージを表示 |
| 認証エラー(キー未設定を含む) | `auth`。どの provider のキーかを示す(キー値は出さない) |
| LLM のタイムアウト | `timeout`。サーバーは生き続ける |
| LLM 側のエラー | `llm`。メッセージを表示 |
| replay サービスに接続できない | `ReplayUnavailable`。「replay サービスに接続できません」を表示 |
| 同時実行数の超過 | サーバー側のセマフォで待機(待機が長いときはクライアント側のタイムアウト) |

## テスト

Rust(`lsp`、`--features replay`):
- axum の Router を `tower::ServiceExt::oneshot` で直接呼び、HTTP を立てずに検証する。LLM はテスト用スタブ(`LlmInterface` の偽実装)を使い、ネットワークに出ない。
- ケース: 正常、`bad_request`(JSON 不正・モデル名不正・空プロンプト)、`timeout`、LLM エラー、認証エラー、応答の切り詰め、`/healthz`。
- 同時実行の上限が効くこと(セマフォ)。

Python(`shinasadame`):
- `httpx.MockTransport` で ReplayClient を検証する: 正常、各 `error.kind` の写像、接続失敗、タイムアウト。
- エンドポイントのテスト: 対象外テーブル、存在しないレコード、成功時の部分テンプレートに元の応答と試行結果が並ぶこと、`tool なし` の注記があること。
- CI(`ubuntu-slim`)で Rust バイナリや replay サービスが無くても通るよう、実サービスを使うテストは含めない。

## 検討したが採用しなかった案(2026-10-10 の調査)

- **web コンテナ内の子プロセス(stdio、JSON Lines)**: 軽いが、Python のイメージに Rust のビルドステージが入り、API キーが web 側に載り、子プロセスの再起動・`id` の突き合わせを自前で実装することになる。
- **既存 CLI を HTTP 化する汎用ラッパ**:
  - [`webify`](https://hub.docker.com/r/beefsack/webify): リクエストごとにコマンドを起動し、ボディを標準入力、標準出力をレスポンスにする。
  - [`http-cmd`](https://www.lib.rs/crates/http-cmd)(Rust の crate): 同様にリクエストごとの実行。静的サイトジェネレーター向けで、公開しないよう README に注意書きがある。
  - [`servep`](https://www.npmjs.org/package/servep)(npm): session-id ごとに常駐プロセスを持てるが、Node.js の依存が増える。
  - いずれも、LSP 自身が HTTP を話すほうが構成が単純なため採用しない(`webify` / `http-cmd` はリクエストごとの起動で、初期化が毎回走る)。

## 将来の課題

- tool の再現(上記)。
- `llm.rs` を `fifty_four_llm` crate に切り出し、PyO3 で Python から直接呼ぶ構成。DSPy で大量の呼び出しを行う段階で、HTTP のオーバーヘッドと合わせて判断する。`llm.rs` が `crate::` 経由で依存するのは `metrics` / `assets` / `text` の3つだけなので、切り出しの見通しは立っている。
- 試行結果の保存(履歴・比較)。必要になった時点で、app.db にテーブルを追加する。
- replay サービスへの認証・NetworkPolicy(ローカル専用でなくなる場合)。
