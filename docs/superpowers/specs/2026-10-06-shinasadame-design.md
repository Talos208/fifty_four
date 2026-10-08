# shinasadame 設計仕様

**日付:** 2026-10-06

## 概要

FlightRecorder(`lsp/src/flight_recorder.rs`)が debug ビルドで記録する SQLite を取り込み、閲覧・分析する Web アプリ `shinasadame` を fifty_four リポジトリ内に追加する。
名前は源氏物語「帚木」の「雨夜の品定め」から(LLM 出力の品定め=評価)。

最終的には DSPy によるプロンプト最適化を載せるが、**最初のマイルストーンは「記録の閲覧・分析」**。DSPy はスコープ外。

### 前提・制約

- 公開先はローカルのみ。基盤は Docker Desktop の Kubernetes(**kind 方式**、ノード3台、containerd)。
- SigNoz は同クラスタの `signoz` namespace にある(OTLP: `http://localhost:4317`)。
- kind ノードからは Windows のファイルシステムが見えないため、hostPath で `db/` をマウントする方式は取れない。
- `docker build` したイメージは Docker Desktop の `registry-mirror` 経由で kind から pull できる(push 不要)。
- LoadBalancer Service は localhost に公開される。
- FlightRecorder の DB は LSP の所有物。shinasadame は**読むだけで書き戻さない**。

### スコープ

含む:
- ブラウザからの FlightRecorder DB 取り込み(バッチジョブとして実行)
- 補完の採用率分析
- 全テーブルのレコード一覧・検索・詳細閲覧
- k8s マニフェスト、SigNoz へのトレース送信

含まない:
- DSPy による評価・最適化(次のマイルストーン)
- プロンプトテンプレート種別ごとの採用率(`completions` にテンプレート種別の列がないため。必要になったら LSP 側に V6 マイグレーションを追加する)
- 確信度と採用率の相関、品質指摘の傾向分析

## アーキテクチャ

```
Windows ホスト (Chrome/Edge)
 ├─ fifty_four/target/debug/db/fifty_four.db (LSP が今書いている DB)
 └─ fifty_four/db/fifty_four.db              (以前の LSP が書いた DB など、任意の FlightRecorder DB)
        │ File System Access API(FileHandle を IndexedDB に保存)
        │ 取り込みボタン → ファイル読み取り → POST /api/imports (multipart)
        ▼  http://localhost:8054
k8s namespace: shinasadame
 Deployment: shinasadame (replicas 1)
 ├─ container: web     FastAPI + Jinja2 + HTMX (uvicorn)
 ├─ container: worker  Huey consumer (SqliteHuey)
 └─ PVC /data (512Mi, storageClass standard, RWO)
     ├─ queue.db      Huey のキュー
     ├─ app.db (WAL)  imports / snapshots / fr_* テーブル
     └─ uploads/      アップロードの一時置き場
 Service: LoadBalancer 8054 → localhost:8054
 OTLP gRPC → http://localhost:4317
```

- web と worker は**同一 Pod** に置き、同じ PVC 上の SQLite を共有する。同一ノード・同一ファイルシステムなので SQLite のロックが正しく機能する。PVC は RWO のため replicas は 1 固定。
- イメージは1つ。web と worker は起動コマンドだけ変える。
- Postgres は使わない。DSPy の最適化を複数 Pod(k8s Job)で並列実行し、結果を同時に書き込む必要が出た時点で Postgres へ移行する。データアクセスは SQLAlchemy Core で書き、移行時の変更をキューの置き換えと接続先の変更に限定する。
- htmx と ECharts は `static/` に同梱し、CDN に依存しない。
- `opentelemetry-instrumentation-fastapi` でトレースを SigNoz に送る。送信先は環境変数 `OTEL_EXPORTER_OTLP_ENDPOINT`。

### 取り込み元

- 取り込むファイルをブラウザで登録する(何個でもよい。取り込み元の種別は区別しない)。FileHandle は IndexedDB に保存する。
- 取り込む前に、名前・サイズ・最終更新日時が同じスナップショットが残っていないかを `GET /api/snapshots/lookup` で問い合わせる。残っていれば、読み込みをスキップしてそれを開くか、取り込み直すかを選べる。中身のハッシュではなくメタデータで判定するのは、ファイルを読まずに判定するため。
- File System Access API は secure context が必要。`http://localhost:8054` でアクセスする前提(LB の IP `172.19.0.x` 経由では使えない)。
- 対応していないブラウザでは `<input type=file>` で代替する。

## 画面

| パス | 内容 |
|---|---|
| `/` | 登録済みファイルの一覧(サイズ・更新日時・取り込み済みか)、ファイルの登録、各ファイルの取り込みボタンのみ。押すと進捗表示に切り替わり、1秒ごとにジョブ状態を問い合わせる。完了したら `HX-Redirect` で `/s/{sid}/acceptance` へ |
| `/s/{sid}/acceptance` | 補完の採用率。期間・モデルで絞り込み。日別推移(折れ線)、モデル別・順位別(棒)、文書別(表) |
| `/s/{sid}/records/{table}` | レコード一覧。プロンプト・候補の LIKE 検索、ページ送り。対象: completions / code_actions / character_updates / quality_reviews / quality_findings |
| `/s/{sid}/records/{table}/{id}` | 詳細。プロンプト、生の応答、候補(selected・confidence)、キャラ更新の各項目(applied・skip_reason) |

