# OpenBao CSI 導入 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** LLM の API キーを外部 OpenBao(`bao.talos208.dev`)の KV から CSI 経由で `replay` Pod に渡し、手動作成の Secret `replay-keys` をやめる。

**Architecture:** Secrets Store CSI Driver と OpenBao CSI Provider をクラスタに一度だけ helm で入れる(Skaffold の管理外)。OpenBao 側は jwt auth(公開鍵を静的に登録)で `replay` の ServiceAccount を認証し、`SecretProviderClass` の `secretObjects` で `replay-keys` に同期する。

**Tech Stack:** helm, Secrets Store CSI Driver, OpenBao CSI Provider, OpenBao (`bao` CLI), kustomize, Skaffold, Python 3(`cryptography`)

**Spec:** `docs/superpowers/specs/2026-10-15-openbao-csi-design.md`

## Global Constraints

- OpenBao: `https://bao.talos208.dev`(公開 CA の証明書。CA 設定は入れない)
- KV v2: マウント `secret`、パス `fifty-four`、キーは `GEMINI_API_KEY` / `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `XAI_API_KEY`
- ポリシー名 `fifty-four-llm-read`(`secret/data/fifty-four` の read のみ)、jwt auth マウント `jwt-fifty-four`、ロール名 `fifty-four-llm`
- k8s の namespace は `shinasadame`、同期先 Secret 名は `replay-keys`(`replay` の `envFrom` を変えない)
- ドライバー(Driver / Provider)は Skaffold・kustomize に含めない
- OpenBao 本体はこちらからは操作しない。`bao` を実行するのは利用者(管理権限の認証情報が要る)
- Secret の値は表示・コミットしない(キー名の確認だけ)
- コミットメッセージの末尾: `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`

## Review Focus

- KV にキーが 1 つでも無い → マウント失敗で Pod が `ContainerCreating` のまま止まる(使わない provider は SPC の `objects` と `secretObjects` の両方から削除する)
- ServiceAccount が `bound_claims` の許可リストに無い → ログイン拒否で Pod が止まる
- クラスタを作り直した → 署名鍵が変わりログイン失敗。`setup-openbao.sh` の再実行で復旧する
- 手動作成の `replay-keys` が残っている → CSI が作る Secret と衝突する。切り替え前に削除する
- OpenBao 側でキーを更新した → ローテーションは有効にしないので、`replay` の Pod 再作成まで反映されない

## File Structure

| ファイル | 責務 |
|---|---|
| `shinasadame/k8s/csi/secrets-store-values.yaml`(新規) | Secrets Store CSI Driver の helm values |
| `shinasadame/k8s/csi/openbao-values.yaml`(新規) | OpenBao CSI Provider の helm values(サーバー・injector は無効) |
| `docs/openbao-csi/jwks_to_pem.py`(新規) | JWKS(stdin)を PEM(カンマ区切り、stdout)に変換する |
| `docs/openbao-csi/setup-openbao.sh`(新規) | OpenBao 側のポリシー・jwt マウント・ロールを作る(再実行可) |
| `docs/openbao-csi/README.md`(新規) | 導入と運用の手順 |
| `shinasadame/k8s/base/replay-serviceaccount.yaml`(新規) | `replay` 専用の ServiceAccount |
| `shinasadame/k8s/base/replay-secretproviderclass.yaml`(新規) | KV と `replay-keys` の対応 |
| `shinasadame/k8s/base/replay-deployment.yaml`(変更) | SA 指定と CSI ボリューム追加 |
| `shinasadame/k8s/base/kustomization.yaml`(変更) | 上の 2 ファイルを resources に追加 |
| `shinasadame/README.md`(変更) | Secret の作り方を置き換える |

---

### Task 1: Driver と Provider をクラスタに入れる

**Files:**
- Create: `shinasadame/k8s/csi/secrets-store-values.yaml`
- Create: `shinasadame/k8s/csi/openbao-values.yaml`

**Interfaces:**
- Produces: CSIDriver `secrets-store.csi.k8s.io`、`kube-system` の DaemonSet(Driver と Provider)、同期 Secret を有効にした Driver。後続タスクは `SecretProviderClass` の `provider` 名をここで確認した値に合わせる。

- [ ] **Step 1: values を書く**

`shinasadame/k8s/csi/secrets-store-values.yaml`:

```yaml
# Secrets Store CSI Driver。secretObjects(k8s Secret への同期)を使うので syncSecret を有効にする。
# ローテーションは使わない(キー更新は replay の Pod 再作成で反映する)。
syncSecret:
  enabled: true
