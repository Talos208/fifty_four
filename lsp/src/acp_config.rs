//! ACP セッション設定(モデル・思考レベル)のカタログ。
//!
//! [`crate::acp`] が Zed へ出すセレクタの選択肢と、選択値から
//! `anthropic-agent-sdk` の [`anthropic_agent_sdk::ClaudeAgentOptions`] へ渡す値への
//! 変換をここに集める。`acp.rs` は既に長いので、選択肢の定義とマッピングだけを
//! 独立したモジュールに切り出した。
//!
//! # なぜ即座に切り替えられないか
//!
//! `anthropic-agent-sdk` はセッション途中のモデル/thinking切替を非対応で、
//! `ClaudeSDKClient::set_model()` はローカルに保存するだけで `claude` CLI へは
//! 届かない。このプロジェクトは ACP の `SessionId` を `claude` CLI の
//! セッションID(UUID)としてそのまま使っているため、設定を変えるには
//! 同じIDで `--resume` してプロセスを起こし直すしかない
//! (`crate::acp` の `session/prompt` ハンドラがターンの頭でこれを行う)。

use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigSelect, SessionConfigSelectOption,
    SessionConfigValueId,
};
use anthropic_agent_sdk::ClaudeSDKClient;

/// モデル選択の `SessionConfigOption.id`。
pub(crate) const CONFIG_ID_MODEL: &str = "model";
/// 思考レベル(effort)選択の `SessionConfigOption.id`。
pub(crate) const CONFIG_ID_EFFORT: &str = "effort";

/// モデル/thinking の既定値を表す value id。選ぶと `claude` CLI にフラグを渡さず、
/// CLI 自身の既定に任せる。
const DEFAULT_VALUE_ID: &str = "default";

/// モデルのエイリアス。`claude` CLI 側が常に最新のバージョンへ解決してくれるので、
/// [`ClaudeSDKClient::supported_models`] の静的リスト(バージョン固定ID)が
/// 古くなっても実用上困らない。
const MODEL_ALIASES: &[(&str, &str)] = &[
    (DEFAULT_VALUE_ID, "既定(CLI 任せ)"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
    ("fable", "Fable"),
];

/// 思考レベル(effort)の選択肢。`max_thinking_tokens` へ写す。
/// 値は Claude Code が使っている段階に合わせてある。
const EFFORT_LEVELS: &[(&str, &str, Option<u32>)] = &[
    (DEFAULT_VALUE_ID, "既定(CLI 任せ)", None),
    ("off", "考えない", Some(0)),
    ("low", "低", Some(4_000)),
    ("medium", "中", Some(10_000)),
    ("high", "高", Some(31_999)),
    ("max", "最大", Some(63_999)),
];

/// セッションに現在適用されている(またはこれから適用する)モデル・思考レベル設定。
///
/// `None` は「`claude` CLI へフラグを渡さず、CLI 自身の既定に任せる」ことを意味する。
/// GUI 上は [`DEFAULT_VALUE_ID`] として表現される。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionConfig {
    pub(crate) model: Option<String>,
    pub(crate) thinking_tokens: Option<u32>,
}

/// 現在の設定を Zed へ見せる `SessionConfigOption` の一覧に変換する。
///
/// `session/new` / `session/load` / `session/set_config_option` の3箇所で同じ関数を使う。
/// `SetSessionConfigOptionResponse` は「全オプションの現在値」を返す規約なので、
/// ここを共通化しないと片方だけ古い値を返しかねない。
pub(crate) fn to_config_options(config: &SessionConfig) -> Vec<SessionConfigOption> {
    let model_current = config.model.as_deref().unwrap_or(DEFAULT_VALUE_ID);
    let model_options: Vec<SessionConfigSelectOption> = model_select_options();

    let effort_current = effort_value_id_for(config.thinking_tokens);
    let effort_options: Vec<SessionConfigSelectOption> = EFFORT_LEVELS
        .iter()
        .map(|(id, name, _)| SessionConfigSelectOption::new(SessionConfigValueId::new(*id), *name))
        .collect();

    vec![
        SessionConfigOption::new(
            SessionConfigId::new(CONFIG_ID_MODEL),
            "モデル",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new(model_current),
                model_options,
            )),
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::new(
            SessionConfigId::new(CONFIG_ID_EFFORT),
            "思考レベル",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::new(effort_current),
                effort_options,
            )),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]
}

/// モデルセレクタの選択肢。エイリアスを先頭に、`supported_models()` の
/// バージョン固定IDを重複なく続ける。
fn model_select_options() -> Vec<SessionConfigSelectOption> {
    let mut seen: std::collections::HashSet<String> =
        MODEL_ALIASES.iter().map(|(id, _)| (*id).to_string()).collect();
    let mut options: Vec<SessionConfigSelectOption> = MODEL_ALIASES
        .iter()
        .map(|(id, name)| SessionConfigSelectOption::new(SessionConfigValueId::new(*id), *name))
        .collect();

    for model in ClaudeSDKClient::supported_models() {
        if !seen.insert(model.id.clone()) {
            continue;
        }
        let name = model.name.unwrap_or_else(|| model.id.clone());
        options.push(SessionConfigSelectOption::new(
            SessionConfigValueId::new(model.id),
            name,
        ));
    }

    options
}

/// `thinking_tokens` から対応する `EFFORT_LEVELS` の value id を逆引きする。
/// 一覧に無い値(将来 GUI 外から設定された等)は "default" 扱いにはせず、
/// 一番近いラベルを出すよりも「未知」を明示したいので `DEFAULT_VALUE_ID` にはしない。
/// 現状は `apply()` 経由でしか `thinking_tokens` は変わらないため、常に一致するはず。
fn effort_value_id_for(thinking_tokens: Option<u32>) -> &'static str {
    EFFORT_LEVELS
        .iter()
        .find(|(_, _, tokens)| *tokens == thinking_tokens)
        .map(|(id, _, _)| *id)
        .unwrap_or(DEFAULT_VALUE_ID)
}

