"""画面表示用の整形。"""

from dataclasses import dataclass
from pathlib import PurePosixPath
from urllib.parse import unquote, urlsplit


@dataclass(frozen=True)
class DocName:
    name: str  # ファイル名
    folder: str  # 親フォルダ名(同名ファイルの見分け用)
    path: str  # デコードしたフルパス


def doc_name(uri: str | None) -> DocName | None:
    """LSP の document_uri(`file:///C:/…/%E5%8E%9F%E7%A8%BF.txt`)を、日本語のまま読める形にする。

    URI は LSP から受け取ったまま記録されていて、日本語はパーセントエンコードされている。
    `file:` 以外(未保存のバッファなど)は、デコードした文字列をそのまま名前にする。
    """
    if not uri:
        return None
    parts = urlsplit(uri)
    if parts.scheme != "file":
        text = unquote(uri)
        return DocName(name=text, folder="", path=text)
    path = unquote(parts.path)
    # Windows のドライブ付きパス(/C:/…)は先頭の "/" を落とす
    if len(path) >= 3 and path[0] == "/" and path[2] == ":":
        path = path[1:]
    p = PurePosixPath(path)
    return DocName(name=p.name or path, folder=p.parent.name, path=path)
