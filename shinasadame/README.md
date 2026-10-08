# shinasadame(品定め)

fifty_four の FlightRecorder(debug ビルドの LSP が書く SQLite)を取り込み、閲覧・分析する Web アプリ。
名前は源氏物語「帚木」の「雨夜の品定め」から。設計は
[`docs/superpowers/specs/2026-10-06-shinasadame-design.md`](../docs/superpowers/specs/2026-10-06-shinasadame-design.md)。

FlightRecorder の DB は LSP の所有物で、shinasadame は**読むだけ**(書き戻さない)。

## ローカル開発

web と worker の 2 プロセスを、同じ `DATA_DIR` で起動する。**worker(`huey_consumer`)を起動しないと、
取り込みは「待機中」のまま進まない**(取り込みジョブは worker が処理する。待機が 10 秒を超えると画面にも警告が出る)。
worker を後から起動すれば、待機中のジョブはそのまま処理される。`DATA_DIR` の既定は実行ディレクトリの
`.data` なので、2 つのターミナルとも `shinasadame/` で起動するなら設定は要らない。別の場所を使うときは、
**両方のターミナルで**設定する(環境変数はシェルごとに別)。

PowerShell:

```powershell
cd shinasadame
uv sync
# $env:DATA_DIR = ".data"   # 既定と違う場所を使うときだけ(web / worker の両方で)
uv run uvicorn shinasadame.app:app --port 8054
```

```powershell
# 別の PowerShell で
cd shinasadame
uv run huey_consumer shinasadame.tasks.huey -w 1 -k thread
```

sh / bash:

```sh
cd shinasadame
uv sync
# export DATA_DIR=.data   # 既定と違う場所を使うときだけ(web / worker の両方で)
uv run uvicorn shinasadame.app:app --port 8054
# 別のターミナルで
uv run huey_consumer shinasadame.tasks.huey -w 1 -k thread
```

`http://localhost:8054` を **Chrome / Edge** で開く(File System Access API は secure context が必要。
`localhost` は secure context として扱われるが、`172.19.0.x` などの IP 直打ちでは使えない)。

## テスト

PowerShell と sh で共通:

```
uv run pytest
```

テスト用の FlightRecorder DB は `lsp/migrations/V*.sql` を実際に適用して作る(V1〜V5 版と V1〜V4 版)。
別のディレクトリのマイグレーションで試したいときは `FR_MIGRATIONS_DIR` で差し替える。

```powershell
$env:FR_MIGRATIONS_DIR = "C:\path\to\migrations"; uv run pytest
```

```sh
FR_MIGRATIONS_DIR=/path/to/migrations uv run pytest
```

## k8s(Docker Desktop の kind)

リポジトリのルートで実行する。PowerShell と sh で共通:

```
docker build -t shinasadame:latest shinasadame
kubectl apply -k shinasadame/k8s
```

- `http://localhost:8054` で開く(LoadBalancer が localhost に公開される)
- web と worker は同じ Pod で、PVC(`/data`、512Mi)上の `app.db` / `queue.db` を共有する。PVC が RWO なので replicas は 1 固定
- トレースは `OTEL_EXPORTER_OTLP_ENDPOINT`(`k8s/base/deployment.yaml` では `http://localhost:4317`)へ送る。未設定なら送らない
- イメージ名を変える場合は `k8s/base/deployment.yaml` の 2 箇所(web / worker)を合わせる

### Ingress(Traefik)で公開する

既定は LoadBalancer が uvicorn に直結する構成で、これで足りる。複数のサービスを 1 つの入口にまとめたい
ときだけ、Traefik 経由に切り替える。ingress-nginx は終了予定のため使わない。

手順 1 は PowerShell と sh で共通:

```
# 1. Service を ClusterIP にして Ingress を足したオーバーレイを適用する
#    (先にやる: 直結の LoadBalancer が持っている 8054 を空けるため)
kubectl apply -k shinasadame/k8s/overlays/ingress
```

手順 2 は行継続の記号だけが違う(PowerShell はバッククォート、sh はバックスラッシュ)。
クラスタに Ingress コントローラが無い場合のみ実行する。localhost:8054 に公開される。