/// `session/set_config_option` で受け取った選択を `config` に適用する。
///
/// 未知の `config_id` / 未知の value id はエラー文字列を返す(呼び出し側でログに落とす)。
pub(crate) fn apply(
    config: &mut SessionConfig,
    config_id: &str,
    value: &SessionConfigOptionValue,
) -> Result<(), String> {
    let SessionConfigOptionValue::ValueId { value } = value else {
        return Err(format!(
            "config_id={} はブール値ではなく選択式です",
            config_id
        ));
    };
    let value_id = value.0.as_ref();

    match config_id {
        CONFIG_ID_MODEL => {
            let known = MODEL_ALIASES.iter().any(|(id, _)| *id == value_id)
                || ClaudeSDKClient::supported_models()
                    .iter()
                    .any(|m| m.id == value_id);
            if !known {
                return Err(format!("未知のモデルです: {}", value_id));
            }
            config.model = if value_id == DEFAULT_VALUE_ID {
                None
            } else {
                Some(value_id.to_string())
            };
            Ok(())
        }
        CONFIG_ID_EFFORT => {
            let Some((_, _, tokens)) = EFFORT_LEVELS.iter().find(|(id, _, _)| *id == value_id)
            else {
                return Err(format!("未知の思考レベルです: {}", value_id));
            };
            config.thinking_tokens = *tokens;
            Ok(())
        }
        other => Err(format!("未知の config_id です: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_config_options_reflects_current_values() {
        let config = SessionConfig {
            model: Some("opus".to_string()),
            thinking_tokens: Some(10_000),
        };
        let options = to_config_options(&config);
        assert_eq!(options.len(), 2);

        let SessionConfigKind::Select(select) = &options[0].kind else {
            panic!("expected select");
        };
        assert_eq!(select.current_value, SessionConfigValueId::new("opus"));

        let SessionConfigKind::Select(select) = &options[1].kind else {
            panic!("expected select");
        };
        assert_eq!(select.current_value, SessionConfigValueId::new("medium"));
    }

    #[test]
    fn test_to_config_options_default_is_default_id() {
        let options = to_config_options(&SessionConfig::default());
        let SessionConfigKind::Select(model) = &options[0].kind else {
            panic!("expected select");
        };
        assert_eq!(model.current_value, SessionConfigValueId::new("default"));
        let SessionConfigKind::Select(effort) = &options[1].kind else {
            panic!("expected select");
        };
        assert_eq!(effort.current_value, SessionConfigValueId::new("default"));
    }

    #[test]
    fn test_apply_model_alias() {
        let mut config = SessionConfig::default();
        apply(
            &mut config,
            CONFIG_ID_MODEL,
            &SessionConfigOptionValue::value_id("sonnet"),
        )
        .unwrap();
        assert_eq!(config.model, Some("sonnet".to_string()));
    }

    #[test]
    fn test_apply_model_default_clears() {
        let mut config = SessionConfig {
            model: Some("opus".to_string()),
            thinking_tokens: None,
        };
        apply(
            &mut config,
            CONFIG_ID_MODEL,
            &SessionConfigOptionValue::value_id("default"),
        )
        .unwrap();
        assert_eq!(config.model, None);
    }

    #[test]
    fn test_apply_unknown_model_is_rejected() {
        let mut config = SessionConfig::default();
        let err = apply(
            &mut config,
            CONFIG_ID_MODEL,
            &SessionConfigOptionValue::value_id("gpt-5"),
        )
        .unwrap_err();
        assert!(err.contains("gpt-5"));
    }

    #[test]
    fn test_apply_unknown_config_id_is_rejected() {
        let mut config = SessionConfig::default();
        let err = apply(
            &mut config,
            "not-a-real-option",
            &SessionConfigOptionValue::value_id("default"),
        )
        .unwrap_err();
        assert!(err.contains("not-a-real-option"));
    }

    #[test]
    fn test_effort_levels_map_to_expected_thinking_tokens() {
        let mut config = SessionConfig::default();
        let expectations: &[(&str, Option<u32>)] = &[
            ("default", None),
            ("off", Some(0)),
            ("low", Some(4_000)),
            ("medium", Some(10_000)),
            ("high", Some(31_999)),
            ("max", Some(63_999)),
        ];
        for (id, expected) in expectations {
            apply(
                &mut config,
                CONFIG_ID_EFFORT,
                &SessionConfigOptionValue::value_id(*id),
            )
            .unwrap();
            assert_eq!(config.thinking_tokens, *expected, "value id={}", id);
        }
    }

    #[test]
    fn test_apply_unknown_effort_is_rejected() {
        let mut config = SessionConfig::default();
        let err = apply(
            &mut config,
            CONFIG_ID_EFFORT,
            &SessionConfigOptionValue::value_id("ultra"),
        )
        .unwrap_err();
        assert!(err.contains("ultra"));
    }

    /// モデル候補にエイリアス5件が含まれ、`supported_models()` 由来のIDと重複しないこと。
    #[test]
    fn test_model_options_contain_aliases_without_duplicating_known_ids() {
        let options = model_select_options();
        for (id, _) in MODEL_ALIASES {
            assert!(
                options.iter().any(|o| o.value == SessionConfigValueId::new(*id)),
                "missing alias {}",
                id
            );
        }
        let mut ids: Vec<&str> = options.iter().map(|o| o.value.0.as_ref()).collect();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate ids in model options");
    }
}
