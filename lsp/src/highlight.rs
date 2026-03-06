use std::fmt::Debug;

use lindera::mode::Mode;
use lindera::tokenizer::TokenizerBuilder;
use tracing::{debug, instrument};

/// 会話ハイライト用のトークナイザ・ユーティリティ
///
/// `lindera` を使って形態素解析を行い、会話テキストをハイライトします。

/// ハイライト用トークンを表す型。
///
/// `start`/`length` はバイト単位のオフセットを想定しています（LSP の semantic tokens 生成時に変換して使います）。
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

/// 会話テキストを受け取り、ハイライト用トークン列を返す。
///
/// Lindera を用いて形態素解析を行い、語種に基づくトークン種別を生成します。
#[instrument]
pub fn tokenize_conversation(text: impl AsRef<str> + Debug) -> Vec<SemanticToken> {
    // IPADIC を使用する設定でトークナイザを作成
    let tokenizer = TokenizerBuilder::new()
        .unwrap()
        .set_segmenter_mode(&Mode::Normal)
        .set_segmenter_dictionary("embedded://ipadic")
        // .set_segmenter_user_dictionary("")
        .build()
        .expect("failed to create lindera tokenizer");

    let mut tokens = Vec::new();
    // `tokenize` は Result を返すため、expect で処理
    let lindera_tokens = tokenizer
        .tokenize(text.as_ref())
        .expect("failed to tokenize text");

    for mut token in lindera_tokens {
        let start = token.byte_start;
        let length = token.byte_end - token.byte_start;

        debug!(
            "Token: `{:?}`, Details: {:?}",
            text.as_ref()[start..start + length].to_string(),
            token.details()
        );

        let details = token.details();
        let kind = match details[0] {
            "名詞" => "variable",
            "動詞" => "function",
            "形容詞" => "function",
            "記号" => {
                match details.get(1) {
                    Some(&"句点") | Some(&"読点") => "comment",
                    Some(&"括弧閉") => {
                        // TODO 括弧モード開始
                        "string"
                    }
                    Some(&"括弧開") => {
                        // TODO 括弧モード終了
                        "string"
                    }
                    _ => "comment",
                }
            }
            _ => "comment",
        };

        tokens.push(SemanticToken::from_kind(start as u32, length as u32, kind));
    }

    tokens
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
        let tokens = tokenize_conversation("これはテストです。");
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
        let tokens = tokenize_conversation("");
        assert!(tokens.is_empty(), "Empty string should produce no tokens");
    }

    #[test]
    fn test_tokenize_conversation_unknown_words() {
        let tokens = tokenize_conversation("がびがび");
        // "ぴえん" は名詞として扱われるはず
        assert_eq!(tokens[0].token_type, SemanticTokenType::Comment as u32);
    }

    #[test]
    fn test_tokenize_conversation_complex_sentence() {
        let tokens = tokenize_conversation("吾輩は猫である。名前はまだない。");
        assert!(!tokens.is_empty());
    }
}