enableSecretRotation: false
```

`shinasadame/k8s/csi/openbao-values.yaml`:

```yaml
# OpenBao CSI Provider だけを入れる。OpenBao サーバーは外部ホストにあるので入れない。
server:
  enabled: false
injector:
  enabled: false
csi:
  enabled: true
```

- [ ] **Step 2: インストールする**

```bash
helm repo add secrets-store-csi-driver https://kubernetes-sigs.github.io/secrets-store-csi-driver/charts
helm repo add openbao https://openbao.github.io/openbao-helm
helm repo update
helm upgrade --install csi-secrets-store secrets-store-csi-driver/secrets-store-csi-driver \
  -n kube-system -f shinasadame/k8s/csi/secrets-store-values.yaml
helm upgrade --install openbao-csi openbao/openbao \
  -n kube-system -f shinasadame/k8s/csi/openbao-values.yaml
```

- [ ] **Step 3: 動作を確認する**

Run: `kubectl -n kube-system get pods` と `kubectl get csidriver`
Expected: Driver と Provider の Pod が `Running`、`secrets-store.csi.k8s.io` が表示される。

Run: `kubectl -n kube-system get ds -o name` と `kubectl -n kube-system logs ds/openbao-csi-provider --tail=5`(DaemonSet 名は前の出力に合わせる)
Expected: Provider が起動している。**Provider が `SecretProviderClass` で名乗る provider 名(`provider:` に書く値)をログまたは公式 README で確認し、Task 3 で使う値としてメモする**(想定は `openbao`)。

- [ ] **Step 4: Commit**

```bash
git add shinasadame/k8s/csi
git commit -m "$(cat <<'EOF'
feat(k8s): Secrets Store CSI Driver と OpenBao CSI Provider の helm values を追加する

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: OpenBao 側の設定手順を作る

**Files:**
- Create: `docs/openbao-csi/jwks_to_pem.py`
- Create: `docs/openbao-csi/setup-openbao.sh`

**Interfaces:**
- Produces: OpenBao のポリシー `fifty-four-llm-read`、jwt マウント `jwt-fifty-four`、ロール `fifty-four-llm`(許可する ServiceAccount は環境変数 `SAS`、既定 `replay`)。

- [ ] **Step 1: 変換スクリプトを書く(失敗するテストを先に)**

Run: `echo '{"keys":[]}' | python docs/openbao-csi/jwks_to_pem.py`
Expected: FAIL(ファイルが無い)

- [ ] **Step 2: 変換スクリプトを書く**

`docs/openbao-csi/jwks_to_pem.py`:

```python
"""JWKS(stdin)の RSA 公開鍵を PEM に変換し、カンマ区切りで stdout に出す。

OpenBao の jwt_validation_pubkeys はカンマ区切りの PEM を受け取る。
要: pip install cryptography
"""
import base64
import json
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.rsa import RSAPublicNumbers


def b64int(s: str) -> int:
    return int.from_bytes(base64.urlsafe_b64decode(s + "=" * (-len(s) % 4)), "big")


def main() -> None:
    keys = [k for k in json.load(sys.stdin)["keys"] if k.get("kty") == "RSA"]
    if not keys:
        sys.exit("JWKS に RSA 鍵がない(ES256 など別の方式の場合は変換を拡張する)")
    pems = []
    for k in keys:
        pub = RSAPublicNumbers(b64int(k["e"]), b64int(k["n"])).public_key()
        pems.append(
            pub.public_bytes(
                serialization.Encoding.PEM,
                serialization.PublicFormat.SubjectPublicKeyInfo,
            ).decode()
        )
    print(",".join(pems))


if __name__ == "__main__":
    main()
```

- [ ] **Step 3: 変換を確認する**