```powershell
# 2. Traefik を入れる
helm install traefik traefik --repo https://traefik.github.io/charts `
  --namespace traefik --create-namespace -f shinasadame/k8s/traefik-values.yaml
```

```sh
# 2. Traefik を入れる
helm install traefik traefik --repo https://traefik.github.io/charts \
  --namespace traefik --create-namespace -f shinasadame/k8s/traefik-values.yaml
```

- `http://shinasadame.localhost:8054` で開く。`localhost` と `*.localhost` は HTTP でも secure context
  として扱われ、File System Access API が使える。`shinasadame.local` のような**独自ホスト名は TLS が
  無いと取り込み画面が動かない**ので使わない
- Traefik はリクエスト本文の上限を既定で持たないので、256MB のアップロードにも設定は要らない
  (上限はアプリ側の 413 だけ)。長い転送で切れる場合は `traefik-values.yaml` で
  `ports.web.transport.respondingTimeouts` を調整する
- 直結に戻すときは `kubectl apply -k shinasadame/k8s`(Service が LoadBalancer に戻る)。
  Ingress リソースは `kubectl delete ingress shinasadame -n shinasadame` で消す
- 8054 は Traefik が使うので、直結の LoadBalancer とは同時に持てない(だから手順 1 を先にやる)

## 手動確認(ブラウザ部分)

File System Access API の部分は自動テストできないので、次の手順で確認する。

1. `http://localhost:8054` を開く。「ファイルが登録されていません。」と表示される
2. 「ファイルを登録」で `fifty_four/target/debug/db/fifty_four.db` を選ぶ。一覧に出て、サイズ・更新日時と「未取り込み」が表示される
3. もう一度「ファイルを登録」で同じファイルを選ぶ → 「登録済みです」と出て、一覧は増えない
4. `fifty_four/db/fifty_four.db`(同名の別ファイル)を登録する → 2 行目として並び、サイズ・更新日時で見分けられる
5. ページを再読み込みしても一覧が残っている(FileHandle が IndexedDB に残っている)
6. 1 行目の「取り込む」を押す。進捗表示に切り替わり、完了すると `/s/{sid}/acceptance` へ移る
7. `/` に戻ると、その行が「取り込み済み #sid」になっている。もう一度「取り込む」を押すと、アップロードせずに
   「取り込み済みです」と「#sid を開く」「取り込み直す」が出る。「取り込み直す」で新しいスナップショットができる
8. LSP で補完を 1 回使って DB を更新してから「取り込む」を押す → 取り込み済みとは判定されず、そのまま取り込まれる
9. ブラウザを閉じて開き直す。一覧は「取り込むときに読み取りの許可を確認します」になり、「取り込む」を押すと
   **許可の確認が出て**、許可すると取り込める(許可は取り込みボタンのクリック内で `requestPermission()` を呼んでいる)
10. FlightRecorder の DB ではないファイルを登録して取り込む → 「取り込みに失敗しました」と理由、再試行ボタンが出る
11. 「登録解除」で一覧から消える(取り込み済みのスナップショットは消えない)
12. Firefox / Safari(File System Access API 非対応)で開くと、「ファイルを登録」の代わりに `<input type=file>` が出る

## 注意

- 取り込めるのは DB 本体のファイルだけ。LSP が起動中で WAL に未反映の書き込みがあると、
  それは含まれない(LSP を止めてから取り込むと確実)
- 採用率の「採用」は、挿入文字列が候補と**完全一致**したときだけ記録される。手直しして採用した場合や、
  同じ文書で別の補完に上書きされた場合は未採用になるので、画面の採用率は**下限値**
- プロンプトテンプレート種別ごとの採用率は未対応(`completions` にテンプレート種別の列がないため)
- スキーマが V6 以降の DB は、V5 までの既知の列だけ取り込み、画面に警告を出す
- スナップショットは、どのファイルから取り込んだかに関係なく、全体で新しい順に 10 件だけ残る
- 「取り込み済み」の判定は、ファイルの名前・サイズ・最終更新日時の一致で行う(中身は読まない)。
  10 件から押し出されて消えたスナップショットは、取り込み済みとはみなさない
