"""tools/prune_images.py のテスト。docker は呼ばず、選び方(`select_removals`)だけを確かめる。"""

import pytest

import prune_images as pi


def image(created, *tags):
    return (created, list(tags))


def test_keeps_newest_images_and_removes_the_rest():
    images = {
        "sha256:a": image("2026-11-01T00:00:00Z", "AAAAAAA"),
        "sha256:b": image("2026-11-03T00:00:00Z", "BBBBBBB"),
        "sha256:c": image("2026-11-02T00:00:00Z", "CCCCCCC"),
        "sha256:d": image("2026-11-04T00:00:00Z", "DDDDDDD"),
    }
    assert pi.select_removals(images, keep=2) == ["CCCCCCC", "AAAAAAA"]


def test_counts_images_not_tags():
    # 1 つのイメージに 2 つのタグが付いていても、1 個として数える
    images = {
        "sha256:new": image("2026-11-04T00:00:00Z", "latest", "NEWTAG1"),
        "sha256:old": image("2026-11-01T00:00:00Z", "OLDTAG1", "OLDTAG2"),
    }
    assert pi.select_removals(images, keep=1) == ["OLDTAG1", "OLDTAG2"]


def test_removes_all_tags_of_an_old_image_never_a_subset():
    images = {
        "sha256:new": image("2026-11-04T00:00:00Z", "N"),
        "sha256:old": image("2026-11-01T00:00:00Z", "x", "y", "z"),
    }
    assert pi.select_removals(images, keep=1) == ["x", "y", "z"]


def test_nothing_to_remove_when_within_keep():
    images = {"sha256:a": image("2026-11-01T00:00:00Z", "A")}
    assert pi.select_removals(images, keep=3) == []
    assert pi.select_removals({}, keep=3) == []


def test_untagged_images_are_not_targets():
    images = {
        "sha256:new": image("2026-11-04T00:00:00Z", "N"),
        "sha256:old": image("2026-11-01T00:00:00Z", "<none>"),
    }
    assert pi.select_removals(images, keep=1) == []


def test_keep_must_be_at_least_one():
    with pytest.raises(SystemExit):
        pi.main(["--keep", "0", "--dry-run"])