Run: `kubectl get --raw /openid/v1/jwks | python docs/openbao-csi/jwks_to_pem.py | head -3`
Expected: `-----BEGIN PUBLIC KEY-----` で始まる PEM が出る。RSA 鍵が無いと表示された場合は、鍵の種類(`kty`)を確認して計画を見直す。

- [ ] **Step 4: 設定スクリプトを書く**

`docs/openbao-csi/setup-openbao.sh`:

```bash
#!/usr/bin/env bash
# OpenBao 側の設定(再実行できる)。クラスタ作り直し後の公開鍵の再登録にも使う。
# 要: bao, kubectl, python(cryptography)。事前に BAO_ADDR を設定し、管理権限で `bao login` しておく。
#   export BAO_ADDR=https://bao.talos208.dev
#   SAS=replay,other-sa bash docs/openbao-csi/setup-openbao.sh
set -euo pipefail

NS=${NS:-shinasadame}
SAS=${SAS:-replay}            # 許可する ServiceAccount(カンマ区切り、NS 内)
MOUNT=jwt-fifty-four
ROLE=fifty-four-llm
POLICY=fifty-four-llm-read
here=$(cd "$(dirname "$0")" && pwd)

# KV v2 が secret/ にあること
bao secrets list -format=json | python -c '
import json, sys
d = json.load(sys.stdin)
assert "secret/" in d and d["secret/"].get("options", {}).get("version") == "2", "secret/ が KV v2 でない"
'

bao policy write "$POLICY" - <<'EOF'
path "secret/data/fifty-four" {
  capabilities = ["read"]
}
EOF

bao auth list -format=json | python -c "
import json, sys
sys.exit(0 if '$MOUNT/' in json.load(sys.stdin) else 1)
" || bao auth enable -path="$MOUNT" jwt

# クラスタの issuer と署名公開鍵(OpenBao からクラスタへは到達できないので静的に登録する)
issuer=$(kubectl get --raw /.well-known/openid-configuration \
  | python -c 'import json,sys; print(json.load(sys.stdin)["issuer"])')
pems=$(kubectl get --raw /openid/v1/jwks | python "$here/jwks_to_pem.py")
bao write "auth/$MOUNT/config" bound_issuer="$issuer" jwt_validation_pubkeys="$pems"

# トークンの aud は実際に発行したトークンから読む(推測しない)
aud=$(kubectl -n "$NS" create token default \
  | python -c '
import base64, json, sys
p = sys.stdin.read().strip().split(".")[1]
aud = json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))["aud"]
print(aud[0])
')

payload=$(mktemp)
trap 'rm -f "$payload"' EXIT
NS="$NS" SAS="$SAS" POLICY="$POLICY" AUD="$aud" python - >"$payload" <<'EOF'
import json, os
subs = [f"system:serviceaccount:{os.environ['NS']}:{sa}" for sa in os.environ["SAS"].split(",")]
print(json.dumps({
    "role_type": "jwt",
    "user_claim": "sub",
    "bound_audiences": [os.environ["AUD"]],
    "bound_claims": {"sub": subs},
    "token_policies": [os.environ["POLICY"]],
    "token_ttl": "1h",
}))
EOF
bao write "auth/$MOUNT/role/$ROLE" @"$payload"

echo "完了: issuer=$issuer aud=$aud"
```

- [ ] **Step 5: 利用者が実行して、ログインを確認する**

Run(利用者。管理権限のトークンが要る):

```bash
export BAO_ADDR=https://bao.talos208.dev
bao login   # 管理権限
bash docs/openbao-csi/setup-openbao.sh
read -rs KEY; printf %s "$KEY" | bao kv put secret/fifty-four GEMINI_API_KEY=-; unset KEY   # 値は履歴に残さず、末尾の改行も付けない(<<< は改行が付くので使わない)
TOKEN=$(kubectl -n shinasadame create token replay)
bao write auth/jwt-fifty-four/login role=fifty-four-llm jwt="$TOKEN"
```

Expected: 最後のコマンドで `token` と `policies` が返る。`replay` ServiceAccount は Task 3 で作るので、この時点では `kubectl create token` が失敗する。その場合は Task 3 Step 1 を先に実行してから戻る。

