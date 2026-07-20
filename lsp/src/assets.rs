//! `data/` 配下のアセット(プロンプト等)への透過的なアクセス。
//!
//! アセットの実体は「ビルド時に埋め込んだ内容」と「実行ファイル隣の `data/` に置いた
//! 差し替えファイル」の二層だが、その使い分けはこのモジュール内に閉じ込め、呼び出し側は
//! 単一の入口 [`load`] だけを使う(下層の仕組みを意識しない)。

use rust_embed::Embed;

/// ビルド時にリポジトリ直下 `data/` を埋め込む rust-embed アセット。
/// `#[folder]` は crate manifest(`lsp/`)基準で解決される。
#[derive(Embed)]
#[folder = "../data/"]
struct Embedded;

/// `data/<name>` の中身を文字列で返す。
///
/// 実行ファイル隣の `data/<name>` があればそれを優先し(再ビルド不要でプロンプトを
/// 差し替える dev/配布用フック)、無ければビルド時に埋め込んだ内容を使う。
/// どちらにも見つからなければ `None`(欠如時の扱いは呼び出し側に委ねる)。
#[cfg(not(debug_assertions))]
pub(crate) fn load(name: &str) -> Option<String> {
    Embedded::get(name).map(|f| String::from_utf8_lossy(f.data.as_ref()).into_owned())
}

#[cfg(debug_assertions)]
pub(crate) fn load(name: &str) -> Option<String> {
    from_disk(name).or_else(|| {
        Embedded::get(name).map(|f| String::from_utf8_lossy(f.data.as_ref()).into_owned())
    })
}

/// 実行ファイルと同じディレクトリの `data/<name>` を読む。無ければ `None`。
#[cfg(debug_assertions)]
fn from_disk(name: &str) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    std::fs::read_to_string(exe.parent()?.join("data").join(name)).ok()
}
