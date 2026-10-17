# OpenBao から API キーを取得する(Secrets Store CSI)

`replay` の LLM API キーは、k8s の Secret を手で作る代わりに、外部ホストの OpenBao(`https://bao.talos208.dev`)から
Secrets Store CSI Driver 経由で取得する。

## 構成

```
replay Pod (ServiceAccount: replay)
  └ CSI ボリューム(SecretProviderClass: fifty-four-llm)
      │ ① Pod の ServiceAccount トークン(JWT)を渡す
      ▼
  OpenBao CSI Provider(DaemonSet, kube-system)
      │ ② jwt auth(マウント jwt-fifty-four, ロール fifty-four-llm)でログイン
      ▼
  OpenBao(bao.talos208.dev)── ポリシー fifty-four-llm-read ── KV v2 secret/fifty-four
      │ ③ キーを返す
      ▼
  マウント時に k8s Secret `replay-keys` へ同期(secretObjects)→ Deployment が envFrom で参照
```

- OpenBao からクラスタへは到達できないので、クラスタの JWKS 公開鍵を OpenBao に静的に登録する(`setup-openbao.sh`)。
- `SecretProviderClass`(`shinasadame/k8s/base/replay-secretproviderclass.yaml`)は kustomize / DevSpace で配備される。
  Driver と Provider(helm)と OpenBao 側の設定は DevSpace の管理外。

## 初回導入

1. Driver と Provider を入れる(クラスタに 1 度だけ。リポジトリルートで):

   ```bash
   helm repo add secrets-store-csi-driver https://kubernetes-sigs.github.io/secrets-store-csi-driver/charts
   helm repo add openbao https://openbao.github.io/openbao-helm
   helm repo update
   helm upgrade --install csi-secrets-store secrets-store-csi-driver/secrets-store-csi-driver \
     -n kube-system -f shinasadame/k8s/csi/secrets-store-values.yaml
   helm upgrade --install openbao-csi openbao/openbao \
     -n kube-system -f shinasadame/k8s/csi/openbao-values.yaml
   ```

2. OpenBao 側を設定する(`bao` CLI で管理権限ログイン済みであること。要 `python` + `cryptography`):

   ```bash
   export BAO_ADDR=https://bao.talos208.dev
   bash docs/openbao-csi/setup-openbao.sh
   ```

3. キーを KV に入れる。キーをシェル履歴に残さないよう、標準入力から渡す(`key=-` は標準入力を読む)。
   `bao kv put` は秘密全体を上書きするので、複数のキーを入れるときは 1 回のコマンドに全部含める:

   ```bash
   read -rs KEY; printf %s "$KEY" | bao kv put secret/fifty-four GEMINI_API_KEY=-; unset KEY
   ```

   **値の末尾に改行を付けない。** `printf %s` を使う。`echo` やヒアストリング(`<<<`)は末尾に改行を足す。
   CSI は KV の値をそのまま Secret に写すので、改行が混じると HTTP ヘッダに載せられず、LLM 呼び出しが
   `InvalidHeaderValue`(reqwest の `Builder` エラー)で失敗する(下の「トラブルシュート」参照)。

4. 配備する:

   ```bash
   devspace deploy -p replay    # または kubectl apply -k shinasadame/k8s
   kubectl -n shinasadame rollout status deploy/replay
   ```

5. 同期された値の形を確認する(値そのものは表示しない。末尾バイトが `0a` や `0d` でなく、`non-printable` が空ならよい):

   ```bash
   kubectl -n shinasadame get secret replay-keys -o jsonpath='{.data.GEMINI_API_KEY}' | base64 -d | python -c '
   import sys
   b = sys.stdin.buffer.read()
   print("length:", len(b), "last byte:", b[-1:].hex())
   print("non-printable:", [i for i, c in enumerate(b) if c < 0x21 or c > 0x7e])'
   ```

## 現在のキーと provider の追加

`SecretProviderClass` は今 `GEMINI_API_KEY` だけを取得する(KV にあるキーもこれだけ)。
KV に無いキーを SecretProviderClass に書くとマウントに失敗して Pod が起動しないので、先に KV へ入れる。

別の provider(`ANTHROPIC_API_KEY` など)を足す手順:

