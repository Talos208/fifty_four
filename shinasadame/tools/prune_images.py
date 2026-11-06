"""devspace がビルドした古いイメージのタグを消す。標準ライブラリと docker CLI だけで動く。

devspace はビルドのたびに新しいタグ(ランダムな 7 文字)を付け、古いタグは残る(`skipPush` なので、
どこにも push されず、手元の docker に積もる)。Rust のイメージは 1 つが 150〜200MB あり、放っておくと
数十 GB になる。リポジトリごとに、新しい順に `--keep` 個のイメージだけ残して、残りのタグを外す。

安全のために:
- `docker rmi` を `-f` なしで呼ぶ。使用中のイメージ(コンテナが動いているもの)は、消せずに残る
- 残す数の数え方は「タグ」ではなく「イメージ」(同じイメージに複数のタグが付いていても 1 つと数える)
- 同じイメージに付いたタグは、イメージごと残すか消すかを決める(一部のタグだけ外すことはしない)

使い方: `uv run python tools/prune_images.py [--keep 3] [--dry-run] [リポジトリ名 ...]`
(リポジトリ名の既定は devspace.yaml のイメージ名: fifty-four-replay と shinasadame)
"""

from __future__ import annotations

import argparse
import subprocess
import sys

DEFAULT_REPOSITORIES = ("fifty-four-replay", "shinasadame")
DEFAULT_KEEP = 3


def select_removals(images: dict[str, tuple[str, list[str]]], keep: int) -> list[str]:
    """消すタグ(`repo:tag` に使う、タグだけの文字列)を返す。

    `images` は イメージ ID -> (作成時刻の文字列, タグの一覧)。作成時刻は ISO 8601(同じ表記なら
    文字列の比較で時刻の順になる)。新しい順に `keep` 個のイメージを残し、それより古いイメージのタグを返す。
    `<none>`(タグ無し)は対象にしない。
    """
    newest_first = sorted(images.items(), key=lambda item: item[1][0], reverse=True)
    removals: list[str] = []
    for _image_id, (_created, tags) in newest_first[max(keep, 0) :]:
        removals.extend(tag for tag in tags if tag != "<none>")
    return removals


def _docker(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(["docker", *args], capture_output=True, text=True, check=False)


def list_images(repository: str) -> dict[str, tuple[str, list[str]]]:
    """リポジトリのイメージを、イメージ ID -> (作成時刻, タグの一覧) で返す。"""
    listed = _docker("images", repository, "--no-trunc", "--format", "{{.ID}} {{.Tag}}")
    if listed.returncode != 0:
        raise RuntimeError(listed.stderr.strip() or "docker images に失敗")
    tags_by_id: dict[str, list[str]] = {}
    for line in listed.stdout.splitlines():
        image_id, _, tag = line.partition(" ")
        if image_id:
            tags_by_id.setdefault(image_id, []).append(tag)
    if not tags_by_id:
        return {}
    inspected = _docker("image", "inspect", "--format", "{{.Id}} {{.Created}}", *tags_by_id)
    if inspected.returncode != 0:
        raise RuntimeError(inspected.stderr.strip() or "docker image inspect に失敗")
    images: dict[str, tuple[str, list[str]]] = {}
    for line in inspected.stdout.splitlines():
        image_id, _, created = line.partition(" ")
        if image_id in tags_by_id:
            images[image_id] = (created, tags_by_id[image_id])
    return images


def prune(repository: str, keep: int, dry_run: bool) -> int:
    """1 つのリポジトリを整理して、外したタグの数を返す。"""
    removals = select_removals(list_images(repository), keep)
    removed = 0
    for tag in removals:
        reference = f"{repository}:{tag}"
        if dry_run:
            print(f"[dry-run] docker rmi {reference}")
            removed += 1
            continue
        result = _docker("rmi", reference)
        if result.returncode == 0:
            print(f"removed {reference}")
            removed += 1
        else:
            # 使用中などで消せなかった。止めずに続ける
            print(f"kept    {reference} ({result.stderr.strip().splitlines()[-1:] or ['?']})")
    return removed


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("repositories", nargs="*", default=list(DEFAULT_REPOSITORIES))
    parser.add_argument("--keep", type=int, default=DEFAULT_KEEP, help="リポジトリごとに残すイメージの数")
    parser.add_argument("--dry-run", action="store_true", help="消さずに、消すものを表示する")
    args = parser.parse_args(argv)
    if args.keep < 1:
        parser.error("--keep は 1 以上(0 だと、いま使っているイメージまで消しにいく)")

    total = 0
    for repository in args.repositories:
        try:
            total += prune(repository, args.keep, args.dry_run)
        except (RuntimeError, OSError) as e:  # docker が無い・動いていない
            print(f"{repository}: {e}", file=sys.stderr)
            return 1
    print(f"{'would remove' if args.dry_run else 'removed'} {total} tag(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
