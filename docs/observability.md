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

## レベルフィルタ（OTel 送出）

`otel_filter()` は **`RUST_LOG` の値を一切見ず、常に TRACE まで送出する**
（ノイズ抑制の対象を除く。下記参照）。

以前は `EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy()`
で `RUST_LOG` 任せにしていたが、`RUST_LOG=fifty_four_lsp=trace` を指定しても
TRACE が出ないことがあった。原因は `log::` マクロ側にある: `tracing-subscriber` の
`init()` は `tracing-log` feature 経由で `tracing_log::LogTracer` を自動初期化し、
その際 `log::set_max_level()` を「グローバル subscriber 全体の
`EnvFilter::max_level_hint()`」に合わせて設定する。この合成された実効レベルが
低いと、`log::trace!`（このリポジトリの `trace!` はほぼ全て `log` クレート由来、
`tracing::trace!` ではない）系のマクロ呼び出しがそもそも `log` の時点で
弾かれ、`tracing` 側にすら伝わらない。`RUST_LOG` の内容や複数レイヤの合成に
左右されず確実に出すため、`EnvFilter::new("trace")` で固定している。

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

## stderr フォーマッタ(`PlainFormat` / `SecondsUtcTime`)

開発ビルドの stderr ミラーは `tracing_subscriber::fmt` の既定フォーマッタを使わず、
`lsp/src/logging.rs` の `PlainFormat`(`FormatEvent`)と `SecondsUtcTime`(`FormatTime`)を
自前実装している。背景:

- **`with_line_number`/`with_file`/`with_target` は効かない**: これらは
  `fmt::layer()` が内蔵する既定 `Format` 用のオプションで、`.event_format(PlainFormat)`
  で丸ごと差し替えると黙って無視される。そのため file:line の表示と target(モジュール
  パス)の非表示は `PlainFormat::format_event` 内で手書きしている(target は「file:line の
  方がエディタからジャンプできて情報として上位互換」という判断で意図的に省いている)。
- **`FmtSpan::NONE` は `#[instrument(ret)]` の戻り値イベントを止めない**: `FmtSpan` が
  抑制するのは fmt レイヤ自身が合成する ENTER/EXIT 等の span ライフサイクルイベントのみで、
  `#[instrument(ret)]` が関数終了時に発行する戻り値イベントは通常の `Event`(span ではない)
  として素通りする。これは `log!`/`tracing::info!` 等と違い `"message"` フィールドを持たず
  `"return"` フィールドのみを持つため、`prepare_stderr_tracing()` では
  `meta.fields().field("message").is_some()` を条件にした `filter_fn` を `EnvFilter` に
  `.and()` で重ねて除外している(`Metadata::fields()` はコールサイトで静的に決まるフィールド
  名の集合なので、実行時コストなしに判別できる)。span 自体は `is_event()` で除外対象から
  外し、通常の span 伝播は妨げない。
- **時刻は秒精度**: 既定の `fmt::time::SystemTime` はナノ秒まで出て冗長なため、
  `time` クレート(既存依存、`parse_borrowed` は実行時パース。マクロ版
  `time::macros::format_description!` を使うと "macros" feature が余分に要るため避けた)
  で `YYYY-MM-DDTHH:MM:SSZ`(UTC)に固定している。

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

## panic の Otel 送出

panic は stdio（JSON-RPC チャネル）にも Zed のログにも残らないため、`prepare_network_tracing()`
が `install_otel_panic_hook()` で panic hook を設置し、`tracing::error!(target: "panic", ...)`
として `otel_log_layer` 経由で Otel へ送る（デフォルトの panic 出力はそのまま残す）。
`tracing_subscriber::registry()...init()` の**後**に設置しないとイベントが届かないため、
その順序に依存する。

panic 直後にプロセスが異常終了するとバッチエクスポータの周期フラッシュを待てないため、
ログ発行後に `logger_provider.force_flush()` を明示的に呼ぶ。

**発生位置は `log.file`/`log.line` という独自フィールドで送る。**
`tracing` のマクロは `file!()`/`line!()` をコンパイル時に静的展開するため、
`meta.file()/line()`(`opentelemetry-appender-tracing` が `code.filepath`/`code.lineno` へ
マッピングする値)は常にこのフック自身の呼び出し箇所を指してしまい、実際のpanic発生位置には
ならない。そのため `PanicHookInfo::location()` から実際の発生位置を取り出し、別名の
フィールドとして明示的に付与している。ドット付きフィールド名を並べたままメッセージ文字列
(`"{}", info`)を続けると `tracing::error!` のマクロパーサが "local ambiguity" で構文解析に
失敗するため、`{ log.file = file, log.line = line }` のように `{ .. }` でフィールドを
明示的にブロック化している(`error!` マクロが `target: .., { フィールド… }, メッセージ`
という中括弧区切りの構文を別途サポートしているため)。

ACP モード (debug ビルド) には別途 `main.rs` の `install_acp_panic_hook()` があり
（`logs/acp_panic.log` へファイル追記）、こちらは `Logger::new()` の後に設置されるため、
両方の panic hook が(`take_hook`/`set_hook` のチェーンで)両方とも効く。

## Otelへ送出されず終了する経路(既知の制約)

panic 以外にも「Otel へ何も送出せずプロセスが終了する経路」がないか調査した結果を記録する。

1. **シグナル/強制終了**(SIGINT/SIGTERM、Windows の Ctrl+Close イベント等):
   コードベースにハンドラが一切無く、受信時は OS のデフォルト処理で即終了する
   (`Logger::drop()` の shutdown も panic hook も実行されない)。
   → **意図的に対応しない**。クライアントアプリでのシグナル/強制終了はユーザー起因で
   発生源が分かっており、ログが無いと原因不明になるケースではないため。
2. **`std::process::exit` の直接呼び出し**: 本稿執筆時点で `main.rs` に 2 箇所あるが、
   いずれも `Logger` 生成前、または明示的に `drop(_log)` してから呼んでおり問題ない。
   ただし `process::exit` は Drop を一切実行しないため、将来同様の呼び出しを追加する際は
   `Logger` の生存期間と shutdown 順序を必ず確認すること。
3. **OOM・二重 panic**: Rust の既定挙動で abort するため対処が現実的でない
   (OOM ハンドラのカスタマイズは nightly 限定、二重 panic は極めて稀)。
4. メインタスクの通常 panic は `panic = "abort"` の設定が無い(既定の unwind)ため、
   `async_main` 内での unwind により `_log: Logger` の Drop が正しく呼ばれ、既に安全。
   `tokio::spawn` された別タスクの panic は上記のグローバルな panic hook で既にカバー済み。

## 関連ファイル

- `lsp/src/logging.rs` — 本ドキュメントが説明する実装本体
- `lsp/src/main.rs` — `default_acp_log_level()`（ACP 用 `RUST_LOG` 既定値）、`install_acp_panic_hook()`
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