1. KV に足す。`bao kv patch` は他のキーを残したまま足せる(管理用トークンに `patch` capability が要る)。
   `bao kv put` は秘密全体を上書きするので、足すだけの用途には使わない:

   ```bash
   read -rs KEY; printf %s "$KEY" | bao kv patch secret/fifty-four ANTHROPIC_API_KEY=-; unset KEY
   ```

2. `replay-secretproviderclass.yaml` の `objects` と `secretObjects` の**両方**に、そのキーを足す。
3. 再配備する: `kubectl apply -k shinasadame/k8s`(または `devspace deploy -p replay`)。
4. Pod を再作成する(下の「キーの更新」)。

使わなくなった provider は、同じ 2 箇所から外す。

## Secret `replay-keys` の寿命

`replay-keys` は、CSI ボリュームをマウントする Pod が最初に起動したときに Driver が作る。Driver はマウントした Pod の
ownerReference(Deployment の場合は ReplicaSet)を Secret に写すので、Secret はその ReplicaSet が存在する間残り、
Deployment を削除すると一緒に消える(Pod を作り直しただけでは消えない)。owner の無い単体 Pod
(試験用の素の Pod など)がマウントした場合だけ、Pod ごとの管理になり、その Pod の削除で消える。
Deployment が無い状態で `kubectl get secret replay-keys` が NotFound になるのは正常。手動で作らない(作ると Driver の管理と衝突する)。

## キーの更新

ローテーションは使わない。KV を更新したら Pod を作り直して反映する:

```bash
kubectl -n shinasadame rollout restart deploy/replay
```

注意: ローテーションの反映は**エンドツーエンドでは未検証**。RollingUpdate では旧 Pod と新 Pod が重なり、
Secret の更新より先に新しいコンテナが起動する可能性がある。実際にキーを更新したら、値そのものは表示せず、
ハッシュだけを比べて確認する(例: `kubectl -n shinasadame exec deploy/replay -- sh -c 'printf %s "$GEMINI_API_KEY" | sha256sum'`
と、KV 側の値のハッシュ)。

## トラブルシュート: `InvalidHeaderValue`

LLM 呼び出しが `Reqwest(reqwest::Error { kind: Builder, source: http::Error(InvalidHeaderValue) })`
(`Web call failed ... Reqwest error: builder error`)で失敗するときは、API キーに改行などの不正な文字が混じっている。

1. 上の「初回導入」手順 5 の確認コマンドで、末尾バイト(`0a` = 改行、`0d` = CR)と `non-printable` を見る。
2. 混じっていたら、`printf %s` でキーを入れ直す(手順 3)。
3. Pod を作り直す(`kubectl -n shinasadame rollout restart deploy/replay`)。
4. 手順 5 を再実行し、末尾バイトが文字(`0x21`〜`0x7e`)になったことを確認する。

## ワークロードを足す

1. `SAS` にその ServiceAccount を足して `setup-openbao.sh` を再実行する(ロールの許可リストが置き換わる。既存の分も含めて指定する):

   ```bash
   SAS=replay,other-sa bash docs/openbao-csi/setup-openbao.sh
   ```

2. その ServiceAccount を持つ Pod で、同じ `SecretProviderClass`(`fifty-four-llm`)を CSI ボリュームとして使う。

許可されていない ServiceAccount の Pod は `ContainerCreating` のまま止まり、events に `FailedMount ... failed to login` が出る
(`replay` の Pod には影響しない)。原因は、その ServiceAccount がロールの `bound_claims.sub` に入っていないこと。

## クラスタを作り直したとき

クラスタの署名鍵が変わるので、OpenBao に登録した公開鍵が合わなくなる。Driver と Provider を入れ直した後、
`bash docs/openbao-csi/setup-openbao.sh` を再実行して公開鍵を登録し直す。KV の中身はそのまま使える。

## 注意

- Git Bash では `kubectl get --raw /...` の `/` から始まる引数が Windows パスに変換される。
  `MSYS_NO_PATHCONV=1 kubectl get --raw /openid/v1/jwks` のように付ける(`setup-openbao.sh` は対応済み)。
- OpenBao CSI Provider の chart は `openbao-agent` のサイドカーを同梱するが、この構成では使っていない。
  `csi.agent.enabled=false` で無効にできる(任意の後続作業。未実施)。
