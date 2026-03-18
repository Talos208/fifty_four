use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    Normal,
    InBracket,
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
}

impl CachedLinderaToken {
    pub const EOT: &'static CachedLinderaToken = &CachedLinderaToken {
        details: vec![],
        byte_start: usize::MAX,
        byte_end: usize::MAX,
        tag: TokenStatus::Normal,
    };
}

#[derive(Debug, Clone, PartialEq)]
pub struct LineData {
    pub text: String,
    pub tokens: Vec<CachedLinderaToken>,
}

impl FromStr for LineData {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self {
            text: text.to_string(),
            tokens: Vec::new(),
        })
    }
}

impl LineData {
    #[allow(dead_code)]
    pub fn surface(&self, ptr: &CachedLinderaToken) -> &str {
        &self.text[ptr.byte_start..ptr.byte_end].as_ref()
    }
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
