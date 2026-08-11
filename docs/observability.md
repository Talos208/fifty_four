# OpenTelemetry / ログ計装

`lsp/src/logging.rs` が担う、ログ・トレース・メトリクスの計装まわりの環境変数と挙動をまとめる。
実装の背景・経緯（何が壊れていて何を直したか）は `docs/plans/` の該当する調査記録を参照。

## 有効/無効の切り替え

- **ビルド時**: `lsp` クレートの Cargo feature `otel`（既定で有効）。無効化すると
  OpenTelemetry 一式を一切リンクせず、代わりに `env_logger` ベースの `prepare_env_logger()`
  にフォールバックする（`cargo build --no-default-features`）。
- **実行時**: `RUST_LOG` を `off`、またはこの crate 名を対象にした `off`
  （例: `fifty_four_lsp=off`）にすると、`logging_disabled()` が検知してエクスポータ・
  プロバイダの生成そのものをスキップする（gRPC 接続の試行すら発生しない）。
  `RUST_LOG` 未設定時は `EnvFilter::from_default_env()` の既定（ERROR 相当）になり、
  これは「無効化」とは区別される。

## エンドポイント

`OTLP_ENDPOINT` のような自前の定数は無い。`opentelemetry-otlp` の
`TonicExporterBuilder::resolve_endpoint()` の解決順にそのまま任せている
（`.with_endpoint(...)` を呼んでいないため）。

1. シグナル別環境変数（`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` /
   `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` / `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT`）
2. 汎用 `OTEL_EXPORTER_OTLP_ENDPOINT`
3. 既定値 `http://localhost:4317`

いずれも gRPC (`tonic`) 前提。別ポート・別ホストのコレクタへ向けたい場合は起動前に
上記のいずれかを設定する。

## service.name

LSP と ACP は別プロセスとして起動される（`docs/acp-agent.md` 参照）ため、
コレクタ側で区別できるよう `service.name` を分けている。

| モード | service.name |
|---|---|
| LSP サーバ | `fifty_four_lsp` |
| ACP エージェント (`--acp`) | `fifty_four_acp` |

`service.version` は両方とも `CARGO_PKG_VERSION`（ビルド時のクレートバージョン）。

## `RUST_LOG` のスコープ（ACP 特有の注意）

`RUST_LOG` は OTel へ送るかどうかの `EnvFilter` と、開発ビルドの stderr ミラー
（後述）の両方に効く、通常の `tracing-subscriber` の仕組みそのまま。

ACP モード (`--acp`) だけ特別な既定値がある。Zed は `agent_servers` 経由でこの
バイナリを直接起動するため、ターミナルから `RUST_LOG` を渡す手段が事実上ない。
そこで `main.rs` の `default_acp_log_level()` が、**`RUST_LOG` にこの crate 名
（`fifty_four_lsp`）への言及が含まれていなければ**、ACP 関連モジュール
（`acp`/`acp_config`/`writing_agent`/`session_log`）だけを debug にする既定値を
上書き設定する:

```
warn,fifty_four_lsp::acp=debug,fifty_four_lsp::acp_config=debug,\
fifty_four_lsp::writing_agent=debug,fifty_four_lsp::session_log=debug
```

「含まれていなければ」という判定にしているのは、**Zed 自身が起動時点で
`RUST_LOG`（例: `RUST_LOG=lsp=trace` のような Zed 自身のデバッグ用の値）を
すでに設定しており、それが子プロセスへそのまま継承されるケースがある**ため。
`lsp` というターゲットはこの crate のどのモジュールパス（`fifty_four_lsp::*`）にも
マッチせず、かつベアの（対象なしの）デフォルトレベル指定を含まないディレクティブ
なので、素通ししてしまうと `EnvFilter` が全イベントを弾き、ACP のログ・トレースが
stderr にも OTel にも一切出なくなる（実際にこの不具合が起きた）。

