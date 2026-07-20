use log::debug;
use std::fmt::Debug;
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    Normal,
    InBracket,
}

impl Debug for TokenStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenStatus::Normal => write!(f, "Normal"),
            TokenStatus::InBracket => write!(f, "InBracket"),
        }
    }
}

/// Linderaトークンの必要情報をOwned形式でキャッシュするための型。
/// tagはclassify_cached_tokens()によって設定される。
#[derive(Debug, Clone, PartialEq)]
pub struct CachedLinderaToken {
    /// 品詞情報（details[0]="名詞", details[1]="固有名詞", ...）
    pub details: Vec<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    pub tag: TokenStatus,
    // TODO line_noも記録したほうがいいかも
}

#[derive(Debug, Clone, PartialEq)]
pub struct LineData {
    pub text: String,
    pub tokens: Vec<CachedLinderaToken>,
    /// この行の処理を終えた時点での括弧ネスト深さ。None = 未計算/無効化済み。
    /// 前方の全行に依存する累積量のため、編集時は編集行以降を一括で None にすること
    /// (`apply_changes` 参照)。
    pub bracket_depth_after: Option<u32>,
}

impl FromStr for LineData {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        // debug!("LineData::from_str");
        Ok(Self {
            text: text.to_string(),
            tokens: Vec::new(),
            bracket_depth_after: None,
        })
    }
}

impl LineData {
    #[allow(dead_code)]
    pub fn surface(&self, ptr: &CachedLinderaToken) -> &str {
        self.text[ptr.byte_start..ptr.byte_end].as_ref()
    }
}

/// UTF-16 コード単位オフセットをバイトオフセットに変換する。
///
/// LSP の `Position.character` (positionEncoding=utf-16) を Rust 文字列の
/// バイトオフセットへ変換するために使う。
/// オフセットが行末を超える場合は `text.len()` にクランプする。
/// サロゲートペアの中間を指す場合はその文字の直後に丸める
/// (LSP仕様上、正当な Position はペア中間を指さない)。
pub fn utf16_to_byte_offset(text: &str, utf16_offset: usize) -> usize {
    let mut u16_count = 0;
    for (byte_ix, c) in text.char_indices() {
        if u16_count >= utf16_offset {
            return byte_ix;
        }
        u16_count += c.len_utf16();
    }
    text.len()
}

/// 文字列の UTF-16 コード単位数を返す。
///
/// バイトオフセットから LSP の character 値(positionEncoding=utf-16)を
/// 算出する際、`&text[..byte_offset]` を渡して使う。
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// カーソル位置によるcompletion プロンプト分類
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorContext {
    /// 括弧外、」の直後
    AfterClosingBracket,
    /// 括弧外、文末(。)の直後
    AfterSentenceEnd,
    /// 括弧内、」の直前
    BeforeClosingBracket,
    /// 括弧内、空の括弧「」の中
    EmptyBracket,
    /// 括弧内、それ以外
    InBracketOther,
    /// 上記以外のすべて
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    // "a𠮷b": a=1byte/1u16, 𠮷(U+20BB7,サロゲートペア)=4byte/2u16, b=1byte/1u16

    #[test]
    fn test_utf16_to_byte_offset() {
        let s = "a𠮷b";
        assert_eq!(utf16_to_byte_offset(s, 0), 0);
        assert_eq!(utf16_to_byte_offset(s, 1), 1); // aの直後
        assert_eq!(utf16_to_byte_offset(s, 3), 5); // 𠮷の直後
        assert_eq!(utf16_to_byte_offset(s, 4), 6); // bの直後(=末尾)
        // 範囲外は末尾にクランプ
        assert_eq!(utf16_to_byte_offset(s, 100), 6);
        // サロゲートペア中間(u16オフセット2)は文字の直後に丸める
        assert_eq!(utf16_to_byte_offset(s, 2), 5);
        // 空文字列
        assert_eq!(utf16_to_byte_offset("", 5), 0);
    }

    #[test]
    fn test_utf16_len() {
        assert_eq!(utf16_len(""), 0);
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("あいう"), 3); // BMP: 1文字=1u16
        assert_eq!(utf16_len("𠮷"), 2); // サロゲートペア
        assert_eq!(utf16_len("a𠮷b"), 4);
    }
}
