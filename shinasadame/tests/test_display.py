from shinasadame.display import DocName, doc_name


def test_percent_encoded_japanese_file_uri_is_decoded():
    uri = (
        "file:///C:/Users/talos/writes/%E9%87%91%E5%89%9B%E4%BB%A3%E8%89%A6%E8%A8%88%E7%94%BB/"
        "00_%E3%83%88%E3%83%B3%E3%83%96%E3%83%AA%E6%B2%88%E6%B2%A1.fable.txt"
    )
    assert doc_name(uri) == DocName(
        name="00_トンブリ沈没.fable.txt",
        folder="金剛代艦計画",
        path="C:/Users/talos/writes/金剛代艦計画/00_トンブリ沈没.fable.txt",
    )


def test_ascii_file_uri():
    assert doc_name("file:///C:/Users/talos/writes/anko0/Draft/00.txt") == DocName(
        name="00.txt", folder="Draft", path="C:/Users/talos/writes/anko0/Draft/00.txt"
    )


def test_posix_file_uri_keeps_leading_slash():
    assert doc_name("file:///home/u/%E5%8E%9F%E7%A8%BF.txt") == DocName(
        name="原稿.txt", folder="u", path="/home/u/原稿.txt"
    )


def test_non_file_uri_and_empty_values_do_not_break():
    assert doc_name("untitled:Untitled-1").name == "untitled:Untitled-1"
    assert doc_name(None) is None
    assert doc_name("") is None