- [ ] **Step 6: Commit**

```bash
git add docs/openbao-csi
git commit -m "$(cat <<'EOF'
docs: OpenBao 側の設定スクリプトと JWKS 変換を追加する

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: 経路の検証(spike): jwt マウントを CSI Provider で使えるか

**これが計画の関門。ここで失敗したら Task 4 以降に進まず、設計を見直す(spec「要検証事項」)。**

**Files:**
- Create: `shinasadame/k8s/base/replay-serviceaccount.yaml`
- Create: `shinasadame/k8s/base/replay-secretproviderclass.yaml`

**Interfaces:**
- Consumes: Task 1 で確認した provider 名、Task 2 のロール `fifty-four-llm`。
- Produces: ServiceAccount `replay`、`SecretProviderClass` `fifty-four-llm`(同期先 Secret `replay-keys`)。

- [ ] **Step 1: ServiceAccount を書く**

`shinasadame/k8s/base/replay-serviceaccount.yaml`:

```yaml
# replay 専用の ServiceAccount。OpenBao の jwt ロール(fifty-four-llm)がこの名前を許可する。
apiVersion: v1
kind: ServiceAccount
metadata:
  name: replay
```

- [ ] **Step 2: SecretProviderClass を書く**

`shinasadame/k8s/base/replay-secretproviderclass.yaml`(`provider:` は Task 1 で確認した値に合わせる):

```yaml
# OpenBao の KV(secret/fifty-four)を k8s Secret `replay-keys` に同期する。
# 使わない provider のキーは、objects と secretObjects の両方から削除する
# (KV にキーが無いとマウントに失敗して Pod が起動しない)。
apiVersion: secrets-store.csi.x-k8s.io/v1
kind: SecretProviderClass
metadata:
  name: fifty-four-llm
spec:
  provider: openbao
  parameters:
    vaultAddress: https://bao.talos208.dev
    roleName: fifty-four-llm
    # kubernetes auth 用の指定だが、ログイン API が jwt auth と同形式なので jwt マウントを向ける
    vaultKubernetesMountPath: jwt-fifty-four
    objects: |
      - objectName: GEMINI_API_KEY
        secretPath: secret/data/fifty-four
        secretKey: GEMINI_API_KEY
      - objectName: ANTHROPIC_API_KEY
        secretPath: secret/data/fifty-four
        secretKey: ANTHROPIC_API_KEY
      - objectName: OPENAI_API_KEY
        secretPath: secret/data/fifty-four
        secretKey: OPENAI_API_KEY
      - objectName: XAI_API_KEY
        secretPath: secret/data/fifty-four
        secretKey: XAI_API_KEY
  secretObjects:
    - secretName: replay-keys
      type: Opaque
      data:
        - objectName: GEMINI_API_KEY
          key: GEMINI_API_KEY
        - objectName: ANTHROPIC_API_KEY
          key: ANTHROPIC_API_KEY
        - objectName: OPENAI_API_KEY
          key: OPENAI_API_KEY
        - objectName: XAI_API_KEY
          key: XAI_API_KEY
```

- [ ] **Step 3: 手動の replay-keys を退避してから、ふたつを apply する**

```bash
kubectl -n shinasadame get secret replay-keys -o name   # あれば存在する(値は表示しない)
kubectl -n shinasadame delete secret replay-keys --ignore-not-found
kubectl -n shinasadame apply -f shinasadame/k8s/base/replay-serviceaccount.yaml \
  -f shinasadame/k8s/base/replay-secretproviderclass.yaml
```

注意: 実行中の `replay` は `envFrom` で `replay-keys` を読んでいる。削除しても起動済みのプロセスの環境変数は変わらないが、Pod が再作成されるとキー無しで起動する。Task 5 まで完了すれば解消する。

- [ ] **Step 4: 使い捨ての Pod でマウントを確かめる**

```bash
kubectl -n shinasadame apply -f - <<'EOF'
apiVersion: v1
kind: Pod
metadata:
  name: csi-probe
