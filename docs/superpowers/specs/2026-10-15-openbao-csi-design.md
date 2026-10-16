# OpenBao CSI Driver 導入 設計

日付: 2026-10-15

## 目的

LLM の API キー(`GEMINI_API_KEY` / `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `XAI_API_KEY`)を、
手動作成の k8s Secret `replay-keys` から、外部ホストの OpenBao(`https://bao.talos208.dev`)で
管理する方式に移す。`replay` 以外のワークロードが LLM を呼ぶときも同じキーを使えるようにする。

## 前提

- OpenBao は k8s クラスタ外(bao.talos208.dev / 192.168.0.39)で稼働している。証明書は公開 CA で、追加の CA 設定は不要。
- k8s クラスタは開発マシン上のコンテナ型(Docker Desktop / kind / minikube 等)。OpenBao からクラスタの API サーバーへは到達できない前提で設計する。
- OpenBao 側の読み取り用アカウント(ポリシー・ロール)は未作成。OpenBao 本体はこちらでは操作せず、手順スクリプトを用意する。
- OpenBao 専用の「CSI driver」は存在しない。Secrets Store CSI Driver(汎用)と OpenBao CSI Provider(Vault 互換)の組み合わせを使う。

## 構成と分担

| 層 | 内容 | 管理場所 |
|---|---|---|
| OpenBao(外部) | KV v2 のキー、jwt auth のマウント・ポリシー・ロール | 手順スクリプトを `docs/` に置く |
| クラスタ(一度だけ) | Secrets Store CSI Driver と OpenBao CSI Provider を `helm install` | values ファイルと手順をリポジトリに置く |
| アプリ(Skaffold) | `SecretProviderClass` と `replay` Deployment の変更 | `shinasadame/k8s/base/` |

ドライバーは Skaffold の管理対象に含めない。`skaffold delete` や `dev` 終了時のクリーンアップで消えないようにするため。

## データの流れ

1. `replay` Pod の起動時に kubelet が CSI Driver を呼ぶ。
2. Provider が Pod の ServiceAccount トークンで `bao.talos208.dev` の jwt auth にログインする。
3. KV `secret/fifty-four` を読み、Driver がボリュームに書く。
4. `secretObjects` で k8s Secret `replay-keys` に同期する(`replay` の `envFrom` は変更不要)。

## OpenBao 側の設定

- KV v2: マウント `secret`、パス `fifty-four`、キーは上記 4 つを直接置く。
- ポリシー `fifty-four-llm-read`: `secret/data/fifty-four` の読み取りのみ。
- jwt auth をマウント `jwt-fifty-four` に作る。
- ロール `fifty-four-llm`: `bound_claims` で許可する ServiceAccount を列挙する(最初は `replay` のみ)。他のワークロードで使うときは 1 行足す。

### 公開鍵の登録

`kubectl get --raw /openid/v1/jwks` の JWKS を PEM に変換して `jwt_validation_pubkeys` に登録する。
OpenBao からクラスタへ到達できないため `oidc_discovery_url` は使わない。
クラスタを作り直すと署名鍵が変わるので、再登録が必要になる。手順スクリプトは再実行できる形にする。

## 要検証事項(最大のリスク)

OpenBao CSI Provider は `auth/<mount>/login` に `{role, jwt}` を POST する。kubernetes auth 用の実装だが、
jwt auth のログイン API も同じ形式のため、`SecretProviderClass` の `vaultKubernetesMountPath: jwt-fifty-four`
を jwt マウントに向けても動く見込みである。ただし公式にうたわれた使い方ではない。

**実装計画の最初のステップでこの経路が通ることを確認する。** 通らない場合は設計を見直す
(例: kubernetes auth に切り替え、OpenBao からクラスタへの到達経路を用意する)。

## エラー時の挙動

現状は `replay-keys` が無くても `replay` が起動する(`optional: true`)。CSI 経由にすると、
マウントに失敗した Pod は `ContainerCreating` のまま止まる。OpenBao 停止中や認証設定の誤りが
起動失敗として見えるようになる。これは許容する(auth エラーに気づくのが遅れるより良い)。

手動作成の `replay-keys` は、同名の同期 Secret に置き換える前に削除する。

## 完了条件(検証手順)

1. 外部から ServiceAccount トークンで `bao write auth/jwt-fifty-four/login` が成功し、トークンが得られる。
2. 手動の `replay-keys` Secret を削除する。
3. `skaffold run -p replay` で `replay` が `Running` になる。
4. `replay-keys` が CSI で同期され、Pod の環境変数に 4 つのキー名がある(値は表示しない)。
5. `replay` の Pod を削除して再作成しても、同じ結果になる。

## スコープ

作るもの:

- OpenBao 側の設定手順(`docs/`)
- `helm` の values と導入手順
- `SecretProviderClass` と `replay` Deployment の変更
- `shinasadame/README.md` の更新(Secret の作り方を置き換える)

作らないもの:

- ドライバーの Skaffold / kustomize 統合
- `shinasadame` 側の Secret 化(必要なキーがない)
