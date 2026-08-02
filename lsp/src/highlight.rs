/// 会話ハイライト用のトークナイザ・ユーティリティ
///
/// `lindera` を使って形態素解析を行い、会話テキストをハイライトします。
use std::collections::HashSet;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::Ordering::Relaxed;

use lindera::mode::Mode;
use lindera::tokenizer::TokenizerBuilder;
use parking_lot::RwLock;
use strum_macros::EnumIter;

use crate::types::{CachedLinderaToken, LineData, TokenStatus};
#[allow(unused_imports)]
use log::{debug, trace, warn};

/// ハイライト用トークンを表す型。
///
/// `start`/`length` は UTF-16 コード単位（LSP の positionEncoding=utf-16 に合わせた単位）。
#[derive(Debug, Clone)]
pub struct SemanticToken {
    /// 行頭からの UTF-16 コード単位オフセット
    pub start: u32,
    /// UTF-16 コード単位での長さ
    pub length: u32,
    /// トークンの種類（例: "keyword", "string", "function" など）
    pub token_type: u32,
    pub modifier: u32,
}

/// LSP の `SemanticToken`(デルタエンコード済み)をそのまま表す独自型。
/// LSPクレートへの依存を main.rs 境界に閉じ込めるため、highlight.rs はこの型を返す。
#[derive(Debug, Clone)]
pub struct EncodedSemanticToken {
    pub delta_line: u32,
    pub delta_start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers_bitset: u32,
}

#[derive(Debug, EnumIter)]
#[repr(u32)]
pub enum SemanticTokenType {
    // LSP 3.17 仕様の SemanticTokenTypes 定義順
    // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#semanticTokenTypes
    Namespace,
    Type,
    Class,
    Enum,
    Interface,

    Struct,
    TypeParameter,
    Parameter,
    Variable,
    Property,

    EnumMember,
    Event,
    Function,
    Method,
    Macro,

    Keyword,
    Modifier,
    Comment,
    String,
    Number,

    Regexp,
    Operator,
    Decorator,

    Undefined = u32::MAX,
}

impl SemanticToken {
    /// 新しいトークンを作成する簡易コンストラクタ
    #[allow(dead_code)]
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
            "namespace" => (SemanticTokenType::Namespace as u32, 0),
            "type" => (SemanticTokenType::Type as u32, 0),
            "class" => (SemanticTokenType::Class as u32, 0),
            "enum" => (SemanticTokenType::Enum as u32, 0),
            "interface" => (SemanticTokenType::Interface as u32, 0),
            "struct" => (SemanticTokenType::Struct as u32, 0),
            "typeparameter" => (SemanticTokenType::TypeParameter as u32, 0),
            "parameter" => (SemanticTokenType::Parameter as u32, 0),
            "variable" => (SemanticTokenType::Variable as u32, 0),
            "property" => (SemanticTokenType::Property as u32, 0),
            "enummember" => (SemanticTokenType::EnumMember as u32, 0),
            "event" => (SemanticTokenType::Event as u32, 0),
            "function" => (SemanticTokenType::Function as u32, 0),
            "method" => (SemanticTokenType::Method as u32, 0),
            "macro" => (SemanticTokenType::Macro as u32, 0),
            "keyword" => (SemanticTokenType::Keyword as u32, 0),
            "modifier" => (SemanticTokenType::Modifier as u32, 0),
            "comment" => (SemanticTokenType::Comment as u32, 0),
            "string" => (SemanticTokenType::String as u32, 0),
            "number" => (SemanticTokenType::Number as u32, 0),
            "regexp" => (SemanticTokenType::Regexp as u32, 0),
            "operator" => (SemanticTokenType::Operator as u32, 0),
            "decorator" => (SemanticTokenType::Decorator as u32, 0),
            _ => (
                SemanticTokenType::Undefined as u32,
                SemanticTokenType::Undefined as u32,
            ),
        }
    }
}

pub struct Highlighter {
    /// Lindera トークナイザ。キャラ名のユーザー辞書差し替え(`rebuild_user_dictionary`)が
    /// あるため RwLock で内部可変にしている。
    tokenizer: RwLock<lindera::tokenizer::Tokenizer>,
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