spec:
  serviceAccountName: replay
  containers:
    - name: probe
      image: busybox:1.36
      command: ["sleep", "300"]
      volumeMounts:
        - {name: keys, mountPath: /mnt/keys, readOnly: true}
  volumes:
    - name: keys
      csi:
        driver: secrets-store.csi.k8s.io
        readOnly: true
        volumeAttributes: {secretProviderClass: fifty-four-llm}
EOF
kubectl -n shinasadame wait --for=condition=Ready pod/csi-probe --timeout=90s
kubectl -n shinasadame exec csi-probe -- ls /mnt/keys
kubectl -n shinasadame get secret replay-keys -o jsonpath='{.data}' | python -c 'import json,sys; print(sorted(json.load(sys.stdin)))'
```

Expected: Pod が `Ready`、`ls` に KV に入れたキー名が並び、`replay-keys` のキー名が同じ一覧になる(値は表示しない)。

失敗時: `kubectl -n shinasadame describe pod csi-probe` の events と `kubectl -n kube-system logs ds/openbao-csi-provider` を見る。`provider` 名の誤り、`bound_audiences` の不一致、ログイン API の形式の不一致を区別する。**jwt マウントが受け付けられない場合は、ここで止めて利用者に報告する。**

- [ ] **Step 5: 後始末と Commit**

```bash
kubectl -n shinasadame delete pod csi-probe
git add shinasadame/k8s/base/replay-serviceaccount.yaml shinasadame/k8s/base/replay-secretproviderclass.yaml
git commit -m "$(cat <<'EOF'
feat(k8s): replay の ServiceAccount と OpenBao 用 SecretProviderClass を追加する

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: replay の Deployment を CSI に切り替える

**Files:**
- Modify: `shinasadame/k8s/base/replay-deployment.yaml:1-2,16-32`
- Modify: `shinasadame/k8s/base/kustomization.yaml`

**Interfaces:**
- Consumes: Task 3 の ServiceAccount `replay` と SecretProviderClass `fifty-four-llm`。

- [ ] **Step 1: 失敗する確認を先に実行する**

Run: `kubectl kustomize shinasadame/k8s | grep -c 'kind: SecretProviderClass'`
Expected: `0`(まだ kustomization に入っていない)

- [ ] **Step 2: kustomization に追加する**

`shinasadame/k8s/base/kustomization.yaml` の resources 末尾に追加:

```yaml
  - replay-serviceaccount.yaml
  - replay-secretproviderclass.yaml
```

- [ ] **Step 3: Deployment を変更する**

`shinasadame/k8s/base/replay-deployment.yaml` の冒頭 2 行のコメントを置き換える:

```yaml
# replay サービス(fifty_four_lsp --replay)。shinasadame の Pod とは別の Pod にする。
# API キーは OpenBao から CSI 経由で取得し、Secret `replay-keys` に同期する(SecretProviderClass fifty-four-llm)。
```

`spec.template.spec` の先頭に `serviceAccountName` を追加し、`containers` と同じ階層に `volumes` を追加する:

```yaml
    spec:
      serviceAccountName: replay
      containers:
        - name: replay
          # ...(既存のまま)...
          envFrom:
            - secretRef:
                name: replay-keys # CSI が同期する。取得できなければ Pod は起動しない(optional にしない)
          volumeMounts:
            - name: llm-keys
              mountPath: /mnt/llm-keys # このマウントがあるときだけ replay-keys が同期される
              readOnly: true
          # readinessProbe / livenessProbe は既存のまま
      volumes:
        - name: llm-keys
          csi:
            driver: secrets-store.csi.k8s.io
            readOnly: true
            volumeAttributes:
              secretProviderClass: fifty-four-llm
```

- [ ] **Step 4: 確認する**

Run: `kubectl kustomize shinasadame/k8s | grep -E 'kind: SecretProviderClass|serviceAccountName: replay|secretProviderClass: fifty-four-llm'`
Expected: 3 行とも出る。

Run: `kubectl kustomize shinasadame/k8s | kubectl apply --dry-run=server -f -`
Expected: エラーなし。

- [ ] **Step 5: Commit**

