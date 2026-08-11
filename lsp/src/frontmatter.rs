//! frontmatter(`gray_matter::Pod`)から JSON 互換の値への変換ユーティリティ。
//!
//! `main.rs` から切り出したモジュール。プロンプトテンプレートの frontmatter は
//! YAML で書かれるため、スキーマ定義のような配列・オブジェクトも
//! 文字列(JSON)として一度フラット化してから利用側へ渡す。

use std::collections::HashMap;
use tracing::instrument;

/// アセットからプロンプトを読み込み、YAML frontmatter を本文と分離して返す。
///
/// - アセットが見つからない場合は `None`(欠如時の扱いは呼び出し側に委ねる)。
/// - frontmatter が無い・パースに失敗した場合は本文をそのまま・空の data を返す。
#[instrument]
pub(crate) fn load_prompt(name: &str) -> Option<(String, HashMap<String, String>)> {
    let template = crate::assets::load(name)?;
    let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
    // いったん Pod として受け、スカラー値を文字列へ正規化する。
    // HashMap<String, String> へ直接デシリアライズすると、frontmatter に数値や真偽値が
    // 1つでもあると(例: `max_tokens: 4096`)全体のパースが失敗し、`schema` 等も巻き添えで
    // 失われてしまうため(option は use_llm_with_option 側で文字列から parse し直す前提)。
    let parsed = match matter.parse::<gray_matter::Pod>(&template) {
        Ok(p) => p,
        Err(_) => return Some((template, HashMap::new())),
    };
    let body = parsed.content;
    let mut data = HashMap::new();
    if let Some(gray_matter::Pod::Hash(hash)) = parsed.data {
        for (k, v) in hash {
            if let Some(s) = pod_to_string(&v) {
                data.insert(k, s);
            }
        }
    }
    Some((body, data))
}

/// frontmatter の Pod を文字列へ正規化する。
///
/// スカラーはそのまま文字列化し、配列・オブジェクト(JSON schema 等)は JSON 文字列に変換する。
/// Null のみ `None`(マップから除外)。
#[instrument]
pub(crate) fn pod_to_string(pod: &gray_matter::Pod) -> Option<String> {
    use gray_matter::Pod;
    match pod {
        Pod::String(s) => Some(s.clone()),
        Pod::Integer(n) => Some(n.to_string()),
        Pod::Float(f) => Some(f.to_string()),
        Pod::Boolean(b) => Some(b.to_string()),
        Pod::Null => None,
        Pod::Array(_) | Pod::Hash(_) => serde_json::to_string(&pod_to_json_value(pod)).ok(),
    }
}

