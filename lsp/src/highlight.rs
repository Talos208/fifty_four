/// 会話ハイライト用のトークナイザ・ユーティリティ
///
/// `lindera` を使って形態素解析を行い、会話テキストをハイライトします。
use std::collections::{HashSet, VecDeque};
use std::fmt::{Debug, Formatter};
use std::sync::atomic::Ordering::Relaxed;
use std::usize;

use lindera::mode::Mode;
use lindera::tokenizer::TokenizerBuilder;
use parking_lot::RwLock;
use strum_macros::EnumIter;
use tracing::instrument;

use crate::types::{CachedLinderaToken, LineData, TokenMeaning, TokenStatus, is_kanji_all};
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

    pub fn from_meaning(start: u32, length: u32, meaning: TokenMeaning) -> Self {
        let (token_type, modifier) = match meaning {
            TokenMeaning::Normal => (SemanticTokenType::Undefined as u32, 0u32),

            TokenMeaning::Bracket | TokenMeaning::BracketClose => {
                (SemanticTokenType::Comment as u32, 0u32)
            }
            TokenMeaning::InnerBracket => (SemanticTokenType::String as u32, 0u32),

            TokenMeaning::RubyBracket => (SemanticTokenType::Comment as u32, 0u32),
            TokenMeaning::RubyBody => (SemanticTokenType::Namespace as u32, 0u32),
            TokenMeaning::Ruby => (SemanticTokenType::String as u32, 0u32),

            TokenMeaning::Characters => (SemanticTokenType::Keyword as u32, 0u32),

            _ => (SemanticTokenType::Undefined as u32, 0u32),
        };
        Self {
            start,
            length,
            token_type,
            modifier,
        }
    }
}

/// 括弧内トークンの色分け方針。
///
/// `.txt` 原稿本文では「」が台詞であり、地の文(括弧外)とは別の色で塗りたい。一方
/// `.md`(`characters.md`/`plot.md` 等の設定・メモ)では括弧は単なる注釈で、台詞と
/// 見なして塗り分けると段落全体が `string` 色に沈んで読みにくくなる。この違いを
/// `tokenize_with_depth` の呼び出し側(`backend.rs` の `is_md`)から選べるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketColoring {
    /// 括弧内は台詞として `classify_bracket` で塗る(`.txt` 原稿本文)
    Distinct,
    /// 括弧内外を区別せず常に `classify_normal` で塗る(`.md` の設定・メモ)
    Uniform,
}

/// `Clone` は `Arc` の参照カウント増加のみ。トークナイザ実体は clone 間で共有され、
/// どの clone から `rebuild_user_dictionary` を呼んでも全 clone に反映される
/// (`character_updater::run` 完了後の辞書再構築を spawn タスク内から行うために必要)。
#[derive(Clone)]
pub struct Highlighter {
    /// Lindera トークナイザ。キャラ名のユーザー辞書差し替え(`rebuild_user_dictionary`)が
    /// あるため RwLock で内部可変にしている。
    ///
    /// IPADIC 辞書のビルドは(特に最適化無しの debug ビルドで)数百ms〜数秒かかる重い処理。
    /// `Backend::new()` は LSP のメッセージループが始まる前に同期実行されるため、ここで
    /// 即座に構築すると `initialize` への応答そのものが遅延し、Zed 側のワークスペース
    /// 復元(タブの一括 didOpen)と競合して semantic tokens が塗られないタブが生じる
    /// (実測: debug ビルドで `initialize` 応答が同一マシン上の他の LSP の約10倍かかっていた)。
    /// `OnceLock` にして `new()` ではバックグラウンドスレッドに構築を投げるだけにし、
    /// `initialize` を即座に返せるようにする。実際にトークナイズが必要になった時点
    /// (`tokenizer()`)で初めて `get_or_init` が走り、未完了ならそこだけ待つ
    /// (通常運用ではバックグラウンド構築が先に終わっているためほぼ待たない)。
    tokenizer: std::sync::Arc<std::sync::OnceLock<RwLock<lindera::tokenizer::Tokenizer>>>,
}

impl std::fmt::Debug for crate::highlight::Highlighter {
    // #[instrument(skip(self, f))]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Highlight tokenizer using Lindera")?;
        Ok(())
    }
}

impl Highlighter {
    pub fn new() -> Self {
        let tokenizer = std::sync::Arc::new(std::sync::OnceLock::new());
        {
            let tokenizer = tokenizer.clone();
            std::thread::spawn(move || {
                tokenizer.get_or_init(Self::build_tokenizer);
            });
        }
        Self { tokenizer }
    }

    /// IPADIC 辞書からトークナイザを構築する。`OnceLock::get_or_init` から呼ばれる
    /// ことを前提としており、バックグラウンドスレッドと実利用側のどちらが先に
    /// 呼んでも二重構築は起きない(`OnceLock` が排他する)。
    #[instrument]
    fn build_tokenizer() -> RwLock<lindera::tokenizer::Tokenizer> {
        // IPADIC を使用する設定でトークナイザを作成
        let tokenizer = TokenizerBuilder::new()
            .unwrap()
            .set_segmenter_mode(&Mode::Normal)
            .set_segmenter_dictionary("embedded://ipadic")
            // .set_segmenter_user_dictionary("")
            .build()
            .expect("failed to create lindera tokenizer");
        RwLock::new(tokenizer)
    }

    /// トークナイザ本体を取得する。バックグラウンド構築が未完了ならここで待つ。
    fn tokenizer(&self) -> &RwLock<lindera::tokenizer::Tokenizer> {
        self.tokenizer.get_or_init(Self::build_tokenizer)
    }

