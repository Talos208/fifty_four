use std::fmt::{Debug, Formatter};

use lindera::mode::Mode;
use lindera::tokenizer::TokenizerBuilder;
use tracing::{debug, instrument};

/// 会話ハイライト用のトークナイザ・ユーティリティ
///
/// `lindera` を使って形態素解析を行い、会話テキストをハイライトします。

/// ハイライト用トークンを表す型。
///
/// `start`/`length` はバイト単位のオフセットを想定しています（LSP の semantic tokens 生成時に変換して使います）。
#[derive(Debug, Clone)]
pub struct SemanticToken {
    /// 先頭バイトオフセット
    pub start: u32,
    /// バイト長
    pub length: u32,
    /// トークンの種類（例: "keyword", "string", "function" など）
    pub token_type: u32,
    pub modifier: u32,
}

#[derive(Debug)]
#[repr(u32)]
pub enum SemanticTokenType {
    Comment = 0,
    String = 1,
    Keyword = 2,
    Number = 3,
    Regexp = 4,
    Operator = 5,
    Namespace = 6,
    Type = 7,
    Struct = 8,
    Class = 9,
    Interface = 10,
    Enum = 11,
    TypeParameter = 12,
    Function = 13,
    Method = 14,
    Member = 15,
    Macro = 16,
    Variable = 17,
    Parameter = 18,
    Property = 19,
    Label = 20,

    Undefined = u32::MAX,
}

impl SemanticToken {
    /// 新しいトークンを作成する簡易コンストラクタ
    pub fn new(start: u32, length: u32, token_type: u32, modifier: u32) -> Self {
        Self {
            start,
            length,
            token_type,
            modifier,
        }
    }

    pub fn from_kind(start: u32, length: u32, kind: &str) -> Self {
        let (token_type, modifier) = Self::kind2token(kind);
        Self {
            start,
            length,
            token_type,
            modifier,
        }
    }

    pub fn kind2token(kind: &str) -> (u32, u32) {
        match kind {
            "comment" => (SemanticTokenType::Comment as u32, 0),
            "string" => (SemanticTokenType::String as u32, 0),
            "keyword" => (SemanticTokenType::Keyword as u32, 0),
            "number" => (SemanticTokenType::Number as u32, 0),
            "regexp" => (SemanticTokenType::Regexp as u32, 0),
            "operator" => (SemanticTokenType::Operator as u32, 0),

            "namespace" => (SemanticTokenType::Namespace as u32, 0),
            "type" => (SemanticTokenType::Type as u32, 0),
            "struct" => (SemanticTokenType::Struct as u32, 0),
            "class" => (SemanticTokenType::Class as u32, 0),
            "interface" => (SemanticTokenType::Interface as u32, 0),

            "enum" => (SemanticTokenType::Enum as u32, 0),
            "typeParameter" => (SemanticTokenType::TypeParameter as u32, 0),
            "function" => (SemanticTokenType::Function as u32, 0),
            "method" => (SemanticTokenType::Method as u32, 0),
            "member" => (SemanticTokenType::Member as u32, 0),

            "macro" => (SemanticTokenType::Macro as u32, 0),
            "variable" => (SemanticTokenType::Variable as u32, 0),
            "parameter" => (SemanticTokenType::Parameter as u32, 0),
            "property" => (SemanticTokenType::Property as u32, 0),
            "label" => (SemanticTokenType::Label as u32, 0),

            _ => (
                SemanticTokenType::Undefined as u32,
                SemanticTokenType::Undefined as u32,
            ),
        }
    }
}

// #[derive(Debug)]
pub struct Highlighter {
    tokenizer: lindera::tokenizer::Tokenizer,
}

impl std::fmt::Debug for crate::highlight::Highlighter {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Highlight tokenizer using Lindera")?;
        Ok(())
    }
}

impl Highlighter {
    pub fn new() -> Self {
        // IPADIC を使用する設定でトークナイザを作成
        let tokenizer = TokenizerBuilder::new()
            .unwrap()
            .set_segmenter_mode(&Mode::Normal)
            .set_segmenter_dictionary("embedded://ipadic")
            // .set_segmenter_user_dictionary("")
            .build()
            .expect("failed to create lindera tokenizer");

        Self { tokenizer }
    }

    /// テキストを受け取り、ハイライト用トークン列を返す。
    ///
    /// Lindera を用いて形態素解析を行い、語種に基づくトークン種別を生成します。
    pub fn tokenize(&self, text: impl AsRef<str> + Debug) -> Vec<SemanticToken> {
        let mut tokens = Vec::new();
        // `tokenize` は Result を返すため、expect で処理
        let lindera_tokens = self
            .tokenizer
            .tokenize(text.as_ref())
            .expect("failed to tokenize text");

        for mut token in lindera_tokens {
            let details = token.details();
            let kind = match details[0] {
                "名詞" => Some("keyword"),
                "動詞" => Some("variable"),
                "形容詞" => Some("function"),
                "記号" => {
                    match details.get(1) {
                        Some(&"句点") | Some(&"読点") => Some("comment"),
                        Some(&"括弧閉") => {
                            // TODO 括弧モード開始
                            Some("string")
                        }
                        Some(&"括弧開") => {
                            // TODO 括弧モード終了
                            Some("string")
                        }
                        _ => Some("comment"),
                    }
                }
                _ => None,
            };

            if let Some(k) = kind {
                let s = &text.as_ref()[0..token.byte_end];
                let (left, right) = s.split_at(token.byte_start);

                let start = left.chars().count();
                let length = right.chars().count();

                tokens.push(SemanticToken::from_kind(start as u32, length as u32, k));
            }
        }

        tokens
    }