/// `{{NAME}}` プレースホルダを一度の走査で置換する。
///
/// `String::replace` を複数キーに対して連鎖させると、先の置換で埋め込まれた
/// 値の中に別のプレースホルダ表記(例: 小説本文が偶然 `{{CHAPTER}}` という
/// 文字列を含む場合)が紛れ込んでいたとき、後続の `replace` がそれも書き換えて
/// しまう(テンプレートインジェクション)。この関数は出力バッファを一度しか
/// 構築せず、置換後の文字列を再走査しないためその心配がない。
///
/// 未知のプレースホルダ(`vars` に無いキー)は `{{NAME}}` の形のまま残す。
#[instrument]
pub(crate) fn expand(template: &str, vars: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        match after_open.find("}}") {
            Some(end) => {
                let key = &after_open[..end];
                match vars.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push_str("{{");
                        out.push_str(key);
                        out.push_str("}}");
                    }
                }
                rest = &after_open[end + 2..];
            }
            None => {
                // 閉じ "}}" が見つからない: 開き "{{" 以降をそのまま残して終了
                out.push_str("{{");
                rest = after_open;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[instrument]
pub(crate) fn pod_to_json_value(pod: &gray_matter::Pod) -> serde_json::Value {
    use gray_matter::Pod;
    match pod {
        Pod::Null => serde_json::Value::Null,
        Pod::String(s) => serde_json::Value::String(s.clone()),
        Pod::Integer(n) => serde_json::json!(n),
        Pod::Float(f) => serde_json::json!(f),
        Pod::Boolean(b) => serde_json::Value::Bool(*b),
        Pod::Array(items) => {
            serde_json::Value::Array(items.iter().map(pod_to_json_value).collect())
        }
        Pod::Hash(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), pod_to_json_value(v));
            }
            serde_json::Value::Object(obj)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;
    use std::collections::HashMap;

    /// load_prompt と同じ frontmatter 正規化を文字列入力で再現する(Asset 非依存のテスト用)。
    fn parse_frontmatter(template: &str) -> HashMap<String, String> {
        let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
        let parsed = matter.parse::<gray_matter::Pod>(template).unwrap();
        let mut data = HashMap::new();
        if let Some(gray_matter::Pod::Hash(hash)) = parsed.data {
            for (k, v) in hash {
                if let Some(s) = pod_to_string(&v) {
                    data.insert(k, s);
                }
            }
        }
        data
    }

    // frontmatter に数値・真偽値が混在しても、文字列フィールド(schema 等)が
    // 巻き添えで失われないこと(EOF schema バグの回帰防止)。
    #[test]
    fn test_frontmatter_mixed_scalar_types() {
        let fm = parse_frontmatter(indoc!(
            r#"---
            schema: >
              {"type":"object"}
            schema_name: character_updates
            max_tokens: 4096
            temperature: 0.3
            enabled: true
            ---
            body text"#
        ));
        // 数値の隣にあっても schema(文字列)が生き残る
        assert_eq!(
            fm.get("schema").map(|s| s.trim()),
            Some(r#"{"type":"object"}"#)
        );
        assert_eq!(
            fm.get("schema_name").map(|s| s.as_str()),
            Some("character_updates")
        );
        // 数値・真偽値は文字列化されて取得できる(use_llm_with_option が parse し直す)
        assert_eq!(fm.get("max_tokens").map(|s| s.as_str()), Some("4096"));
        assert_eq!(fm.get("temperature").map(|s| s.as_str()), Some("0.3"));
        assert_eq!(fm.get("enabled").map(|s| s.as_str()), Some("true"));
    }

    #[test]
    fn test_pod_to_string() {
        use gray_matter::Pod;
        assert_eq!(pod_to_string(&Pod::Integer(42)), Some("42".to_string()));
        assert_eq!(
            pod_to_string(&Pod::Boolean(false)),
            Some("false".to_string())
        );
        assert_eq!(
            pod_to_string(&Pod::String("x".into())),
            Some("x".to_string())
        );
        assert_eq!(pod_to_string(&Pod::Null), None);
        assert_eq!(pod_to_string(&Pod::Array(vec![])), Some("[]".to_string()));
        let mut schema = Pod::new_hash();
        schema["type"] = Pod::String("object".into());
        let s = pod_to_string(&schema).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "object");
    }

    // schema を YAML オブジェクトとして書いた場合も JSON 文字列として取得できる。
    #[test]
    fn test_frontmatter_schema_as_yaml_object() {
        let fm = parse_frontmatter(indoc!(
            r#"---
            schema:
              type: object
              properties:
                updates:
                  type: array
            schema_name: character_updates
            max_tokens: 4096
            ---
            body"#
        ));
        let schema_str = fm.get("schema").expect("schema missing");
        let schema: serde_json::Value = serde_json::from_str(schema_str).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["updates"]["type"], "array");
        assert_eq!(fm.get("max_tokens").map(|s| s.as_str()), Some("4096"));
    }

    // ---- expand のテスト ----

    #[test]
    fn test_expand_replaces_multiple_placeholders() {
        let mut vars = HashMap::new();
        vars.insert("CHAPTER", "5");
        vars.insert("TEXT", "本文");
        assert_eq!(expand("第{{CHAPTER}}章: {{TEXT}}", &vars), "第5章: 本文");
    }

    #[test]
    fn test_expand_does_not_double_expand_injected_placeholder() {
        // 回帰: 本文が偶然 "{{CHAPTER}}" という文字列を含んでいても、
        // TEXT展開後の結果を再走査して二重展開しないこと
        // (String::replaceの連鎖だとこれが起きる)。
        let mut vars = HashMap::new();
        vars.insert("TEXT", "a{{CHAPTER}}b");
        vars.insert("CHAPTER", "X");
        assert_eq!(
            expand("{{TEXT}}", &vars),
            "a{{CHAPTER}}b",
            "TEXT展開結果に含まれる{{CHAPTER}}は再展開されないこと"
        );
    }

    #[test]
    fn test_expand_leaves_unknown_placeholder_untouched() {
        let vars = HashMap::new();
        assert_eq!(
            expand("見出し{{UNKNOWN}}続き", &vars),
            "見出し{{UNKNOWN}}続き"
        );
    }

    #[test]
    fn test_expand_no_placeholder_returns_as_is() {
        let vars = HashMap::new();
        assert_eq!(expand("プレースホルダなし", &vars), "プレースホルダなし");
    }

    #[test]
    fn test_expand_unclosed_placeholder_left_as_is() {
        let vars = HashMap::new();
        assert_eq!(expand("前{{未閉じ", &vars), "前{{未閉じ");
    }

    /// `expand` は未知のプレースホルダを `{{NAME}}` のまま残すため、テンプレートに
    /// 変数を足したのに呼び出し側が渡し忘れると、リテラルがプロンプトへ漏れる。
    /// 実テンプレートと、呼び出し側(`Backend::completion` / `Backend::code_action`)が
    /// 実際に渡している変数名の組が食い違っていないことを確かめる。
    fn assert_no_leftover_placeholder(name: &str, keys: &[&str]) {
        let (template, _) = load_prompt(name).unwrap_or_else(|| panic!("{} not found", name));
        let vars: HashMap<&str, &str> = keys.iter().map(|k| (*k, "x")).collect();
        let expanded = expand(&template, &vars);
        assert!(
            !expanded.contains("{{"),
            "{} に未展開のプレースホルダが残っている: {}",
            name,
            expanded
                .split("{{")
                .skip(1)
                .filter_map(|s| s.split("}}").next())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    #[test]
    fn test_completion_prompts_have_no_unsupplied_placeholder() {
        // Backend::completion が渡す変数
        const KEYS: &[&str] = &["CHAPTER", "TEXT", "CHAT"];
        for name in [
            "prompt_completion.md",
            "prompt_completion_after_bracket.md",
            "prompt_completion_after_sentence.md",
            "prompt_completion_before_bracket.md",
            "prompt_completion_empty_bracket.md",
            "prompt_completion_in_bracket.md",
        ] {
            assert_no_leftover_placeholder(name, KEYS);
        }
    }

    #[test]
    fn test_code_action_prompts_have_no_unsupplied_placeholder() {
        // Backend::code_action が渡す変数
        const KEYS: &[&str] = &["CHAPTER", "TEXT", "TARGET", "CHAT"];
        for name in ["prompt_fill_mark.md", "prompt_rephrase.md"] {
            assert_no_leftover_placeholder(name, KEYS);
        }
    }

    #[test]
    fn test_chat_digest_prompt_has_no_unsupplied_placeholder() {
        // crate::acp が渡す変数
        assert_no_leftover_placeholder("prompt_chat_digest.md", &["HISTORY"]);
    }
}