    /// 与えられたトークン(品詞details+表層形)がハイライト対象の人名かどうかを判定する。
    /// `classify_normal`/`classify_bracket` のkeyword判定と同一基準であり、hover等
    /// ハイライト以外の箇所でも同じ判定基準を使いたい場合に利用する
    /// (例: `backend.rs` の hover ハンドラ。ハイライトされないトークンでhoverだけ
    /// 表示されるという食い違いを防ぐため)。
    /// `allowed` は呼び出し側が対象ドキュメントのワークスペースに応じて渡す
    /// (`CharacterStore::allowed_names`)。
    #[instrument]
    pub fn is_recognized_name(
        details: &[String],
        surface: &str,
        allowed: &HashSet<String>,
    ) -> bool {
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
    #[instrument(skip(self), ret)]
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
            self.tokenizer().write().segmenter.user_dictionary = None;
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
            let mut tok = self.tokenizer().write();
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
    #[instrument]
    fn user_dict_csv_row(surface: &str, subcategory2: &str, subcategory3: &str) -> String {
        format!(
            "{s},0,0,2000,名詞,固有名詞,{c2},{c3},*,*,{s},*,*",
            s = surface,
            c2 = subcategory2,
            c3 = subcategory3,
        )
    }

    #[instrument(skip(self), ret)]
    pub fn text_to_lindera_token(&self, text: &str) -> Vec<CachedLinderaToken> {
        let tokenizer = self.tokenizer().read();
        let tmp2 = tokenizer
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
                    details: t
                        .details()
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>()
                        .as_chunks::<7>()
                        .0[0]
                        .clone(),
                    byte_start: t.byte_start,
                    byte_end: t.byte_end,
                    meaning: TokenMeaning::Normal,
                })
            })
            .collect();
        tmp2
    }

    #[instrument(ret)]
    fn process_ruby_kanji(
        base: &[CachedLinderaToken],
        car: CachedLinderaToken,
        results: &mut Vec<CachedLinderaToken>,
    ) -> TokenStatus {
        let surface = car.details[6].as_str();
        if is_kanji_all(surface) {
            let mut next_base = base.iter().map(|i| i.clone()).collect::<Vec<_>>();
            next_base.push(car);
            TokenStatus::RubyBaseKanji(next_base)
        } else if surface == "《" {
            TokenStatus::Ruby(base.to_vec(), vec![car.meaning(TokenMeaning::RubyBracket)])
        } else {
            // TODO: ホントはバックトラック
            for tkn in base {
                results.push(tkn.clone());
            }
            results.push(car);
            TokenStatus::Normal
        }
    }

    #[instrument(ret)]
    fn process_ruby_backet(
        base: &[CachedLinderaToken],
        car: CachedLinderaToken,
        results: &mut Vec<CachedLinderaToken>,
    ) -> TokenStatus {
        let surface = car.details[6].as_str();
        if surface == "《" {
            TokenStatus::Ruby(base.to_vec(), vec![car.meaning(TokenMeaning::RubyBracket)])
        } else {
            let mut next_base = base.iter().map(|i| i.clone()).collect::<Vec<_>>();
            next_base.push(car);
            TokenStatus::RubyBaseBracket(next_base)
        }
    }

    #[instrument(skip(self, line), ret)]
    pub fn parse_line_token(
        &self,
        line: &mut LineData,
        initial_state: TokenStatus,
    ) -> (Vec<CachedLinderaToken>, TokenStatus) {
        if line.tokens.is_empty() {
            line.tokens = self.text_to_lindera_token(&line.text);
        }

        let mut tokens: VecDeque<CachedLinderaToken> =
            VecDeque::from_iter(line.tokens.clone().drain(..));
        // let mut next_state = TokenStatus::Initial;
        let mut last_state = initial_state;
        let mut results: Vec<CachedLinderaToken> = vec![];
        // debug!("last_state: {:?}", last_state);

        while !tokens.is_empty() {
            let car = tokens.pop_front().unwrap();

            let surface = car.details[6].clone();
            let types = car.details[0].as_str();
            let subtypes = car.details[1].as_str();
            // debug!("State: {:?} ({:?}, {:?})", last_state, types, subtypes);

            let next_state = match last_state {
                TokenStatus::Normal => match (types, subtypes) {
                    ("記号", "括弧開") => TokenStatus::InBracket(
                        surface.chars().next().unwrap_or_default(),
                        vec![car.meaning(TokenMeaning::Bracket)],
                    ),
                    ("記号", "一般") if surface.as_str() == "｜" => {
                        TokenStatus::RubyBaseBracket(vec![car])
                    }
                    // TODO: 固有名詞処理
                    _ if crate::types::is_kanji_all(surface.as_str()) => {
                        TokenStatus::RubyBaseKanji(vec![car])
                    }
                    _ => {
                        results.push(car.meaning(TokenMeaning::Normal));
                        TokenStatus::Normal
                    }
                },
                TokenStatus::InBracket(brkt, inner) => {
                    let close_brkt = ['〓', '《', '「', '『', '〈', '（', '【', '〔', '｛']
                        .iter()
                        .position(|c| *c == brkt)
                        .map_or('〓', |ix| {
                            ['〓', '》', '」', '』', '〉', '）', '】', '〕', '｝'][ix]
                        });

                    match (types, subtypes) {
                        ("記号", "括弧閉")
                            if close_brkt == surface.chars().next().unwrap_or('〓') =>
                        {
                            for tkn in inner.iter() {
                                results.push(tkn.clone());
                            }
                            results.push(car.meaning(TokenMeaning::BracketClose));
                            TokenStatus::Normal
                        }
                        _ => {
                            // TODO: 括弧内のルビはどうしよう
                            let mut next_inner =
                                inner.iter().map(|i| i.clone()).collect::<Vec<_>>();
                            next_inner.push(car.meaning(TokenMeaning::InnerBracket));
                            TokenStatus::InBracket(brkt, next_inner)
                        }
                    }
                }
                TokenStatus::RubyBaseKanji(base) => {
                    Self::process_ruby_kanji(base.as_slice(), car, &mut results)
                }
                TokenStatus::RubyBaseBracket(base) => {
                    Self::process_ruby_backet(base.as_slice(), car, &mut results)
                }
                TokenStatus::Ruby(base, ruby) => {
                    if surface == "》" {
                        let mut iter = base.iter();
                        results.push(iter.next().unwrap().meaning(TokenMeaning::RubyBracket));
                        while let Some(tkn) = iter.next() {
                            results.push(tkn.meaning(TokenMeaning::RubyBody));
                        }

                        let mut iter = ruby.iter();
                        results.push(iter.next().unwrap().meaning(TokenMeaning::RubyBracket));
                        while let Some(tkn) = iter.next() {
                            results.push(tkn.meaning(TokenMeaning::Ruby));
                        }
                        results.push(car.meaning(TokenMeaning::RubyBracket));
                        TokenStatus::Normal
                    } else {
                        let mut next_ruby = ruby.iter().map(|i| i.clone()).collect::<Vec<_>>();
                        next_ruby.push(car);
                        TokenStatus::Ruby(base, next_ruby)
                    }
                }
                _ => {
                    warn!("Not implemented: {:?}", last_state);
                    TokenStatus::Undefined
                }
            };
            debug!("{:?}", next_state);
            last_state = next_state;
        }
        // TODO: 残ってる分のemit
        match last_state {
            TokenStatus::InBracket(brkt, inner) => {
                // 各要素は追加時点で既に正しい meaning が付いている(上のコメント参照)。
                for tkn in inner.iter() {
                    results.push(tkn.clone());
                }
                last_state = TokenStatus::InBracket(brkt, vec![]);
            }
            // 被ルビは改行をまたいだとしても確定するまで
            _ => {}
        }

        // `results` は `parse_line_token` の戻り値としてのみ meaning を確定させていたため、
        // `line.tokens` を直接読む cursor_context.rs 側からは常に Normal しか見えなかった。
        // ここで書き戻すことで、以後 `line.tokens[i].meaning` を再計算なしに参照できる。
        results
            .iter()
            .zip(line.tokens.iter_mut())
            .for_each(|(l, r)| {
                r.meaning = l.meaning;
            });
        line.state_after = last_state.clone();

        (results, last_state)
    }

    /// `line_no` 行移行の状態を畳み込む。
    /// `apply_changes`などで未解決になっている行を、後方の直近解決済み行(無ければ行0/`Normal`)
    /// から前向きに再計算し、各行の `tokens[].meaning`/`state_after` を更新する。
    #[instrument(skip(self, lines))]
    pub fn ensure_line_state(&self, lines: &mut [LineData], start_line_no: usize) {
        if lines.is_empty() {
            return;
        }
        let line_no = start_line_no.min(lines.len() - 1);
        if lines[line_no].state_after.is_resolved() {
            return; // 高速パス
        }

        // 最寄りの解決済み祖先を後方探索
        let mut start = line_no;
        while start > 0 && !lines[start - 1].state_after.is_resolved() {
            start -= 1;
        }
        // start > 0 なら直前行の状態を引き継ぐ。
        let mut state = if start == 0 {
            TokenStatus::Normal
        } else {
            lines[start - 1].state_after.clone()
        };

        for line in &mut lines[start..=line_no] {
            let (_tokens, s) = self.parse_line_token(line, state);
            state = s;
        }
    }

    /// テキストを受け取り、ハイライト用トークン列と行終端の括弧深さを返す。
    ///
    /// `tag_line_depth` でタグ付けした後、語種と括弧内外に基づいて
    /// ハイライト用のトークン種別を生成する。
    #[instrument(skip(self), ret)]
    pub fn tokenize_with_state(
        &self,
        line: &mut LineData,
        initial_state: TokenStatus,
        coloring: BracketColoring,
        allowed: &HashSet<String>,
    ) -> (Vec<SemanticToken>, TokenStatus) {
        let (mut tokens, last_state) = self.parse_line_token(line, initial_state);

        (
            tokens
                .iter_mut()
                .filter_map(|token| {
                    // `.md` では括弧内外を区別しないため、InnerBracket をいったん無色に
                    // 格下げしてから人名判定を通す(許可名一致なら Characters で復活する)。
                    if coloring == BracketColoring::Uniform
                        && token.meaning == TokenMeaning::InnerBracket
                    {
                        token.meaning = TokenMeaning::Normal;
                    }

                    let surface = &line.text[token.byte_start..token.byte_end];
                    if Self::is_recognized_person_name(&token.details, surface, allowed) {
                        token.meaning = TokenMeaning::Characters;
                    }

                    if token.meaning == TokenMeaning::Normal {
                        return None;
                    }

                    // positionEncoding=utf-16 に合わせ、UTF-16 コード単位で位置と長さを算出する
                    let start = crate::types::utf16_len(&line.text[..token.byte_start]);
                    let length =
                        crate::types::utf16_len(&line.text[token.byte_start..token.byte_end]);

                    Some(SemanticToken::from_meaning(
                        start as u32,
                        length as u32,
                        token.meaning,
                    ))
                })
                .collect::<Vec<_>>(),
            last_state,
        )
    }

    /// トークンが「許可名一致の人名」であるかを判定する共通述語。
    ///
    /// `classify_normal`/`classify_bracket` のkeyword判定条件そのもの
    /// (品詞が固有名詞,人名 かつ 表層形が許可名集合に含まれる)であり、
    /// hover等ハイライト以外の箇所からも同一基準で判定できるよう公開している
    /// (`Highlighter::is_recognized_name` 経由)。
    /// ここでの判定が変わらない限り hover とハイライトは常に一致する。
    #[instrument(ret)]
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

    /// ハイライト用トークン列をLSP用に変換する。
    ///
    #[instrument(skip(tokens))]
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

    /// 空の許可名集合(名前判定を伴わないテスト用)。
    fn no_names() -> HashSet<String> {
        HashSet::new()
    }

    /// 通常モード(括弧外・状態Normal起点)でトークン化するテスト用ヘルパ。
    /// 旧 `h.tokenize(line, &allowed)` 相当。
    fn tokenize(
        h: &Highlighter,
        line: &mut LineData,
        allowed: &HashSet<String>,
    ) -> Vec<SemanticToken> {
        h.tokenize_with_state(
            line,
            TokenStatus::Normal,
            BracketColoring::Distinct,
            allowed,
        )
        .0
    }

    /// `.md` 向け Uniform 配色でトークン化するテスト用ヘルパ。
    fn tokenize_uniform(
        h: &Highlighter,
        line: &mut LineData,
        allowed: &HashSet<String>,
    ) -> Vec<SemanticToken> {
        h.tokenize_with_state(line, TokenStatus::Normal, BracketColoring::Uniform, allowed)
            .0
    }

    /// 括弧内モード(開始状態: `「` が開いたまま)でトークン化するテスト用ヘルパ。
    /// 旧 `tokenize_in_bracket`(深さ1起点)相当。
    fn tokenize_in_bracket(
        h: &Highlighter,
        line: &mut LineData,
        allowed: &HashSet<String>,
    ) -> Vec<SemanticToken> {
        h.tokenize_with_state(
            line,
            TokenStatus::InBracket('「', vec![]),
            BracketColoring::Distinct,
            allowed,
        )
        .0
    }

    #[test]
    fn highlight_token_new() {
        let t = SemanticToken::from_meaning(5, 3, TokenMeaning::Characters);
        assert_eq!(t.start, 5);
        assert_eq!(t.length, 3);
        assert_eq!(t.token_type, SemanticTokenType::Keyword as u32);
        assert_eq!(t.modifier, 0);
    }

    #[test]
    fn test_tokenize_conversation_produces_tokens() {
        let h = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &h,
            &mut LineData::from_str("これはテストです。").unwrap(),
            &no_names(),
        );
        assert!(
            !tokens.is_empty(),
            "tokenize_conversation should produce tokens"
        );
        // 括弧内モードでは、許可名一致以外はすべて string に丸められる。
        assert!(
            tokens
                .iter()
                .all(|t| t.token_type == SemanticTokenType::String as u32),
            "{:?}",
            tokens
        );
    }

    #[test]
    fn test_tokenize_conversation_empty_string() {
        let h = Highlighter::new();
        let tokens = tokenize(&h, &mut LineData::from_str("").unwrap(), &no_names());
        assert!(tokens.is_empty(), "Empty string should produce no tokens");
    }

    #[test]
    fn test_tokenize_conversation_unknown_words() {
        let h = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &h,
            &mut LineData::from_str("がびがび").unwrap(),
            &no_names(),
        );
        // "がびがび" は名詞として扱われるはず
        assert_eq!(tokens[0].token_type, SemanticTokenType::String as u32);
    }

    #[test]
    fn test_tokenize_conversation_complex_sentence() {
        let h = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &h,
            &mut LineData::from_str("吾輩は猫である。名前はまだない。").unwrap(),
            &no_names(),
        );
        assert!(!tokens.is_empty());
    }

    #[test]
    fn test_registered_person_name_is_keyword() {
        // "田中" は Lindera IPADIC で 固有名詞,人名,姓 の単一トークンになる(実測確認済み)。
        // 漢字のみのトークンは行末で「ルビが続くかもしれない」保留状態
        // (`TokenStatus::RubyBaseKanji`)のまま次行を待つ設計のため、単独では確定せず
        // 出力されない。末尾に非漢字トークン(句点)を置いて確定させる。
        let h = Highlighter::new();
        let allowed = HashSet::from(["田中".to_string()]);
        let tokens = tokenize(&h, &mut LineData::from_str("田中。").unwrap(), &allowed);
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
        h.rebuild_user_dictionary(&allowed)
            .expect("辞書再構築に失敗");

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

        // ハイライト結果とも一致すること。"・"のような一般記号は(括弧内外を問わず)
        // 無色化されるため、生き残るのはジョサイア(keyword)のみ。
        let sem = tokenize(&h, &mut LineData::from_str(text).unwrap(), &allowed);
        assert_eq!(sem.len(), 1, "{:?}", sem);
        assert_eq!(sem[0].token_type, SemanticTokenType::Keyword as u32);
    }

    #[test]
    fn test_single_char_registered_name_is_keyword() {
        // 1文字の姓("原")はユーザー辞書登録(コスト3000)によって固有名詞,人名として
        // 単独トークンに切り出され、許可名一致で keyword になる。
        let h = Highlighter::new();
        let allowed = HashSet::from(["原".to_string(), "原顕三郎".to_string()]);
        h.rebuild_user_dictionary(&allowed)
            .expect("辞書再構築に失敗");

        let tokens = tokenize(
            &h,
            &mut LineData::from_str("原は独りごちた").unwrap(),
            &allowed,
        );
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::Keyword as u32,
            "{:?}",
            tokens
        );
        assert_eq!(tokens[0].length, 1, "{:?}", tokens);

        // フルネームは1トークンとして keyword になる(単独名との共存回帰確認)。
        // 全文字が漢字のみだとルビ保留状態のまま確定しないため、末尾に句点を置く。
        let tokens = tokenize(
            &h,
            &mut LineData::from_str("原顕三郎少将。").unwrap(),
            &allowed,
        );
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
        h.rebuild_user_dictionary(&allowed)
            .expect("辞書再構築に失敗");

        let tokens = tokenize(&h, &mut LineData::from_str("シルビア").unwrap(), &allowed);
        assert_eq!(tokens.len(), 1, "{:?}", tokens);
        assert_eq!(tokens[0].token_type, SemanticTokenType::Keyword as u32);
    }

    #[test]
    fn test_single_char_registered_name_does_not_split_common_words() {
        // 1文字人名のユーザー辞書登録が、"高原"/"原因"/"原則"のような一般語を
        // 誤って分割してハイライトしないこと。
        let h = Highlighter::new();
        let allowed = HashSet::from(["原".to_string()]);
        h.rebuild_user_dictionary(&allowed)
            .expect("辞書再構築に失敗");

        for word in ["高原", "原因", "原則"] {
            let tokens = tokenize(&h, &mut LineData::from_str(word).unwrap(), &allowed);
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
        let tokens = tokenize(&h, &mut LineData::from_str("田中").unwrap(), &no_names());
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
        // keyword 化の対象外(「固有名詞,人名」かつ許可名集合に一致した場合のみ)なので、
        // 許可名集合が空なら通常モードでは一切トークンを生成しない。
        let h = Highlighter::new();
        for word in ["自民党", "東京", "日本"] {
            let tokens = tokenize(&h, &mut LineData::from_str(word).unwrap(), &no_names());
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
        let h = Highlighter::new();
        let tokens = tokenize_in_bracket(
            &h,
            &mut LineData::from_str("これはテストです。").unwrap(),
            &no_names(),
        );
        let encoded = Highlighter::to_semantic_tokens([tokens.clone()]);
        assert_eq!(tokens.len(), encoded.len());
        assert!(encoded.len() >= 3, "{:?}", encoded);
        assert_eq!(encoded[1].delta_line, 0);
        assert_eq!(encoded[1].delta_start, tokens[1].start - tokens[0].start);
        assert_eq!(encoded[2].delta_line, 0);
        assert_eq!(encoded[2].delta_start, tokens[2].start - tokens[1].start);
    }

    #[test]
    fn test_encode_semantic_tokens_skips_empty_lines_with_line_gap() {
        let h = Highlighter::new();
        let per_line = ["これはテストです。", "", "これはテストです。"]
            .iter()
            .map(|s| tokenize_in_bracket(&h, &mut LineData::from_str(s).unwrap(), &no_names()))
            .collect::<Vec<_>>();
        let first_line_len = per_line[0].len();
        let encoded = Highlighter::to_semantic_tokens(per_line);
        assert_eq!(encoded.len(), first_line_len * 2, "{:?}", encoded);
        // 2つ目の非空行(line_no=2)は空行(line_no=1)を挟むので、直前の非空トークンからの
        // delta_line は2になる。
        assert_eq!(encoded[first_line_len].delta_line, 2);
        assert_eq!(encoded[first_line_len].delta_start, 0);
    }

    #[test]
    fn test_encode_semantic_tokens_preserves_length_type_modifier() {
        let h = Highlighter::new();
        let source = tokenize_in_bracket(
            &h,
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
        // 通常モード(括弧外)では、許可名集合に無い名詞・助詞は無色トークンとして
        // 一切生成されない。
        let h = Highlighter::new();
        let tokens = tokenize(&h, &mut LineData::from_str("猫は").unwrap(), &no_names());
        assert!(
            tokens.is_empty(),
            "猫は outside bracket should produce no tokens: {:?}",
            tokens
        );
    }

    #[test]
    fn test_bracket_open_and_close_are_comment() {
        // 括弧開・括弧閉自体は "comment"、括弧内本体は "string"
        let h = Highlighter::new();
        let mut l = LineData::from_str("「テスト」").unwrap();
        let tokens = tokenize(&h, &mut l, &no_names());
        // [「(comment), テスト(string), 」(comment)]
        assert_eq!(
            tokens.len(),
            3,
            "「テスト」 should produce 3 tokens: {:?}",
            tokens
        );
        assert_eq!(
            tokens[0].token_type,
            SemanticTokenType::Comment as u32,
            "括弧開「 should be comment"
        );
        assert_eq!(l.tokens[0].meaning, TokenMeaning::Bracket, "{:?}", l.tokens);
        assert_eq!(
            tokens[1].token_type,
            SemanticTokenType::String as u32,
            "テスト(名詞) inside bracket should be string"
        );
        assert_eq!(
            l.tokens[1].meaning,
            TokenMeaning::InnerBracket,
            "{:?}",
            l.tokens
        );
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::Comment as u32,
            "括弧閉」 should be comment"
        );
        assert_eq!(
            l.tokens[2].meaning,
            TokenMeaning::BracketClose,
            "{:?}",
            l.tokens
        );
    }

    #[test]
    fn test_particle_inside_bracket_becomes_string() {
        // 括弧内では助詞も "string" になる。
        // "猫は" で「猫(名詞)」「は(助詞)」に分かれる → 括弧内の「は」が string になるか
        let h = Highlighter::new();
        let tokens = tokenize(
            &h,
            &mut LineData::from_str("「猫は」").unwrap(),
            &no_names(),
        );
        // [「(comment), 猫(string), は(string), 」(comment)]
        assert_eq!(
            tokens.len(),
            4,
            "「猫は」 should produce 4 tokens: {:?}",
            tokens
        );
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::String as u32,
            "助詞「は」 inside bracket should be string"
        );
    }

    #[test]
    fn test_uniform_coloring_in_bracket_matches_outside() {
        // .md 向け: Uniform では括弧内外を区別せず塗る。
        // "「猫は」" は 猫(名詞) と は(助詞) がどちらも許可名集合に無いため無色化され、
        // 残るのは括弧記号2つ(comment)のみ。Distinct(従来の .txt 挙動)では4トークンのまま。
        let h = Highlighter::new();
        let tokens = tokenize_uniform(
            &h,
            &mut LineData::from_str("「猫は」").unwrap(),
            &no_names(),
        );
        assert_eq!(tokens.len(), 2, "{:?}", tokens);
        assert!(
            tokens
                .iter()
                .all(|t| t.token_type == SemanticTokenType::Comment as u32),
            "{:?}",
            tokens
        );

        let distinct_tokens = tokenize(
            &h,
            &mut LineData::from_str("「猫は」").unwrap(),
            &no_names(),
        );
        assert_eq!(
            distinct_tokens.len(),
            4,
            "Distinct(.txt既定)は従来どおり4トークンのまま: {:?}",
            distinct_tokens
        );
    }

    #[test]
    fn test_uniform_coloring_keeps_registered_name_keyword() {
        // 許可名に登録した人名は Uniform の括弧内でも keyword のまま。
        let h = Highlighter::new();
        let allowed = HashSet::from(["田中".to_string()]);
        let tokens = tokenize_uniform(&h, &mut LineData::from_str("「田中」").unwrap(), &allowed);
        // [「(comment), 田中(keyword), 」(comment)]
        assert_eq!(tokens.len(), 3, "{:?}", tokens);
        assert_eq!(
            tokens[1].token_type,
            SemanticTokenType::Keyword as u32,
            "{:?}",
            tokens
        );
    }

    #[test]
    fn test_uniform_coloring_does_not_change_meaning_tags() {
        // 色分けだけが変わり、括弧の意味付け(補完モード判定が依存する Bracket/InnerBracket
        // /BracketClose)自体は変化しないこと。
        let h = Highlighter::new();
        let mut line = LineData::from_str("「猫は」").unwrap();
        let (_, end_state) = h.tokenize_with_state(
            &mut line,
            TokenStatus::Normal,
            BracketColoring::Uniform,
            &no_names(),
        );
        assert_eq!(end_state, TokenStatus::Normal);
        assert_eq!(line.tokens[0].meaning, TokenMeaning::Bracket); // 「
        assert_eq!(line.tokens[1].meaning, TokenMeaning::InnerBracket); // 猫
        assert_eq!(line.tokens[2].meaning, TokenMeaning::InnerBracket); // は
        assert_eq!(line.tokens[3].meaning, TokenMeaning::BracketClose); // 」
    }

    #[test]
    fn test_bracket_mode_persists_across_tokenize_calls() {
        // 複数行にまたがる括弧で、状態を戻り値で次の行へ引き継ぐ
        let h = Highlighter::new();
        let names = no_names();
        let (_, s) = h.tokenize_with_state(
            &mut LineData::from_str("「猫").unwrap(),
            TokenStatus::Normal,
            BracketColoring::Distinct,
            &names,
        ); // 括弧開 → InBracket
        assert!(
            matches!(s, TokenStatus::InBracket(brkt, _) if brkt == '「'),
            "{:?}",
            s
        );

        let (inside, s) = h.tokenize_with_state(
            &mut LineData::from_str("猫は").unwrap(),
            s,
            BracketColoring::Distinct,
            &names,
        ); // 括弧内 → は が "string"
        assert_eq!(
            inside.len(),
            2,
            "猫は inside bracket (cross-line) should produce 2 tokens: {:?}",
            inside
        );
        assert_eq!(
            inside[1].token_type,
            SemanticTokenType::String as u32,
            "助詞 should be string when inside bracket across lines"
        );

        let (_, s) = h.tokenize_with_state(
            &mut LineData::from_str("」").unwrap(),
            s,
            BracketColoring::Distinct,
            &names,
        ); // 括弧閉 → Normal
        assert_eq!(s, TokenStatus::Normal);

        let (outside, _) = h.tokenize_with_state(
            &mut LineData::from_str("猫は").unwrap(),
            s,
            BracketColoring::Distinct,
            &names,
        ); // 括弧外 → 無色化されトークンなし
        assert_eq!(
            outside.len(),
            0,
            "猫は outside bracket should produce no tokens: {:?}",
            outside
        );
    }

    #[test]
    fn test_nested_brackets_depth() {
        // ネストした括弧([外側「」]の内側にもう一段開く)は現設計(TokenStatusは
        // 単一レベル)では区別できず、内側の開き括弧は単なる本文(InnerBracket)として
        // 扱われてしまう。それでも外側の「」自体の開閉は正しく追えるため、
        // 外側の括弧が閉じるまでは string で塗られ続ける(この点だけを確認する)。
        let h = Highlighter::new();
        let names = no_names();
        let (_, s) = h.tokenize_with_state(
            &mut LineData::from_str("「").unwrap(),
            TokenStatus::Normal,
            BracketColoring::Distinct,
            &names,
        );
        let (_, s) = h.tokenize_with_state(
            &mut LineData::from_str("「").unwrap(),
            s,
            BracketColoring::Distinct,
            &names,
        );
        let (inner, _) = h.tokenize_with_state(
            &mut LineData::from_str("猫は").unwrap(),
            s,
            BracketColoring::Distinct,
            &names,
        );
        assert_eq!(inner.len(), 2, "should be in bracket mode at nested depth");
    }

    #[test]
    fn test_ensure_line_state_folds_from_line0() {
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = ["「セリフ１」", "「セリフ２"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();

        h.ensure_line_state(&mut lines, 1);

        // 行0は閉じて Normal、行1は開きっぱなしで InBracket('「', _) のまま終わる。
        assert_eq!(lines[0].state_after, TokenStatus::Normal);
        assert!(
            matches!(&lines[1].state_after, TokenStatus::InBracket(brkt, _) if *brkt == '「'),
            "{:?}",
            lines[1].state_after
        );

        // 行0 の「セリフ１」中身は InnerBracket、開き括弧「と閉じ括弧」は Bracket/BracketClose。
        assert_eq!(
            lines[0].tokens.first().unwrap().meaning,
            TokenMeaning::Bracket
        );
        assert!(
            lines[0]
                .tokens
                .iter()
                .skip(1)
                .take(lines[0].tokens.len() - 2)
                .all(|t| t.meaning == TokenMeaning::InnerBracket),
            "{:?}",
            lines[0].tokens
        );
        assert_eq!(
            lines[0].tokens.last().unwrap().meaning,
            TokenMeaning::BracketClose
        );
    }

    #[test]
    fn test_ensure_line_state_refold_after_invalidation() {
        // 陳腐化修復: 上方の行が変わって以降のキャッシュが Undefined 化されたとき、
        // 再フォールドで新しい meaning に更新されること。
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = ["こんにちは。", "猫は"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();
        h.ensure_line_state(&mut lines, 1);
        assert!(
            lines[1]
                .tokens
                .iter()
                .all(|t| t.meaning == TokenMeaning::Normal)
        );

        // 行0 を「こんにちは(開きっぱなし)へ編集 → apply_changes 相当の無効化
        lines[0] = LineData::from_str("「こんにちは。").unwrap();
        lines[1].state_after = TokenStatus::Undefined; // 編集行以降の一括クリア相当

        // 再フォールドすると行1 は括弧内になる(meaning も InnerBracket に更新される)
        h.ensure_line_state(&mut lines, 1);
        assert!(
            lines[1]
                .tokens
                .iter()
                .all(|t| t.meaning == TokenMeaning::InnerBracket),
            "{:?}",
            lines[1].tokens
        );
    }

    #[test]
    fn test_ensure_line_state_fast_path_skips_refold() {
        // state_after が解決済みの行は再畳み込みされない(高速パス)。
        // 意図的に矛盾した状態を仕込み、それが保持されることで「呼ばれていない」ことを観測する。
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = ["「セリフ", "猫は"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();
        h.ensure_line_state(&mut lines, 1); // 行1は括弧内 → InnerBracket

        // 毒を仕込む: 行0のテキストは括弧を開いたままだが、状態だけ Normal と偽る。
        lines[0].state_after = TokenStatus::Normal;

        // 行1 は既に解決済みなので高速パスに入り、行0 の偽状態は参照されないはず。
        h.ensure_line_state(&mut lines, 1);
        assert!(
            lines[1]
                .tokens
                .iter()
                .all(|t| t.meaning == TokenMeaning::InnerBracket),
            "解決済みの行は再畳み込みされないはず: {:?}",
            lines[1].tokens
        );
        assert_eq!(
            lines[0].state_after,
            TokenStatus::Normal,
            "先行行も再計算されないはず"
        );
    }

    #[test]
    fn test_ensure_line_state_resumes_from_previous_state() {
        // 後方探索で見つかった解決済み行の state_after を起点に畳み込むこと
        // (旧実装は start > 0 でも常に Normal 起点にフォールバックしていたバグの回帰防止)。
        let h = Highlighter::new();
        let mut lines: Vec<LineData> = ["「セリフ", "猫は"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();
        h.ensure_line_state(&mut lines, 0); // 行0のみ畳み込み(InBracketで終わる)
        assert!(matches!(&lines[0].state_after, TokenStatus::InBracket(..)));
        assert_eq!(lines[1].state_after, TokenStatus::Undefined);

        h.ensure_line_state(&mut lines, 1); // start==1 で行0の状態を継承するはず
        assert!(
            lines[1]
                .tokens
                .iter()
                .all(|t| t.meaning == TokenMeaning::InnerBracket),
            "直前行の状態が引き継がれていない: {:?}",
            lines[1].tokens
        );
        assert!(matches!(&lines[1].state_after, TokenStatus::InBracket(brkt, _) if *brkt == '「'));
    }

    #[test]
    fn test_ensure_line_state_handles_empty_and_out_of_range() {
        let h = Highlighter::new();
        let mut empty: Vec<LineData> = vec![];
        h.ensure_line_state(&mut empty, 0); // panicしないこと

        let mut lines: Vec<LineData> = ["「セリフ１」", "「セリフ２"]
            .iter()
            .map(|s| LineData::from_str(s).unwrap())
            .collect();
        h.ensure_line_state(&mut lines, 999); // 範囲外はクランプされ、最終行まで畳み込まれる
        assert!(
            matches!(&lines[1].state_after, TokenStatus::InBracket(brkt, _) if *brkt == '「'),
            "{:?}",
            lines[1].state_after
        );
    }

    #[test]
    fn test_ruby_form_a_with_bar_coloring() {
        // ｜かな《ルビ》: ｜/《/》がcomment、ルビ本体がstring
        let h = Highlighter::new();
        let tokens = tokenize(
            &h,
            &mut LineData::from_str("｜てー《撃て》").unwrap(),
            &no_names(),
        );
        assert!(!tokens.is_empty(), "{:?}", tokens);
        assert_eq!(
            tokens.first().unwrap().token_type,
            SemanticTokenType::Comment as u32,
            "｜ should be comment: {:?}",
            tokens
        );
        assert_eq!(
            tokens.last().unwrap().token_type,
            SemanticTokenType::Comment as u32,
            "》 should be comment: {:?}",
            tokens
        );
        assert!(
            tokens
                .iter()
                .any(|t| t.token_type == SemanticTokenType::String as u32),
            "ルビ本体がstringであるはず: {:?}",
            tokens
        );
    }

    #[test]
    fn test_ruby_excluded_from_bracket_depth() {
        // 漢字《ルビ》は括弧(台詞)ではなくルビとして扱われ、Bracket/InnerBracket/BracketClose
        // には分類されない。
        let h = Highlighter::new();
        let mut line = LineData::from_str("漢字《ルビ》").unwrap();
        let (_, end_state) = h.tokenize_with_state(
            &mut line,
            TokenStatus::Normal,
            BracketColoring::Distinct,
            &no_names(),
        );
        assert_eq!(
            end_state,
            TokenStatus::Normal,
            "ルビは行末で状態を残さないこと"
        );
        assert!(
            line.tokens.iter().all(|t| !matches!(
                t.meaning,
                TokenMeaning::Bracket | TokenMeaning::InnerBracket | TokenMeaning::BracketClose
            )),
            "{:?}",
            line.tokens
        );
    }

    #[test]
    fn test_non_ruby_bracket_still_comment_and_affects_depth() {
        // 《強調》(行頭、親文字なし)は従来どおり普通の括弧として扱われる。
        let h = Highlighter::new();
        let mut line = LineData::from_str("《強調》").unwrap();
        let (tokens, end_state) = h.tokenize_with_state(
            &mut line,
            TokenStatus::Normal,
            BracketColoring::Distinct,
            &no_names(),
        );
        assert_eq!(
            end_state,
            TokenStatus::Normal,
            "開いて閉じるので最終状態はNormal"
        );
        assert_eq!(tokens[0].token_type, SemanticTokenType::Comment as u32); // 《
        assert_eq!(
            tokens.last().unwrap().token_type,
            SemanticTokenType::Comment as u32
        ); // 》
        assert_eq!(
            line.tokens[0].meaning,
            TokenMeaning::Bracket,
            "非ルビの《》は普通の括弧として扱われること: {:?}",
            line.tokens
        );
    }

    #[test]
    fn test_ruby_body_stays_string_in_uniform_coloring() {
        // .md(Uniform)でもルビ本体はstringのまま(括弧内外統一ルールの意図的な例外)。
        // 親文字(漢字)の先頭トークンは `Ruby` 状態が base の先頭要素を「開き括弧枠」
        // として扱う実装上、RubyBracket(comment)になる(base が本来のInBracketの
        // 開き括弧ではなく漢字本文であるケースの既知の粗さ。ここでは現状の挙動を
        // 固定してテストする)。
        let h = Highlighter::new();
        let tokens = tokenize_uniform(
            &h,
            &mut LineData::from_str("漢字《かんじ》").unwrap(),
            &no_names(),
        );
        assert_eq!(tokens.len(), 4, "{:?}", tokens);
        assert_eq!(
            tokens[2].token_type,
            SemanticTokenType::String as u32,
            "ルビ本体はUniformでもstringのまま: {:?}",
            tokens
        );
    }

    #[test]
    fn test_ruby_inside_dialogue_does_not_increase_depth() {
        // 「漢字《かんじ》だ」: 台詞の中にルビがあっても」で正しく閉じること。
        let h = Highlighter::new();
        let mut line = LineData::from_str("「漢字《かんじ》だ」").unwrap();
        let (_, end_state) = h.tokenize_with_state(
            &mut line,
            TokenStatus::Normal,
            BracketColoring::Distinct,
            &no_names(),
        );
        assert_eq!(
            end_state,
            TokenStatus::Normal,
            "」で閉じるので最終状態はNormal"
        );
    }

    // --- parse_line_token の状態遷移網羅テスト ---
    //
    // Lindera の実際の分割結果に依存せず状態機械だけを検証するため、`line.tokens` へ
    // 手組みのトークンを直接注入する。`parse_line_token` は `line.text`/`byte_start`/
    // `byte_end` を一切参照しない(位置計算は呼び出し元の `tokenize_with_state` の仕事)
    // ため、これらは全て 0 で構わない。
    //
    // 現状の実装をそのまま固定するテストであり、既知の未修正の粗さ(R1の項参照)も
    // あえて期待値として書いている(修正はスコープ外、回帰検知のためだけに固定する)。

    /// 品詞・細分類・表層形(=`details[6]`、状態機械が実際に見る値)だけを指定して
    /// 手組みのトークンを作る。
    fn mk(types: &str, subtypes: &str, surface: &str) -> CachedLinderaToken {
        CachedLinderaToken {
            details: [
                types.to_string(),
                subtypes.to_string(),
                "*".to_string(),
                "*".to_string(),
                "*".to_string(),
                "*".to_string(),
                surface.to_string(),
            ],
            byte_start: 0,
            byte_end: 0,
            meaning: TokenMeaning::Normal,
        }
    }

    /// `mk` に加えて `meaning` を明示指定する(状態のペイロードとして持たせる
    /// 「既に分類済みのトークン」を組み立てるため)。
    fn mk_with_meaning(
        types: &str,
        subtypes: &str,
        surface: &str,
        meaning: TokenMeaning,
    ) -> CachedLinderaToken {
        let mut t = mk(types, subtypes, surface);
        t.meaning = meaning;
        t
    }

    /// `TokenStatus` のバリアント名(+括弧文字+ペイロード件数)だけを文字列化する。
    /// 中身のトークンの中身までは比較しない(件数だけで十分に遷移を区別できる)。
    fn state_label(s: &TokenStatus) -> String {
        match s {
            TokenStatus::Normal => "Normal".to_string(),
            TokenStatus::InBracket(c, inner) => format!("InBracket({c:?}, {}個)", inner.len()),
            TokenStatus::InLine(c, inner) => format!("InLine({c:?}, {}個)", inner.len()),
            TokenStatus::RubyBaseKanji(base) => format!("RubyBaseKanji({}個)", base.len()),
            TokenStatus::RubyBaseBracket(base) => format!("RubyBaseBracket({}個)", base.len()),
            TokenStatus::Ruby(base, ruby) => {
                format!("Ruby(base={}個, ruby={}個)", base.len(), ruby.len())
            }
            TokenStatus::Undefined => "Undefined".to_string(),
        }
    }

    /// `parse_line_token` が返す `results` を (表層形, meaning) の列へ変換する。
    fn emitted(results: &[CachedLinderaToken]) -> Vec<(String, TokenMeaning)> {
        results
            .iter()
            .map(|t| (t.details[6].clone(), t.meaning))
            .collect()
    }

    /// 手組みトークン列を1回の `parse_line_token` 呼び出しに通す。
    fn run(
        h: &Highlighter,
        initial: TokenStatus,
        input: &[(&str, &str, &str)],
    ) -> (Vec<CachedLinderaToken>, TokenStatus) {
        let mut line = LineData::from_str("").unwrap();
        line.tokens = input.iter().map(|(t, s, surf)| mk(t, s, surf)).collect();
        h.parse_line_token(&mut line, initial)
    }

    #[test]
    fn test_parse_line_token_state_transition_table() {
        let h = Highlighter::new();

        struct Case {
            name: &'static str,
            initial: TokenStatus,
            input: Vec<(&'static str, &'static str, &'static str)>,
            want_state: &'static str,
            want_emitted: Vec<(&'static str, TokenMeaning)>,
        }

        let cases: Vec<Case> = vec![
            Case {
                name: "N1: Normal+記号,括弧開 → InBracket(単発呼び出しなので行末flush(E1)も同時に踏む)",
                initial: TokenStatus::Normal,
                input: vec![("記号", "括弧開", "「")],
                want_state: "InBracket('「', 0個)",
                want_emitted: vec![("「", TokenMeaning::Bracket)],
            },
            Case {
                name: "N2: Normal+記号,一般(｜) → RubyBaseBracket",
                initial: TokenStatus::Normal,
                input: vec![("記号", "一般", "｜")],
                want_state: "RubyBaseBracket(1個)",
                want_emitted: vec![],
            },
            Case {
                name: "N3: Normal+漢字 → RubyBaseKanji",
                initial: TokenStatus::Normal,
                input: vec![("名詞", "一般", "漢")],
                want_state: "RubyBaseKanji(1個)",
                want_emitted: vec![],
            },
            Case {
                name: "N4: Normal+その他 → Normal(即出力)",
                initial: TokenStatus::Normal,
                input: vec![("助詞", "格助詞", "は")],
                want_state: "Normal",
                want_emitted: vec![("は", TokenMeaning::Normal)],
            },
            Case {
                name: "B1: InBracket+対応する括弧閉 → Normal",
                initial: TokenStatus::InBracket(
                    '「',
                    vec![
                        mk_with_meaning("記号", "括弧開", "「", TokenMeaning::Bracket),
                        mk_with_meaning("感嘆符", "*", "こんにちは", TokenMeaning::InnerBracket),
                    ],
                ),
                input: vec![("記号", "括弧閉", "」")],
                want_state: "Normal",
                want_emitted: vec![
                    ("「", TokenMeaning::Bracket),
                    ("こんにちは", TokenMeaning::InnerBracket),
                    ("」", TokenMeaning::BracketClose),
                ],
            },
            Case {
                name: "B2: InBracket+その他 → InBracket継続(単発呼び出しなのでE1も同時に踏む)",
                initial: TokenStatus::InBracket(
                    '「',
                    vec![mk_with_meaning(
                        "記号",
                        "括弧開",
                        "「",
                        TokenMeaning::Bracket,
                    )],
                ),
                input: vec![("名詞", "一般", "猫")],
                want_state: "InBracket('「', 0個)",
                want_emitted: vec![
                    ("「", TokenMeaning::Bracket),
                    ("猫", TokenMeaning::InnerBracket),
                ],
            },
            Case {
                name: "K1: RubyBaseKanji+漢字 → RubyBaseKanji継続",
                initial: TokenStatus::RubyBaseKanji(vec![mk("名詞", "一般", "漢")]),
                input: vec![("名詞", "一般", "字")],
                want_state: "RubyBaseKanji(2個)",
                want_emitted: vec![],
            },
            Case {
                name: "K2: RubyBaseKanji+《 → Ruby",
                initial: TokenStatus::RubyBaseKanji(vec![mk("名詞", "一般", "漢")]),
                input: vec![("記号", "括弧開", "《")],
                want_state: "Ruby(base=1個, ruby=1個)",
                want_emitted: vec![],
            },
            Case {
                name: "K3: RubyBaseKanji+その他 → Normal(バックトラック未実装、貯めた分をそのまま出力)",
                initial: TokenStatus::RubyBaseKanji(vec![mk("名詞", "一般", "漢")]),
                input: vec![("助詞", "格助詞", "は")],
                want_state: "Normal",
                want_emitted: vec![("漢", TokenMeaning::Normal), ("は", TokenMeaning::Normal)],
            },
            Case {
                name: "P1: RubyBaseBracket+《 → Ruby",
                initial: TokenStatus::RubyBaseBracket(vec![mk("記号", "一般", "｜")]),
                input: vec![("記号", "括弧開", "《")],
                want_state: "Ruby(base=1個, ruby=1個)",
                want_emitted: vec![],
            },
            Case {
                name: "P2: RubyBaseBracket+その他 → RubyBaseBracket継続",
                initial: TokenStatus::RubyBaseBracket(vec![mk("記号", "一般", "｜")]),
                input: vec![("名詞", "一般", "か")],
                want_state: "RubyBaseBracket(2個)",
                want_emitted: vec![],
            },
            Case {
                name: "R1: Ruby+》 → Normal(既知の粗さ: base[0]は無条件でRubyBracket化される。\
                    ｜形式ならbase[0]は本当に｜なので正しいが、漢字形式だと親文字自身が\
                    RubyBracket=Comment色になってしまう。今回は修正せず現状の挙動を固定する)",
                initial: TokenStatus::Ruby(
                    vec![mk("記号", "一般", "｜")],
                    vec![mk_with_meaning(
                        "記号",
                        "括弧開",
                        "《",
                        TokenMeaning::RubyBracket,
                    )],
                ),
                input: vec![("名詞", "一般", "ル"), ("記号", "括弧閉", "》")],
                want_state: "Normal",
                want_emitted: vec![
                    ("｜", TokenMeaning::RubyBracket),
                    ("《", TokenMeaning::RubyBracket),
                    ("ル", TokenMeaning::Ruby),
                    ("》", TokenMeaning::RubyBracket),
                ],
            },
            Case {
                name: "R2: Ruby+その他 → Ruby継続",
                initial: TokenStatus::Ruby(
                    vec![mk("記号", "一般", "｜")],
                    vec![mk_with_meaning(
                        "記号",
                        "括弧開",
                        "《",
                        TokenMeaning::RubyBracket,
                    )],
                ),
                input: vec![("名詞", "一般", "ル")],
                want_state: "Ruby(base=1個, ruby=2個)",
                want_emitted: vec![],
            },
            Case {
                name: "U1: InLine(未実装の状態)+何か → Undefined(warn!して打ち切り)",
                initial: TokenStatus::InLine('x', vec![]),
                input: vec![("名詞", "一般", "あ")],
                want_state: "Undefined",
                want_emitted: vec![],
            },
        ];

        for c in cases {
            let (results, state) = run(&h, c.initial, &c.input);
            assert_eq!(state_label(&state), c.want_state, "{}: 状態", c.name);
            let want_emitted: Vec<(String, TokenMeaning)> = c
                .want_emitted
                .iter()
                .map(|(s, m)| (s.to_string(), *m))
                .collect();
            assert_eq!(emitted(&results), want_emitted, "{}: 出力", c.name);
        }
    }

    // E1/E2(行をまたぐ持ち越し状態からの再開)は1回の parse_line_token 呼び出しでは
    // 表現できない(2回連続で呼んで初めて意味を持つ)ため、テーブルとは別に検証する。

    #[test]
    fn test_parse_line_token_e1_resumes_persisted_bracket_across_calls() {
        // 1回目: 「を開いて行末 → InBracket('「', 空)を持ち越す(E1)。
        let h = Highlighter::new();
        let (_, carried) = run(&h, TokenStatus::Normal, &[("記号", "括弧開", "「")]);
        assert_eq!(state_label(&carried), "InBracket('「', 0個)");

        // 2回目: 持ち越した状態から再開。1回目で確定済みの「自身はもう results に出ない
        // (2回目の呼び出しでは新規トークンの分だけが出力される)ことを確認する。
        let (results, state) = run(&h, carried, &[("名詞", "一般", "猫")]);
        assert_eq!(state_label(&state), "InBracket('「', 0個)");
        assert_eq!(
            emitted(&results),
            vec![("猫".to_string(), TokenMeaning::InnerBracket)]
        );
    }

    #[test]
    fn test_parse_line_token_e1b_resumes_persisted_bracket_across_calls() {
        // 1回目: 「を開いて行末 → InBracket('「', 空)を持ち越す(E1)。
        let h = Highlighter::new();
        let (result, carried) = run(
            &h,
            TokenStatus::Normal,
            &[("記号", "括弧開", "「"), ("感動詞", "*", "こんにちは")],
        );
        assert_eq!(state_label(&carried), "InBracket('「', 0個)");
        assert_eq!(
            emitted(&result),
            vec![
                ("「".to_string(), TokenMeaning::Bracket),
                ("こんにちは".to_string(), TokenMeaning::InnerBracket)
            ]
        );

        // 2回目: 持ち越した状態から再開。1回目で確定済みの「自身はもう results に出ない
        // (2回目の呼び出しでは新規トークンの分だけが出力される)ことを確認する。
        let (results, state) = run(&h, carried, &[("記号", "括弧閉", "」")]);
        assert_eq!(state_label(&state), "Normal");
        assert_eq!(
            emitted(&results),
            vec![("」".to_string(), TokenMeaning::BracketClose)]
        );
    }

    #[test]
    fn test_parse_line_token_e2_resumes_persisted_ruby_base_across_calls() {
        // 1回目: 漢字1文字だけで行末 → RubyBaseKanji を持ち越す(確定しない=無出力)。
        let h = Highlighter::new();
        let (results1, carried) = run(&h, TokenStatus::Normal, &[("名詞", "一般", "漢")]);
        assert_eq!(state_label(&carried), "RubyBaseKanji(1個)");
        assert!(results1.is_empty(), "確定前は無出力: {:?}", results1);

        // 2回目: 持ち越した親文字が、次の行の《で正しくルビ確定フローへ合流すること。
        let (results2, state) = run(&h, carried, &[("記号", "括弧開", "《")]);
        assert_eq!(state_label(&state), "Ruby(base=1個, ruby=1個)");
        assert!(
            results2.is_empty(),
            "まだ》が来ていないので無出力: {:?}",
            results2
        );
    }
}