```bash
git add shinasadame/k8s/base
git commit -m "$(cat <<'EOF'
feat(k8s): replay の API キーを OpenBao の CSI 経由で取得する

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: 切り替えの検証と文書の更新

**Files:**
- Create: `docs/openbao-csi/README.md`
- Modify: `shinasadame/README.md`(「プロンプト試行」の手順 2、「Skaffold で更新する」の Secret の注記)
- Modify: `skaffold.yaml:10`

- [ ] **Step 1: 配備して確認する**

```bash
skaffold run -p replay
kubectl -n shinasadame rollout status deploy/replay
kubectl -n shinasadame exec deploy/replay -- env | cut -d= -f1 | grep -E '_API_KEY$' | sort
```

Expected: `rollout` が成功し、KV に入れたキー名だけが出る(値は表示しない)。

- [ ] **Step 2: Pod を再作成しても同じことを確認する**

```bash
kubectl -n shinasadame delete pod -l app=replay
kubectl -n shinasadame rollout status deploy/replay
kubectl -n shinasadame exec deploy/replay -- env | cut -d= -f1 | grep -E '_API_KEY$' | sort
```

Expected: Step 1 と同じキー名の一覧。

- [ ] **Step 3: 失敗の見え方を確認する**

ロールの許可 SA を一時的に外す代わりに、存在しない SA 名で `SAS=other bash docs/openbao-csi/setup-openbao.sh` を実行して Pod を再作成し、`ContainerCreating` のまま止まり、events にログイン拒否が出ることを確認する。確認後に `bash docs/openbao-csi/setup-openbao.sh`(既定 `replay`)で戻し、Pod を再作成して復旧を確かめる。

- [ ] **Step 4: 文書を更新する**

`docs/openbao-csi/README.md` に次を書く: 構成図(spec のデータの流れ)、初回導入(Task 1 の `helm` コマンド → Task 2 の `setup-openbao.sh` と `bao kv put` → `skaffold run -p replay`)、ワークロード追加時の手順(`SAS` に追加して `setup-openbao.sh` を再実行、その ServiceAccount を持つ Pod で同じ `SecretProviderClass` を使う)、クラスタ作り直し後の復旧(`setup-openbao.sh` の再実行)、使わない provider のキーを SPC から外す手順、キー更新の反映(`kubectl -n shinasadame rollout restart deploy/replay`)。

`shinasadame/README.md` の「プロンプト試行」手順 2 を、`kubectl create secret` から「OpenBao の `secret/fifty-four` にキーを入れる(`docs/openbao-csi/README.md` を参照)」に置き換える。「Skaffold で更新する」の「API キーの Secret `replay-keys` は管理対象外(手動で作る…)」を、「API キーは OpenBao から CSI で取得する。Driver と Provider は Skaffold の管理外(`docs/openbao-csi/README.md`)」に置き換える。

`skaffold.yaml:10` の「API キーの Secret(replay-keys)は管理対象外で、手動で作る。」を「API キーは OpenBao から CSI で取得する(Driver は管理対象外。docs/openbao-csi/README.md)。」に置き換える。

- [ ] **Step 5: Commit**

```bash
git add docs/openbao-csi/README.md shinasadame/README.md skaffold.yaml
git commit -m "$(cat <<'EOF'
docs: OpenBao CSI の導入手順を追加し、Secret の手動作成の記述を置き換える

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

- **Spec coverage:** 構成と分担(Task 1・2・4)、データの流れ(Task 3・4)、OpenBao 側の設定(Task 2)、公開鍵の登録(Task 2)、要検証事項(Task 3)、エラー時の挙動(Task 4 の `optional` 削除、Task 5 Step 3)、完了条件 1〜5(Task 2 Step 5、Task 3 Step 3、Task 5 Step 1・2)、README 更新(Task 5)。漏れなし。
- **未確定の値:** `provider:` の名前だけが Task 1 の確認に依存する。Task 3 の SPC は想定値 `openbao` で書いてあり、Task 1 Step 3 の確認結果が違えば置き換える。
- **名前の一貫性:** `fifty-four-llm`(ロール・SPC)、`fifty-four-llm-read`(ポリシー)、`jwt-fifty-four`(マウント)、`replay-keys`(同期 Secret)、`llm-keys`(ボリューム名)は全タスクで同じ。
