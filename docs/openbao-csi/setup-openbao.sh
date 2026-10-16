#!/usr/bin/env bash
# OpenBao 側の設定(再実行できる)。クラスタ作り直し後の公開鍵の再登録にも使う。
# 要: bao, kubectl, python(cryptography)。事前に BAO_ADDR を設定し、管理権限で `bao login` しておく。
#   export BAO_ADDR=https://bao.talos208.dev
#   SAS=replay,other-sa bash docs/openbao-csi/setup-openbao.sh
set -euo pipefail

NS=${NS:-shinasadame}
SAS=${SAS:-replay}            # 許可する ServiceAccount(カンマ区切り、NS 内)
if [ -z "$(printf '%s' "$SAS" | tr -d ', ')" ]; then
  echo "SAS が空(許可する ServiceAccount を 1 つ以上指定する)" >&2
  exit 1
fi
MOUNT=jwt-fifty-four
ROLE=fifty-four-llm
POLICY=fifty-four-llm-read
here=$(cd "$(dirname "$0")" && pwd)

# KV v2 が secret/ にあること
bao secrets list -format=json | python -c '
import json, sys
d = json.load(sys.stdin)
assert "secret/" in d and (d["secret/"].get("options") or {}).get("version") == "2", "secret/ が KV v2 でない"
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
# Git Bash が /openid/... を Windows パスに書き換えるのを防ぐ(この 2 行だけ。python に渡す $here は変換されたままにする)。Linux/macOS では無害。
issuer=$(MSYS_NO_PATHCONV=1 kubectl get --raw /.well-known/openid-configuration \
  | python -c 'import json,sys; print(json.load(sys.stdin)["issuer"])')
pems=$(MSYS_NO_PATHCONV=1 kubectl get --raw /openid/v1/jwks | python "$here/jwks_to_pem.py")
bao write "auth/$MOUNT/config" bound_issuer="$issuer" jwt_validation_pubkeys="$pems"

# トークンの aud は実際に発行したトークンから読む(推測しない)
# aud はクラスタ共通なので、namespace が未作成でも使える default namespace で発行する
aud=$(kubectl -n default create token default \
  | python -c '
import base64, json, sys
p = sys.stdin.read().strip().split(".")[1]
aud = json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4)))["aud"]
print(aud[0])
')

payload=$(mktemp)
trap 'rm -f "$payload"' EXIT
NS="$NS" SAS="$SAS" POLICY="$POLICY" AUD="$aud" python - >"$payload" <<'EOF'
import json, os, sys
subs = [f"system:serviceaccount:{os.environ['NS']}:{sa}" for sa in (s.strip() for s in os.environ["SAS"].split(",")) if sa]
if not subs:
    sys.exit("SAS が空(許可する ServiceAccount を 1 つ以上指定する)")
print(json.dumps({
    "role_type": "jwt",
    "user_claim": "sub",
    "bound_audiences": [os.environ["AUD"]],
    "bound_claims": {"sub": subs},
    "token_policies": [os.environ["POLICY"]],
    "token_ttl": "1h",
}))
EOF
# ネイティブの bao.exe は /tmp 形式のパスを @payload として読めないので、Git Bash では Windows パスに変換して渡す
payload_arg=$payload
if command -v cygpath >/dev/null 2>&1; then payload_arg=$(cygpath -w "$payload"); fi
bao write "auth/$MOUNT/role/$ROLE" @"$payload_arg"

echo "完了: issuer=$issuer aud=$aud"