明示的にこの crate 向けの `RUST_LOG`（例: `fifty_four_lsp=debug`）を
`agent_servers` の `env` に設定していれば、それは尊重されて上書きされない
（`docs/acp-agent.md` の該当セクション参照）。

## ノイズ抑制

`suppress_transport_noise()` が以下を常に抑制する（stderr・OTel 向けレイヤ共通）:

- `hyper`/`h2`/`tonic`/`tower`: gRPC 通信そのもののフレーム単位ログ
- `opentelemetry_sdk`: バッチ処理スレッドが定期タイマーで吐く内部 housekeeping ログ
  （`BatchLogProcessor.ExportingDueToTimer` 等。`off` ではなく `warn` 止まりなので、
  実際のエクスポート失敗など有用な情報は残る）

さらに OTel 向けレイヤ限定で `reqwest`/`opentelemetry` も外している。これは
「エクスポート自身のログをまたエクスポートする」フィードバックループを断つためで、
stderr はどこにも再送されないためこの心配はなく、対象外にしている。

## stderr ミラー

`fmt::layer()` による stderr への可読ログ出力は **debug ビルドのみ**
（`#[cfg(debug_assertions)]`）。配布バイナリ（`--release`）には含まれない。
LSP・ACP とも stdin/stdout を JSON-RPC チャネルとして使うため、ログは常に stderr。

## トレース伝播とバックグラウンドタスク

`tracing` のスパンコンテキストはスレッドローカル管理のため、`tokio::spawn(...)` で
別タスクへ切り離すと親スパンを暗黙には引き継がない。`#[instrument]` を付けた関数を
そのまま spawn すると、毎回**新しい独立したトレース**になってしまい、呼び出し元の
トレースから辿れなくなる。

回避するには spawn 前に `tracing::Instrument::instrument(tracing::Span::current())`
で明示的に親スパンを運ぶ。`backend.rs` の `character_updater::run` の spawn 箇所が例:

```rust
let fut = crate::character_updater::run(/* ... */);
#[cfg(feature = "otel")]
let fut = {
    use tracing::Instrument;
    fut.instrument(tracing::Span::current())
};
tokio::spawn(fut);
```

新しく spawn するコードを書く際は、spawn 先の関数が `#[instrument]` されているなら
同様の対応が必要かどうか確認すること。

## 関連ファイル

- `lsp/src/logging.rs` — 本ドキュメントが説明する実装本体
- `lsp/src/main.rs` — `default_acp_log_level()`（ACP 用 `RUST_LOG` 既定値）
- `docs/acp-agent.md` — ACP エージェント固有のログ確認手順


---
以下メモ

## 外部のCollector（OpenTelemetry）で制御する（推奨）

コード側からは OpenTelemetry（OTel） のプロトコルを使って全てのログやスパンをノーフィルターで垂れ流し、受け手である OpenTelemetry Collector 側で「エラーじゃなかったら捨てる（端折る）」という処理を行います。

### 設定方法（OTel Collector の設定ファイル config.yaml）

OpenTelemetry Collector には、標準で tail_sampling プロセッサ という機能が備わっています。これを使うと、Collector側で以下のようなルールを定義できます。

```yaml
processors:
  tail_sampling:
    decision_wait: 10s # トレース（一連の処理）が終了するまで最大10秒待ってから判断する
    num_traces: 10000
    policies:
      [
        {
          name: filter_errors_only,
          type: status_code,
          status_code: {status_codes: [ ERROR ]} # ステータスがERRORのものだけを100%通す
        },
        {
          name: sample_normal_traffic,
          type: probabilistic,
          probabilistic: {sampling_percentage: 1.0} # 正常なログは全体の1%だけ生存報告として残し、99%は捨てる
        }
      ]

service:
  pipelines:
    traces:
      receivers: [otlp]
      processors: [tail_sampling, batch] # ここに仕込む
      exporters: [jaeger, otlphttp/loki]
```