    /// ハイライト用トークン列をLSP用に変換する。
    ///
    /// Lindera を用いて形態素解析を行い、語種に基づくトークン種別を生成します。
    pub fn to_semantic_tokens(
        tokens: impl IntoIterator<Item = impl IntoIterator<Item = crate::highlight::SemanticToken>>,
    ) -> Vec<tower_lsp::lsp_types::SemanticToken> {
        let mut encoded = Vec::new();
        let mut prev_line: Option<u32> = None;
        let mut prev_start = 0_u32;

        for (line_no, token) in tokens.into_iter().enumerate() {
            let line_no = line_no as u32;
            for tkn in token {
                let (delta_line, delta_start) = match prev_line {
                    None => (line_no, tkn.start),
                    Some(pl) if pl == line_no => (0, tkn.start.saturating_sub(prev_start)),
                    Some(pl) => (line_no.saturating_sub(pl), tkn.start),
                };

                encoded.push(tower_lsp::lsp_types::SemanticToken {
                    delta_line,
                    delta_start,
                    length: tkn.length,
                    token_type: tkn.token_type,
                    token_modifiers_bitset: tkn.modifier,
                });

                prev_line = Some(line_no);
                prev_start = tkn.start;
            }
        }

        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_token_new() {
        let t = SemanticToken::from_kind(5, 3, "keyword");
        assert_eq!(t.start, 5);
        assert_eq!(t.length, 3);
        assert_eq!(t.token_type, SemanticTokenType::Keyword as u32);
        assert_eq!(t.modifier, 0);
    }

    #[test]
    fn test_tokenize_conversation_produces_tokens() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize("これはテストです。");
        assert!(
            !tokens.is_empty(),
            "tokenize_conversation should produce tokens"
        );

        // 簡単な検証
        // "これ" -> 名詞 -> variable
        // "は" -> 助詞 -> comment
        // "テスト" -> 名詞 -> variable
        // "です" -> 助動詞 -> comment
        // "。" -> 記号 -> comment
        assert_eq!(tokens.len(), 5);
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::Variable as u32, //, "variable",
            "{} <> variable @{}",
            tokens[0].token_type,
            tokens[0].start
        ); // これ
        assert_eq!(
            tokens[1].token_type,
            SemanticTokenType::Comment as u32,
            "{} <> comment @{}",
            tokens[1].token_type,
            tokens[1].start
        ); // は
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::Variable as u32,
            "{} <> variable @{}",
            tokens[2].token_type,
            tokens[2].start
        ); // テスト
        assert_eq!(
            tokens[3].token_type,
            SemanticTokenType::Comment as u32,
            "{} <> comment @{}",
            tokens[3].token_type,
            tokens[3].start
        ); // です
        assert_eq!(
            tokens[4].token_type,
            SemanticTokenType::Comment as u32,
            "{} <> comment @{}",
            tokens[4].token_type,
            tokens[4].start
        ); // 。
    }

    #[test]
    fn test_tokenize_conversation_empty_string() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize("");
        assert!(tokens.is_empty(), "Empty string should produce no tokens");
    }

    #[test]
    fn test_tokenize_conversation_unknown_words() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize("がびがび");
        // "がびがび" は名詞として扱われるはず
        assert_eq!(tokens[0].token_type, SemanticTokenType::Comment as u32);
    }

    #[test]
    fn test_tokenize_conversation_complex_sentence() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize("吾輩は猫である。名前はまだない。");
        assert!(!tokens.is_empty());
    }

    #[test]
    fn test_encode_semantic_tokens_same_line_uses_relative_start() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize("これはテストです。");
        let encoded = Highlighter::to_semantic_tokens(
            ["これはテストです。"]
                .iter()
                .map(|s| hilighter.tokenize(s))
                .collect::<Vec<_>>(),
        );
        assert!(encoded.len() >= 3);
        assert_eq!(encoded[1].delta_line, 0);
        assert_eq!(encoded[1].delta_start, 2);
        assert_eq!(encoded[2].delta_line, 0);
        assert_eq!(encoded[2].delta_start, 1);
    }

    #[test]
    fn test_encode_semantic_tokens_new_line_resets_start_base() {
        let hilighter = Highlighter::new();
        let encoded = Highlighter::to_semantic_tokens(
            ["これはテストです。", "これはテストです。"]
                .iter()
                .map(|s| hilighter.tokenize(s))
                .collect::<Vec<_>>(),
        );
        assert!(encoded.len() >= 6);
        assert_eq!(encoded[5].delta_line, 1);
        assert_eq!(encoded[5].delta_start, 0);
    }

    #[test]
    fn test_encode_semantic_tokens_skips_empty_lines_with_line_gap() {
        let hilighter = Highlighter::new();
        let encoded = Highlighter::to_semantic_tokens(
            ["これはテストです。", "", "これはテストです。"]
                .iter()
                .map(|s| hilighter.tokenize(s))
                .collect::<Vec<_>>(),
        );
        assert!(encoded.len() >= 6);
        assert_eq!(encoded[5].delta_line, 2);
        assert_eq!(encoded[5].delta_start, 0);
    }

    #[test]
    fn test_encode_semantic_tokens_preserves_length_type_modifier() {
        let hilighter = Highlighter::new();
        let source = hilighter.tokenize("これはテストです。");

        let encoded = Highlighter::to_semantic_tokens([source.clone()]);

        assert_eq!(source.len(), encoded.len());
        for (src, out) in source.iter().zip(encoded.iter()) {
            assert_eq!(src.length, out.length);
            assert_eq!(src.token_type, out.token_type);
            assert_eq!(src.modifier, out.token_modifiers_bitset);
        }
    }
}