ヘッダーにスナップショット切り替え欄を置く。`sid` を URL に含めるので、URL から同じ状態を再現できる。

## エンドポイント

- `GET /api/snapshots/lookup?name=&size=&mtime=`: 同じファイルの取り込み済みスナップショット(残っているもののうち最新)。無ければ `{"snapshot": null}`
- `POST /api/imports`(multipart: `file`, `file_mtime`): 202 と `import_id` を返し、Huey にジョブを投入する
- `GET /imports/{id}/status`: 進捗表示用の HTML 断片(取り込み中 / 失敗+理由+再試行ボタン / 完了なら `HX-Redirect`)
- `GET /api/s/{sid}/acceptance?by=day|model|rank|document&from=&to=&model=`: ECharts 用 JSON
- HTML ページ: 上記「画面」の各パス

## 取り込みジョブ(ETL)

1. アップロードを `uploads/` の一時ファイルに保存する。
2. `PRAGMA integrity_check` が `ok` を返すこと、`refinery_schema_history` から最大バージョンを取得できることを確認する。
3. `snapshots` テーブルにファイルの指紋(名前・サイズ・最終更新日時)・スキーマバージョン・取り込み日時(UTC)を記録する。
4. FlightRecorder の各テーブル(completions, completion_candidates, code_actions, code_action_candidates, character_updates, character_update_sections, quality_reviews, quality_findings)を `fr_<table>` に **snapshot_id 付きで**コピーする。旧スキーマで存在しない列(`confidence` など)は NULL。
5. 全体で最新10件のスナップショットだけを残し、それより古いものは行ごと削除する。
6. 一時ファイルを削除する。

手順 3〜5 は1つのトランザクションで行う。

## 採用率の定義

- **リクエスト単位(主指標):** `selected` の候補が1つ以上ある completions 数 ÷ completions 総数
- **順位別:** その rank で `selected` の候補数 ÷ その rank の候補総数

`selected` は、挿入された文字列が候補と完全一致したときだけ立つ(`mark_selected_completion`)。採用後に手直しした場合や、同じ文書で別の補完に上書きされた場合は未採用になる。画面には**下限値**であることを明記する。

## エラー処理

| 状況 | 対応 |
|---|---|
| File System Access API 非対応 | `<input type=file>` を表示 |
| ファイルの読み取り許可が切れている | 取り込みボタンのクリック内で `requestPermission()` を呼ぶ |
| アップロードが 256MB 超 | 413 |
| FlightRecorder の DB でない / `integrity_check` 失敗 | ジョブを `failed` にし、理由と再試行ボタンを表示。自動再試行はしない |
| 対応より新しいスキーマ(V6 以降) | 既知の列だけ取り込み、「V6 未対応」と表示 |
| ETL 途中で worker が落ちた | トランザクションなので途中の行は残らない。worker 起動時に `running` のまま残ったジョブを `failed` にする |

## app.db のスキーマ管理

app.db の中身はすべて FlightRecorder の DB から再生成できる派生データ。マイグレーション機構は入れず、起動時にスキーマのハッシュを比べ、変わっていたら作り直す。

テーブル:
- `imports`(id, file_name, file_size, file_mtime, status[queued/running/done/failed], error, snapshot_id, created_at, updated_at)
- `snapshots`(id, file_name, file_size, file_mtime, schema_version, imported_at)
- `fr_*`(元テーブルの列 + `snapshot_id`。主キーは `(snapshot_id, id)`)
- `meta`(schema_hash)

## テスト(pytest)

- **テスト用 DB:** `lsp/migrations/V*.sql` を順に適用し、`refinery_schema_history` に適用履歴を書き込んで作る。V1〜V5 版と V1〜V4 版(`confidence` なし)の2種類。
- **ETL:** 行数、NULL 補完、壊れたファイルで失敗すること、古いスナップショットの削除。
- **採用率:** 値が分かっているデータでリクエスト単位・順位別を検証。
- **API:** `TestClient` と Huey の `immediate=True` で、アップロード → 完了 → リダイレクトまで。
- **ブラウザ部分**(File System Access API)は手動確認。手順を README に書く。

## ディレクトリ構成

```
shinasadame/
├── pyproject.toml / uv.lock / Dockerfile / README.md
├── src/shinasadame/
│   ├── app.py          # FastAPI 生成、ルーティング
│   ├── config.py       # DATA_DIR, OTLP endpoint
│   ├── db.py           # SQLAlchemy Core engine(WAL)、app.db スキーマ
│   ├── tasks.py        # SqliteHuey、取り込みジョブ
│   ├── etl.py          # FlightRecorder DB の検証と app.db へのコピー
│   ├── analysis/acceptance.py
│   ├── records.py
│   ├── telemetry.py
│   ├── templates/
│   └── static/         # htmx, echarts を同梱
├── tests/
└── k8s/                # kustomization, namespace, pvc, deployment(web+worker), service
```

Cargo workspace の members には入れない。ローカル開発は `DATA_DIR=.data` で `uv run uvicorn …` と `uv run huey_consumer …` を起動する。
