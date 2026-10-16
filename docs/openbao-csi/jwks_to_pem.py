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