        Self {
            tokenizer: RwLock::new(tokenizer),
        }
    }

    /// 与えられたトークン(品詞details+表層形)がハイライト対象の人名かどうかを判定する。
    /// `classify_normal`/`classify_bracket` のkeyword判定と同一基準であり、hover等
    /// ハイライト以外の箇所でも同じ判定基準を使いたい場合に利用する
    /// (例: `backend.rs` の hover ハンドラ。ハイライトされないトークンでhoverだけ
    /// 表示されるという食い違いを防ぐため)。
    /// `allowed` は呼び出し側が対象ドキュメントのワークスペースに応じて渡す
    /// (`CharacterStore::allowed_names`)。
    pub fn is_recognized_name(details: &[String], surface: &str, allowed: &HashSet<String>) -> bool {
        Self::is_recognized_person_name(details, surface, allowed)
    }

    /// `names` の全エントリを固有名詞(名詞,固有名詞,人名,一般)としてユーザー辞書に
    /// 登録し直す。空集合なら辞書を外す。`names`は全ワークスペースの許可名の和集合を渡す
    /// (`CharacterStore::all_allowed_names`。トークナイズ品質の担保だけが目的で、
    /// どのワークスペースの名前かを区別する必要はない)。
    ///
    /// lindera 2.3.2 はCSVファイル経由でしかユーザー辞書を構築できないため、一時ファイルに
    /// 書き出してロード後に削除する。`Tokenizer.segmenter.user_dictionary` が pub なので、
    /// 埋め込みIPADIC の再ロードなしに辞書だけを差し替えられる。
    pub fn rebuild_user_dictionary(&self, names: &HashSet<String>) -> std::io::Result<()> {
        use std::io::{Error, ErrorKind};

        let rows: Vec<String> = names
            .iter()
            .filter(|n| {
                // CSVを壊す文字を含む名前は登録スキップ(表層一致ハイライトは引き続き機能する)
                let unsafe_csv =
                    n.contains(',') || n.contains('"') || n.contains('\n') || n.contains('\r');
                if unsafe_csv {
                    warn!("ユーザー辞書登録をスキップ(CSV非対応文字を含む): {:?}", n);
                }
                !unsafe_csv
            })
            .map(|n| Self::user_dict_csv_row(n, "人名", "一般"))
            .collect();

        if rows.is_empty() {
            self.tokenizer.write().segmenter.user_dictionary = None;
            debug!("rebuild_user_dictionary: 登録名なし、ユーザー辞書を解除");
            return Ok(());
        }

        // 一時CSVファイル。並列テスト・複数LSPプロセスの衝突を避けるため PID+連番 を含める。
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fifty_four_userdic_{}_{}.csv",
            std::process::id(),
            COUNTER.fetch_add(1, Relaxed),
        ));
        std::fs::write(&path, rows.join("\n"))?;

        let result = {
            let mut tok = self.tokenizer.write();
            match lindera::dictionary::load_user_dictionary_from_csv(
                &tok.segmenter.dictionary.metadata,
                &path,
            ) {
                Ok(ud) => {
                    tok.segmenter.user_dictionary = Some(ud);
                    debug!("rebuild_user_dictionary: {} 件を登録", rows.len());
                    Ok(())
                }
                Err(e) => Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("failed to build user dictionary: {}", e),
                )),
            }
        };

        let _ = std::fs::remove_file(&path);
        result
    }

    /// ユーザー辞書CSVの1行(IPADIC 詳細13カラム形式)を組み立てる。
    /// 文脈ID 0はlindera が簡易形式に自動付与する実績値(未定義文脈)。
    /// コスト2000は「1文字の人名でも単独トークンとして切り出す」ことと
    /// 「"高原"/"原因"/"原則"等の一般語を誤って分割しない」ことを両立する値として実測で選定
    /// (500〜10000の範囲で両立を確認、IPADIC実在の人名エントリと同程度の常識的なコスト感)。
    /// -10000のような極端に低いコストは、字面が短い名前ほど一般語の部分文字列と衝突しやすく
    /// 誤分割を招くため避ける。
    /// 原形=表層形とし(`text_to_lindera_token` の d[6]=="*" 除外フィルタを回避)、読み/発音は "*"。
    /// 組織(固有名詞,組織,*)・地域(固有名詞,地域,一般)のサポート追加時は subcategory を変えて呼ぶ。
    fn user_dict_csv_row(surface: &str, subcategory2: &str, subcategory3: &str) -> String {
        format!(
            "{s},0,0,2000,名詞,固有名詞,{c2},{c3},*,*,{s},*,*",
            s = surface,
            c2 = subcategory2,
            c3 = subcategory3,
        )
    }

    pub fn text_to_lindera_token(&self, text: &str) -> Vec<CachedLinderaToken> {
        let tokenizer = self.tokenizer.read();
        tokenizer
            .tokenize(text)
            .expect("failed to tokenize text")
            .into_iter()
            .filter_map(|mut t| {
                let d = t.details();
                if d[6] == "*" {
                    trace!("\t{:?} {:?}", d[6], d[0..=3].to_vec());
                    if d[0] == "名詞" && d[1] == "サ変接続" {
                        // 不正なtokenはしまっちゃう
                        return None;
                    }
                }

                Some(CachedLinderaToken {
                    details: t.details().iter().map(|s| s.to_string()).collect(),
                    byte_start: t.byte_start,
                    byte_end: t.byte_end,
                    // tag は行の開始深さに依存するため、ここでは仮値。
                    // `tag_line_depth` が深さを畳み込みながら確定させる。
                    tag: TokenStatus::Normal,
                })
            })
            .collect()
    }

    /// 単一行を `start_depth` 起点でタグ付けし、行終端の括弧深さを返す。
    ///
    /// - `line.tokens` が空なら Lindera で遅延トークン化する
    /// - 各トークンの `tag` を InBracket/Normal に**明示的に**設定する
    ///   (キャッシュ済みトークンを異なる深さで再タグ付けするケースがあるため、
    ///    InBracket → Normal への戻しも必要)
    /// - `line.bracket_depth_after` に終端深さをキャッシュする
    ///
    /// 括弧開閉トークン自身の扱い:
    /// - 括弧開: 自身はまだ外側(処理前の深さで判定) → その後 depth+1
    /// - 括弧閉: depth-1 → 自身はもう外側(処理後の深さで判定)
    fn tag_line_depth(&self, line: &mut LineData, start_depth: u32) -> u32 {
        // 遅延解析
        if line.tokens.is_empty() {
            trace!("lazy tokenize");
            line.tokens = self.text_to_lindera_token(&line.text);
        }

        let mut depth = start_depth;
        for token in line.tokens.iter_mut() {
            match (
                token.details[0].as_str(),
                token.details.get(1).map(|d| d.as_str()),
            ) {
                ("記号", Some("括弧開")) => {
                    token.tag = if depth > 0 {
                        TokenStatus::InBracket
                    } else {
                        TokenStatus::Normal
                    };
                    depth += 1;
                }
                ("記号", Some("括弧閉")) => {
                    depth = depth.saturating_sub(1);
                    token.tag = if depth > 0 {
                        TokenStatus::InBracket
                    } else {
                        TokenStatus::Normal
                    };
                }
                _ => {
                    token.tag = if depth > 0 {
                        TokenStatus::InBracket
                    } else {
                        TokenStatus::Normal
                    };
                }
            }
        }

        line.bracket_depth_after = Some(depth);
        depth
    }

    /// `line_no` 行の終端深さを保証する畳み込み。
    ///
    /// `bracket_depth_after` が Some ならそれを返す(O(1) 高速パス)。
    /// None なら後方へ遡り、最寄りのキャッシュ済み行(無ければ行0, depth=0)から
    /// `line_no` まで `tag_line_depth` で前向きに畳み直す。
    /// 副作用として、再計算した範囲の全トークンの `tag` が正しく設定される。
    ///
    /// `apply_changes` が編集行以降の `bracket_depth_after` を一括 None にする規約
    /// (前方累積量の無効化)と対になっており、定常の打鍵(編集行=カーソル行)では
    /// 再計算は1行分だけで済む。
    pub fn ensure_bracket_depth(&self, lines: &mut [LineData], line_no: usize) -> u32 {
        if lines.is_empty() {
            return 0;
        }
        let line_no = line_no.min(lines.len() - 1);
        if let Some(d) = lines[line_no].bracket_depth_after {
            return d;
        }

        // 最寄りのキャッシュ済み祖先を後方探索
        let mut start = line_no;
        while start > 0 && lines[start - 1].bracket_depth_after.is_none() {
            start -= 1;
        }
        let mut depth = if start == 0 {
            0
        } else {
            lines[start - 1].bracket_depth_after.unwrap()
        };

        // キャッシュが見つかった所から line_no まで前向きに畳み直す
        for line in &mut lines[start..=line_no] {
            depth = self.tag_line_depth(line, depth);
        }
        depth
    }

    /// テキストを受け取り、ハイライト用トークン列と行終端の括弧深さを返す。
    ///
    /// `tag_line_depth` でタグ付けした後、語種と括弧内外に基づいて
    /// ハイライト用のトークン種別を生成する。
    pub fn tokenize_with_depth(
        &self,
        line: &mut LineData,
        start_depth: u32,
        allowed: &HashSet<String>,
    ) -> (Vec<SemanticToken>, u32) {
        let end_depth = self.tag_line_depth(line, start_depth);

        let mut result = Vec::new();
        for token in line.tokens.iter() {
            // 人名判定に使う表層文字列
            let surface = &line.text[token.byte_start..token.byte_end];

            // 括弧開閉自身は常に "comment"。それ以外は tag(tag_line_depth が確定済み)で
            // 括弧内外モードを分けて classify へ委譲する。
            let kind = match (
                token.details[0].as_str(),
                token.details.get(1).map(|d| d.as_str()),
            ) {
                ("記号", Some("括弧開")) | ("記号", Some("括弧閉")) => Some("comment"),
                _ if token.tag == TokenStatus::InBracket => {
                    Self::classify_bracket(&token.details, surface, allowed)
                }
                _ => Self::classify_normal(&token.details, surface, allowed),
            };

            if let Some(k) = kind {
                // positionEncoding=utf-16 に合わせ、UTF-16 コード単位で位置と長さを算出する
                let start = crate::types::utf16_len(&line.text[..token.byte_start]);
                let length = crate::types::utf16_len(&line.text[token.byte_start..token.byte_end]);

                result.push(SemanticToken::from_kind(start as u32, length as u32, k));
            }
        }

        (result, end_depth)
    }

    /// 深さ0(括弧外)起点の `tokenize_with_depth`。単一行・深さ0前提の呼び出し向け互換ラッパ。
    /// 本体コードは深さを明示する `tokenize_with_depth`/`ensure_bracket_depth` を使うため、
    /// 現在はテスト専用。
    #[allow(dead_code)]
    pub fn tokenize(&self, line: &mut LineData, allowed: &HashSet<String>) -> Vec<SemanticToken> {
        self.tokenize_with_depth(line, 0, allowed).0
    }

    /// トークンが「許可名一致の人名」であるかを判定する共通述語。
    ///
    /// `classify_normal`/`classify_bracket` のkeyword判定条件そのもの
    /// (品詞が固有名詞,人名 かつ 表層形が許可名集合に含まれる)であり、
    /// hover等ハイライト以外の箇所からも同一基準で判定できるよう公開している
    /// (`Highlighter::is_recognized_name` 経由)。
    /// ここでの判定が変わらない限り hover とハイライトは常に一致する。
    fn is_recognized_person_name(
        details: &[String],
        surface: &str,
        allowed: &HashSet<String>,
    ) -> bool {
        details.first().map(String::as_str) == Some("名詞")
            && details.get(1).map(String::as_str) == Some("固有名詞")
            && details.get(2).map(String::as_str) == Some("人名")
            && allowed.contains(surface)
    }

    /// 通常モードでの品詞→トークン種別マッピング。
    ///
    /// 人名(固有名詞・人名)は `allowed` (キャラ一覧の名前+aliases) に含まれる場合のみ
    /// ハイライトする。組織名・地域名の判定は無効化(コメントアウト)しているが、
    /// 将来再度有効化できるようロジックは残してある。
    fn classify_normal(
        details: &[String],
        surface: &str,
        allowed: &HashSet<String>,
    ) -> Option<&'static str> {
        let v = details[1..]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        match details[0].as_str() {
            "名詞" => match v.as_ref() {
                ["固有名詞", "人名", ..] => {
                    if Self::is_recognized_person_name(details, surface, allowed) {
                        Some("keyword")
                    } else {
                        None
                    }
                }

                // ["固有名詞", "組織", ..] => Some("variable"),

                // ["固有名詞", "地域", "一般", ..] | ["固有名詞", "地域", "国", ..] => {
                //     Some("function")
                // }
                // ["接尾", "サ変接続", ..] => Some("variable"),
                _ => None,
            },
            "記号" => Some("comment"),
            _ => None,
        }
    }

    /// 括弧内モードでの品詞→トークン種別マッピング。
    ///
    /// 人名の絞り込みは `classify_normal` と同様。組織名・地域名の判定は無効化(コメントアウト)。
    fn classify_bracket(
        details: &[String],
        surface: &str,
        allowed: &HashSet<String>,
    ) -> Option<&'static str> {
        let v = details[1..]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        match details[0].as_str() {
            "名詞" => match v.as_ref() {
                ["固有名詞", "人名", ..] => {
                    if Self::is_recognized_person_name(details, surface, allowed) {
                        Some("keyword")
                    } else {
                        Some("string")
                    }
                }

                // ["固有名詞", "組織"] => Some("variable"),

                // ["固有名詞", "地域", "一般"] | ["固有名詞", "地域", "国"] => {
                //     Some("function")
                // }
                ["サ変接続"] | ["接尾", "サ変接続"] => Some("string"),
                _ => Some("string"),
            },
            "記号" => Some("comment"),
            _ => Some("string"),
        }
    }

    /// ハイライト用トークン列をLSP用に変換する。
    ///
    pub fn to_semantic_tokens(
        tokens: impl IntoIterator<Item = impl IntoIterator<Item = crate::highlight::SemanticToken>>,
    ) -> Vec<EncodedSemanticToken> {
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

                encoded.push(EncodedSemanticToken {
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
    use std::str::FromStr;

    #[test]
    fn highlight_token_new() {
        let t = SemanticToken::from_kind(5, 3, "keyword");
        assert_eq!(t.start, 5);
        assert_eq!(t.length, 3);
        assert_eq!(t.token_type, SemanticTokenType::Keyword as u32);
        assert_eq!(t.modifier, 0);
    }

    /// 空の許可名集合(名前判定を伴わないテスト用)。
    fn no_names() -> HashSet<String> {
        HashSet::new()
    }

    /// 括弧内モード(開始深さ1)でトークン化するテスト用ヘルパ。
    fn tokenize_in_bracket(
        h: &Highlighter,
        line: &mut LineData,
        allowed: &HashSet<String>,
    ) -> Vec<SemanticToken> {
        h.tokenize_with_depth(line, 1, allowed).0
    }

    #[test]
    fn test_tokenize_conversation_produces_tokens() {
        let hilighter = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &hilighter,
            &mut LineData::from_str("これはテストです。").unwrap(),
            &no_names(),
        );
        assert!(
            !tokens.is_empty(),
            "tokenize_conversation should produce tokens"
        );

        // 簡単な検証(括弧内モードは許可名一致以外すべて string に丸められる)
        // "これ" -> 名詞 -> string
        // "は" -> 助詞 -> string
        // "テスト" -> 名詞 -> string
        // "です" -> 助動詞 -> string
        // "。" -> 記号 -> comment
        assert_eq!(tokens.len(), 5);
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::String as u32,
            "{} <> string @{}",
            tokens[0].token_type,
            tokens[0].start
        ); // これ
        assert_eq!(
            tokens[1].token_type,
            SemanticTokenType::String as u32,
            "{} <> variable @{}",
            tokens[1].token_type,
            tokens[1].start
        ); // は
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::String as u32,
            "{} <> string @{}",
            tokens[2].token_type,
            tokens[2].start
        ); // テスト (名詞,サ変接続 -> string)
    }

    #[test]
    fn test_tokenize_conversation_empty_string() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize(&mut LineData::from_str("").unwrap(), &no_names());
        assert!(tokens.is_empty(), "Empty string should produce no tokens");
    }

    #[test]
    fn test_tokenize_conversation_unknown_words() {
        let hilighter = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &hilighter,
            &mut LineData::from_str("がびがび").unwrap(),
            &no_names(),
        );
        // "がびがび" は名詞として扱われるはず
        assert_eq!(tokens[0].token_type, SemanticTokenType::String as u32);
    }

    #[test]
    fn test_tokenize_conversation_complex_sentence() {
        let hilighter = Highlighter::new();
        let tokens = hilighter.tokenize(
            &mut LineData::from_str("吾輩は猫である。名前はまだない。").unwrap(),
            &no_names(),
        );
        assert!(!tokens.is_empty());
    }

    #[test]
    fn test_registered_person_name_is_keyword() {
        // "田中" は Lindera IPADIC で 固有名詞,人名,姓 の単一トークンになる(実測確認済み)。
        let h = Highlighter::new();
        let allowed = HashSet::from(["田中".to_string()]);
        let tokens = h.tokenize(&mut LineData::from_str("田中").unwrap(), &allowed);
        assert_eq!(tokens.len(), 1, "{:?}", tokens);
        assert_eq!(tokens[0].token_type, SemanticTokenType::Keyword as u32);
    }

    #[test]
    fn test_is_recognized_name_consistent_with_highlight_for_common_word_collision() {
        // "コンドル"はIPADICに一般名詞(禿鷲)として登録されており、ユーザー辞書登録名でも
        // Viterbiで一般名詞側の経路が選ばれ 固有名詞,人名 にならない(実測確認済み)。
        // is_recognized_name はハイライトと同じ基準で判定するため、この場合は一致してfalseを返す
        // (hoverでも表示されなくなり、ハイライトとの食い違いが起きない)。
        let h = Highlighter::new();
        let allowed = HashSet::from([
            "ジョサイア・コンドル".to_string(),
            "ジョサイア".to_string(),
            "コンドル".to_string(),
        ]);
        h.rebuild_user_dictionary(&allowed).expect("辞書再構築に失敗");

        let text = "ジョサイア・コンドルの手による";
        let tokens = h.text_to_lindera_token(text);

        let conder = tokens
            .iter()
            .find(|t| &text[t.byte_start..t.byte_end] == "コンドル")
            .expect("「コンドル」トークンが見つからない");
        assert!(
            !Highlighter::is_recognized_name(&conder.details, "コンドル", &allowed),
            "{:?}",
            conder.details
        );

        let josiah = tokens
            .iter()
            .find(|t| &text[t.byte_start..t.byte_end] == "ジョサイア")
            .expect("「ジョサイア」トークンが見つからない");
        assert!(
            Highlighter::is_recognized_name(&josiah.details, "ジョサイア", &allowed),
            "{:?}",
            josiah.details
        );

        // ハイライト結果とも一致すること
        let sem = h.tokenize(&mut LineData::from_str(text).unwrap(), &allowed);
        assert_eq!(sem.len(), 2, "{:?}", sem); // ジョサイア(keyword) + "・"(comment) のみ
    }

    #[test]
    fn test_single_char_registered_name_is_keyword() {
        // 1文字の姓("原")はユーザー辞書登録(コスト3000)によって固有名詞,人名として
        // 単独トークンに切り出され、許可名一致で keyword になる。
        let h = Highlighter::new();
        let allowed = HashSet::from(["原".to_string(), "原顕三郎".to_string()]);
        h.rebuild_user_dictionary(&allowed).expect("辞書再構築に失敗");

        let tokens = h.tokenize(&mut LineData::from_str("原は独りごちた").unwrap(), &allowed);
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::Keyword as u32,
            "{:?}",
            tokens
        );
        assert_eq!(tokens[0].length, 1, "{:?}", tokens);

        // フルネームは1トークンとして keyword になる(単独名との共存回帰確認)
        let tokens = h.tokenize(&mut LineData::from_str("原顕三郎少将").unwrap(), &allowed);
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::Keyword as u32,
            "{:?}",
            tokens
        );
        assert_eq!(tokens[0].length, 4, "{:?}", tokens); // "原顕三郎"
    }

    #[test]
    fn test_unknown_katakana_name_still_single_token() {
        // コスト3000への変更後も、IPADIC未知のカタカナ名(例: "シルビア")が
        // 引き続き1トークンでkeywordになること(コスト緩和による回帰確認)。
        let h = Highlighter::new();
        let allowed = HashSet::from(["シルビア".to_string()]);
        h.rebuild_user_dictionary(&allowed).expect("辞書再構築に失敗");

        let tokens = h.tokenize(&mut LineData::from_str("シルビア").unwrap(), &allowed);
        assert_eq!(tokens.len(), 1, "{:?}", tokens);
        assert_eq!(tokens[0].token_type, SemanticTokenType::Keyword as u32);
    }

    #[test]
    fn test_single_char_registered_name_does_not_split_common_words() {
        // 1文字人名のユーザー辞書登録が、"高原"/"原因"/"原則"のような一般語を
        // 誤って分割してハイライトしないこと。
        let h = Highlighter::new();
        let allowed = HashSet::from(["原".to_string()]);
        h.rebuild_user_dictionary(&allowed).expect("辞書再構築に失敗");

        for word in ["高原", "原因", "原則"] {
            let tokens = h.tokenize(&mut LineData::from_str(word).unwrap(), &allowed);
            assert!(
                tokens.is_empty(),
                "{} が「原」の誤分割でハイライトされてしまっている: {:?}",
                word,
                tokens
            );
        }
    }

    #[test]
    fn test_unregistered_person_name_not_highlighted_normal() {
        // 許可名集合が空の場合、通常モードでは固有名詞人名でも一切トークンを生成しない。
        let h = Highlighter::new();
        let tokens = h.tokenize(&mut LineData::from_str("田中").unwrap(), &no_names());
        assert!(tokens.is_empty(), "{:?}", tokens);
    }

    #[test]
    fn test_unregistered_person_name_in_bracket_is_string() {
        // 括弧内モードでは、許可名集合に無い語は一般名詞と同じ string にフォールバックする。
        let h = Highlighter::new();
        let tokens = tokenize_in_bracket(&h, &mut LineData::from_str("田中").unwrap(), &no_names());
        assert_eq!(tokens.len(), 1, "{:?}", tokens);
        assert_eq!(tokens[0].token_type, SemanticTokenType::String as u32);
    }

    #[test]
    fn test_organization_and_region_not_highlighted() {
        // 組織名("自民党": 固有名詞,組織)・地域名("東京": 固有名詞,地域,一般、"日本": 固有名詞,地域,国)は
        // 品詞ベースの判定ロジックをコメントアウトしているため、許可名集合に入っていなければ
        // Variable/Function は生成されない(通常モードでは None -> トークン自体が生成されない)。
        // keyword 化は「固有名詞,人名」かつ許可名集合に一致した場合のみで、組織名・地域名は
        // 許可名集合に入っていても対象外(classify_normal/classify_bracket 参照)。
        let h = Highlighter::new();
        for word in ["自民党", "東京", "日本"] {
            let tokens = h.tokenize(&mut LineData::from_str(word).unwrap(), &no_names());
            assert!(
                tokens.is_empty(),
                "{} が組織/地域としてハイライトされてしまっている: {:?}",
                word,
                tokens
            );
        }
    }

    #[test]
    fn test_encode_semantic_tokens_same_line_uses_relative_start() {
        let hilighter = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &hilighter,
            &mut LineData::from_str("これはテストです。").unwrap(),
            &no_names(),
        );
        let encoded = Highlighter::to_semantic_tokens([tokens.clone()]);
        assert!(encoded.len() >= 3);
        assert_eq!(encoded[1].delta_line, 0);
        assert_eq!(encoded[1].delta_start, 2);
        assert_eq!(encoded[2].delta_line, 0);
        assert_eq!(encoded[2].delta_start, 1);
    }

    /*    #[test]
        fn test_encode_semantic_tokens_new_line_resets_start_base() {
            let hilighter = Highlighter::in_bracket();
            let encoded = Highlighter::to_semantic_tokens(
                ["これはテストです。", "これはテストです。"]
                    .iter()
                    .map(|s| hilighter.tokenize(&mut LineData::from_str(s).unwrap()))
                    .collect::<Vec<_>>(),
            );
            assert!(encoded.len() >= 6);
            assert_eq!(encoded[5].delta_line, 1);
            assert_eq!(encoded[5].delta_start, 0);
        }
    */
    #[test]
    fn test_encode_semantic_tokens_skips_empty_lines_with_line_gap() {
        let hilighter = Highlighter::new();
        let encoded = Highlighter::to_semantic_tokens(
            ["これはテストです。", "", "これはテストです。"]
                .iter()
                .map(|s| {
                    tokenize_in_bracket(&hilighter, &mut LineData::from_str(s).unwrap(), &no_names())
                })
                .collect::<Vec<_>>(),
        );
        assert!(encoded.len() >= 6);
        assert_eq!(encoded[5].delta_line, 2);
        assert_eq!(encoded[5].delta_start, 0);
    }

    #[test]
    fn test_encode_semantic_tokens_preserves_length_type_modifier() {
        let hilighter = Highlighter::new();
        let source = tokenize_in_bracket(
            &hilighter,
            &mut LineData::from_str("これはテストです。").unwrap(),
            &no_names(),
        );

        let encoded = Highlighter::to_semantic_tokens([source.clone()]);

        assert_eq!(source.len(), encoded.len());
        for (src, out) in source.iter().zip(encoded.iter()) {
            assert_eq!(src.length, out.length);
            assert_eq!(src.token_type, out.token_type);
            assert_eq!(src.modifier, out.token_modifiers_bitset);
        }
    }

    // --- 括弧内モードのテスト ---

    /// 助詞「は」が通常モードでスキップされることを確認するヘルパーテスト。
    /// "猫は" で形態素解析すると「猫(名詞)」「は(助詞)」に分かれることを利用する。
    #[test]
    fn test_particle_ha_is_skipped_outside_bracket() {
        // 括弧内モードでは許可名一致以外すべて string に丸められる
        let h = Highlighter::new();
        let tokens = tokenize_in_bracket(&h, &mut LineData::from_str("猫は").unwrap(), &no_names());
        assert_eq!(tokens.len(), 2, "猫は should produce 2 token");
        assert_eq!(tokens[0].token_type, SemanticTokenType::String as u32);
    }

    #[test]
    fn test_bracket_open_and_close_are_comment() {
        // 括弧開・括弧閉自体は括弧外扱いで "comment"
        let h = Highlighter::new();
        let mut l = LineData::from_str("「テスト」").unwrap();
        let tokens = h.tokenize(&mut l, &no_names());
        // [「(comment), テスト(keyword/名詞), 」(comment)]
        assert_eq!(tokens.len(), 3, "「テスト」 should produce 3 tokens");
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::Comment as u32,
            "括弧開「 should be comment (bracket-external)"
        );
        assert_eq!(
            l.tokens[0].tag,
            TokenStatus::Normal,
            "括弧開「 should be out of bracket"
        );
        assert_eq!(
            tokens[1].token_type,
            SemanticTokenType::String as u32,
            "テスト(名詞,サ変接続) inside bracket should be string"
        );
        assert_eq!(
            l.tokens[1].tag,
            TokenStatus::InBracket,
            "テスト(名詞) should be inside bracket"
        );
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::Comment as u32,
            "括弧閉」 should be comment (bracket-external)"
        );
        assert_eq!(
            l.tokens[2].tag,
            TokenStatus::Normal,
            "括弧閉」 should be out of bracket"
        );
    }

    #[test]
    fn test_particle_inside_bracket_becomes_string() {
        // 括弧内では _ カテゴリが "string" になる
        // "猫は" で「猫(名詞)」「は(助詞)」に分かれる → 括弧内の「は」が string になるか
        let h = Highlighter::new();
        let tokens = h.tokenize(&mut LineData::from_str("「猫は」").unwrap(), &no_names());
        // [「(comment), 猫(keyword), は(string), 」(comment)]
        assert_eq!(tokens.len(), 4, "「猫は」 should produce 4 tokens");
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::String as u32,
            "助詞「は」 inside bracket should be string"
        );
    }

    #[test]
    fn test_bracket_mode_persists_across_tokenize_calls() {
        // 複数行にまたがる括弧で、深さを戻り値で次の行へ引き継ぐ
        let h = Highlighter::new();
        let names = no_names();
        let (_, d) = h.tokenize_with_depth(&mut LineData::from_str("「猫").unwrap(), 0, &names); // 括弧開 → depth=1
        assert_eq!(d, 1);
        let (inside, d) =
            h.tokenize_with_depth(&mut LineData::from_str("猫は").unwrap(), d, &names); // 括弧内 → は が "string"
        // [猫(keyword), は(string)]
        assert_eq!(
            inside.len(),
            2,
            "猫は inside bracket (cross-line) should produce 2 tokens"
        );
        assert_eq!(
            inside[1].token_type,
            SemanticTokenType::String as u32,
            "助詞 should be string when inside bracket across lines"
        );
        let (_, d) = h.tokenize_with_depth(&mut LineData::from_str("」").unwrap(), d, &names); // 括弧閉 → depth=0
        assert_eq!(d, 0);
        let (outside, _) =
            h.tokenize_with_depth(&mut LineData::from_str("猫は").unwrap(), d, &names); // 括弧外 → は スキップ
        assert_eq!(
            outside.len(),
            0,
            "猫は outside bracket should produce 1 token (猫 only)"
        );
    }

    #[test]
    fn test_nested_brackets_depth() {
        // ネストした括弧でdepthが正しく管理される
        let h = Highlighter::new();
        let names = no_names();
        let (_, d) = h.tokenize_with_depth(&mut LineData::from_str("「").unwrap(), 0, &names); // depth=1
        let (_, d) = h.tokenize_with_depth(&mut LineData::from_str("「").unwrap(), d, &names); // depth=2
        assert_eq!(d, 2);
        let (inner, d) = h.tokenize_with_depth(&mut LineData::from_str("猫は").unwrap(), d, &names); // depth=2 → は=string
        assert_eq!(inner.len(), 2, "should be in bracket mode at depth 2");
        assert_eq!(inner[1].token_type, SemanticTokenType::String as u32);
        let (_, d) = h.tokenize_with_depth(&mut LineData::from_str("」").unwrap(), d, &names); // depth=1
        let (still_inside, d) =
            h.tokenize_with_depth(&mut LineData::from_str("猫は").unwrap(), d, &names); // depth=1 → は=string
        assert_eq!(
            still_inside.len(),
            2,
            "should still be in bracket mode at depth 1"
        );
        assert_eq!(still_inside[1].token_type, SemanticTokenType::String as u32);
        let (_, d) = h.tokenize_with_depth(&mut LineData::from_str("」").unwrap(), d, &names); // depth=0
        assert_eq!(d, 0);
        let (outside, _) =
            h.tokenize_with_depth(&mut LineData::from_str("猫は").unwrap(), d, &names); // depth=0 → は スキップ
        assert_eq!(
            outside.len(),
            0,
            "should be outside bracket mode at depth 0"
        );
    }

    #[test]
    fn test_ensure_bracket_depth_fast_path_and_fold() {
        // 全行フォールド後は O(1) 高速パス(キャッシュ値がそのまま返る)。
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = ["「セリフ１」", "「セリフ２"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();

        // 行1(2行目)まで畳み込み: 行0 は閉じて depth=0、行1 は開きっぱなしで depth=1
        assert_eq!(h.ensure_bracket_depth(&mut lines, 1), 1);
        assert_eq!(lines[0].bracket_depth_after, Some(0));
        assert_eq!(lines[1].bracket_depth_after, Some(1));

        // キャッシュ済みなので再要求してもそのまま返る(高速パス)
        assert_eq!(h.ensure_bracket_depth(&mut lines, 0), 0);
        assert_eq!(h.ensure_bracket_depth(&mut lines, 1), 1);

        // 行0 の「セリフ１」中身は InBracket、行頭の「と行末の」は Normal(括弧外扱い)
        assert_eq!(lines[0].tokens.first().unwrap().tag, TokenStatus::Normal);
        assert!(
            lines[0]
                .tokens
                .iter()
                .skip(1)
                .take(lines[0].tokens.len() - 2)
                .all(|t| t.tag == TokenStatus::InBracket),
            "{:?}",
            lines[0].tokens
        );
        assert_eq!(lines[0].tokens.last().unwrap().tag, TokenStatus::Normal);
    }

    #[test]
    fn test_ensure_bracket_depth_refold_after_invalidation() {
        // 陳腐化修復: 上方の行が変わって以降のキャッシュが None 化されたとき、
        // 再フォールドで新しい深さ・タグに更新されること。
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = ["こんにちは。", "猫は"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();
        assert_eq!(h.ensure_bracket_depth(&mut lines, 1), 0);
        assert!(lines[1].tokens.iter().all(|t| t.tag == TokenStatus::Normal));

        // 行0 を「こんにちは(開きっぱなし)へ編集 → apply_changes 相当の無効化
        lines[0] = LineData::from_str("「こんにちは。").unwrap();
        lines[1].bracket_depth_after = None; // 編集行以降の一括 None クリア相当

        // 再フォールドすると行1 は括弧内になる(タグも InBracket に更新される)
        assert_eq!(h.ensure_bracket_depth(&mut lines, 1), 1);
        assert!(
            lines[1]
                .tokens
                .iter()
                .all(|t| t.tag == TokenStatus::InBracket),
            "{:?}",
            lines[1].tokens
        );
    }
}
