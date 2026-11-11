//! ACP セッション設定(モデル・思考レベル)のカタログ。
//!
//! [`crate::acp`] が Zed へ出すセレクタの選択肢と、選択値から
//! `anthropic-agent-sdk` の [`anthropic_agent_sdk::ClaudeAgentOptions`] へ渡す値への
//! 変換をここに集める。`acp.rs` は既に長いので、選択肢の定義とマッピングだけを
//! 独立したモジュールに切り出した。
//!
//! # なぜ即座に切り替えられないか
//!
//! `anthropic-agent-sdk` はセッション途中のモデル/effort切替を非対応で、
//! `ClaudeSDKClient::set_model()` はローカルに保存するだけで `claude` CLI へは
//! 届かない。このプロジェクトは ACP の `SessionId` を `claude` CLI の
//! セッションID(UUID)としてそのまま使っているため、設定を変えるには
//! 同じIDで `--resume` してプロセスを起こし直すしかない
//! (`crate::acp` の `session/prompt` ハンドラがターンの頭でこれを行う)。
//!
//! # 選択肢の出どころ
//!
//! 選択肢は [`ModelCatalog`] にまとめ、表示([`to_config_options`])と選択の検証([`apply`])の
//! 両方が同じものを見る。表示と検証で一覧が食い違うと、表示した候補を選んだ瞬間に
//! 「未知のモデル」で弾かれてしまうため。
//! 通常は `claude` CLI に問い合わせた一覧([`crate::claude_models`])を使い、取得に
//! 失敗したら前回取れた一覧、それも無ければ静的な一覧([`ModelCatalog::fallback`])に戻る
//! ([`resolve_catalog`])。いつ取り直すかは `crate::acp` の `ModelsState` 参照。
//!
//! 思考レベル(effort)の段階はモデルごとに違いうる(CLI の `supportedEffortLevels`)ので、
//! いま選ばれているモデルの段階だけを出す。モデルを変えて今の effort がそのモデルで
//! 使えなくなったら、effort は既定(CLI 任せ)へ戻す。
//!
//! # effort の渡し方
//!
//! `claude` CLI へは環境変数 [`EFFORT_ENV_VAR`] で渡す(`crate::writing_agent`)。
//! `--effort` フラグは SDK の `extra_args` の許可リストに無く渡せない。以前使っていた
//! `--max-thinking-tokens` は CLI の effort に反映されない(`get_settings` の
//! `applied.effort` が変わらない)ことを claude 2.1.300 で確かめたため、使うのをやめた。

use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigSelect, SessionConfigSelectOption, SessionConfigValueId,
};
use anthropic_agent_sdk::ClaudeSDKClient;
use tracing::instrument;

use crate::claude_models::CliModel;

/// モデル選択の `SessionConfigOption.id`。
pub(crate) const CONFIG_ID_MODEL: &str = "model";
/// 思考レベル(effort)選択の `SessionConfigOption.id`。
pub(crate) const CONFIG_ID_EFFORT: &str = "effort";

/// effort を `claude` CLI に渡す環境変数。`--effort` / settings の `effortLevel` と同じ効果
/// (モジュールdocの「effort の渡し方」参照)。
pub(crate) const EFFORT_ENV_VAR: &str = "CLAUDE_CODE_EFFORT_LEVEL";

/// モデル/effort の既定値を表す value id。選ぶと `claude` CLI に指定を渡さず、
/// CLI 自身の既定に任せる。
const DEFAULT_VALUE_ID: &str = "default";

/// CLI から一覧を取れなかったときに出すモデルのエイリアス。`claude` CLI 側が常に
/// 最新のバージョンへ解決してくれるので、[`ClaudeSDKClient::supported_models`] の
/// 静的リスト(バージョン固定ID)が古くなっても最低限の選択はできる。
const MODEL_ALIASES: &[(&str, &str)] = &[
    (DEFAULT_VALUE_ID, "既定(CLI 任せ)"),
    ("opus", "Opus"),
    ("sonnet", "Sonnet"),
    ("haiku", "Haiku"),
    ("fable", "Fable"),
];

/// CLI からモデルごとの段階を取れなかったときの effort の段階。
/// `claude --help` の `--effort` の説明(claude 2.1.300)に合わせてある。
const FALLBACK_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// effort の段階の表示名。知らない段階は値をそのまま出す。
fn effort_label(level: &str) -> &str {
    match level {
        "low" => "低",
        "medium" => "中",
        "high" => "高",
        "xhigh" => "特高",
        "max" => "最大",
        other => other,
    }
}

/// セッションに現在適用されている(またはこれから適用する)モデル・思考レベル設定。
///
/// `None` は「`claude` CLI へ指定を渡さず、CLI 自身の既定に任せる」ことを意味する。
/// GUI 上は [`DEFAULT_VALUE_ID`] として表現される。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionConfig {
    pub(crate) model: Option<String>,
    /// effort の段階(`low` / `high` など)。[`EFFORT_ENV_VAR`] でそのまま CLI へ渡す。
    pub(crate) effort: Option<String>,
}

/// モデルセレクタの選択肢1件。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelChoice {
    /// `--model` に渡す値。[`DEFAULT_VALUE_ID`] だけは「フラグを渡さない」を意味する。
    id: String,
    name: String,
    description: Option<String>,
    /// このモデルで選べる effort の段階。空なら effort は既定(CLI 任せ)しか選べない。
    effort_levels: Vec<String>,
}

/// モデルセレクタに出す選択肢の一覧。先頭は常に [`DEFAULT_VALUE_ID`]。
/// モジュールdocの「選択肢の出どころ」参照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCatalog {
    choices: Vec<ModelChoice>,
}

impl ModelCatalog {
    /// `claude` CLI から取れた一覧をそのまま使う。
    ///
    /// CLI の一覧はエイリアス(`opus` 等)と1M版(`claude-fable-5-1[1m]` 等)を
    /// 選べる形で揃えて返してくるので、静的エイリアスは混ぜない(混ぜると同じモデルが
    /// 別名で2つ並ぶ)。`default` が含まれていなければ先頭に補う
    /// (セッションの既定値 `model: None` を表示できなくなるため)。重複は先勝ちで除く。
    /// `supportsEffort: false` のモデルには effort の段階を持たせない。
    #[instrument(ret)]
    pub(crate) fn from_cli(models: Vec<CliModel>) -> Self {
        let mut choices: Vec<ModelChoice> = Vec::with_capacity(models.len() + 1);
        if !models.iter().any(|m| m.value == DEFAULT_VALUE_ID) {
            choices.push(default_choice());
        }
        for model in models {
            if choices.iter().any(|c| c.id == model.value) {
                continue;
            }
            let effort_levels = if model.supports_effort == Some(false) {
                Vec::new()
            } else {
                model.supported_effort_levels
            };
            choices.push(ModelChoice {
                name: model.display_name.unwrap_or_else(|| model.value.clone()),
                id: model.value,
                description: model.description,
                effort_levels,
            });
        }
        // `default` を先頭に寄せる(CLI が別の位置に返してきた場合のため)。
        if let Some(pos) = choices.iter().position(|c| c.id == DEFAULT_VALUE_ID) {
            let default = choices.remove(pos);
            choices.insert(0, default);
        }
        Self { choices }
    }

    /// CLI から一覧が取れなかったときの静的な一覧。エイリアスを先頭に、
    /// `supported_models()` のバージョン固定IDを重複なく続ける。
    /// effort はどのモデルにも [`FALLBACK_EFFORT_LEVELS`] を出す。
    #[instrument(ret)]
    pub(crate) fn fallback() -> Self {
        let mut choices: Vec<ModelChoice> = MODEL_ALIASES
            .iter()
            .map(|(id, name)| ModelChoice {
                id: (*id).to_string(),
                name: (*name).to_string(),
                description: None,
                effort_levels: fallback_effort_levels(),
            })
            .collect();
        for model in ClaudeSDKClient::supported_models() {
            if choices.iter().any(|c| c.id == model.id) {
                continue;
            }
            choices.push(ModelChoice {
                name: model.name.unwrap_or_else(|| model.id.clone()),
                id: model.id,
                description: None,
                effort_levels: fallback_effort_levels(),
            });
        }
        Self { choices }
    }

    fn contains(&self, id: &str) -> bool {
        self.choices.iter().any(|c| c.id == id)
    }

    /// `model`(`None` は既定)で選べる effort の段階。一覧に無いモデルなら空。
    fn effort_levels_for(&self, model: Option<&str>) -> &[String] {
        let id = model.unwrap_or(DEFAULT_VALUE_ID);
        self.choices
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.effort_levels.as_slice())
            .unwrap_or_default()
    }

    fn select_options(&self) -> Vec<SessionConfigSelectOption> {
        self.choices
            .iter()
            .map(|c| {
                let option = SessionConfigSelectOption::new(
                    SessionConfigValueId::new(c.id.as_str()),
                    c.name.as_str(),
                );
                match &c.description {
                    Some(d) => option.description(d.clone()),
                    None => option,
                }
            })
            .collect()
    }
}

/// `claude` CLI への問い合わせ結果から、表示する一覧を決める。
///
/// 取れればそれを、取れなければ前回取れた一覧を、それも無ければ静的な一覧を使う。
/// 戻り値の `bool` は「いま取れた一覧か」(呼び出し側が「前回取れた一覧」として覚えるかの判断に使う)。
#[instrument(skip(fetched, last_good))]
pub(crate) fn resolve_catalog(
    fetched: Result<Vec<CliModel>, String>,
    last_good: Option<&std::sync::Arc<ModelCatalog>>,
) -> (std::sync::Arc<ModelCatalog>, bool) {
    match fetched {
        Ok(models) => (std::sync::Arc::new(ModelCatalog::from_cli(models)), true),
        Err(e) => {
            tracing::warn!("acp: claude CLI からモデル一覧を取れませんでした: {}", e);
            let kept = last_good
                .cloned()
                .unwrap_or_else(|| std::sync::Arc::new(ModelCatalog::fallback()));
            (kept, false)
        }
    }
}

/// 取り直した一覧 `fresh` を、ある会話に見せる一覧にする。
///
/// その会話で選ばれているモデル(`selected`)が新しい一覧から消えていたら、前に見せていた
/// 一覧(`shown`)にあったその選択肢を末尾に残す。CLI の更新で値の名前が変わることがあり
/// (例: 2.1.318 の `claude-fable-5-2` が、2.1.322 では `fable` になった)、選んだものが
/// 一覧から消えると、表示も思考レベルの選択も壊れるため。CLI はモデルIDをそのまま受け付ける。
#[instrument(skip(fresh, shown))]
pub(crate) fn catalog_for_session(
    fresh: &std::sync::Arc<ModelCatalog>,
    shown: &ModelCatalog,
    selected: &[Option<&str>],
) -> std::sync::Arc<ModelCatalog> {
    let mut choices: Option<Vec<ModelChoice>> = None;
    for id in selected.iter().flatten() {
        if fresh.contains(id)
            || choices
                .as_ref()
                .is_some_and(|c| c.iter().any(|x| x.id == *id))
        {
            continue;
        }
        if let Some(kept) = shown.choices.iter().find(|c| c.id == *id) {
            choices
                .get_or_insert_with(|| fresh.choices.clone())
                .push(kept.clone());
        }
    }
    match choices {
        Some(choices) => std::sync::Arc::new(ModelCatalog { choices }),
        None => fresh.clone(),
    }
}

/// `config` の effort が、そのモデルについて `models` で使えなければ既定へ戻す。戻したら `true`。
pub(crate) fn drop_unsupported_effort(config: &mut SessionConfig, models: &ModelCatalog) -> bool {
    let levels = models.effort_levels_for(config.model.as_deref());
    match &config.effort {
        Some(effort) if !levels.iter().any(|l| l == effort) => {
            config.effort = None;
            true
        }
        _ => false,
    }
}

fn fallback_effort_levels() -> Vec<String> {
    FALLBACK_EFFORT_LEVELS
        .iter()
        .map(|l| (*l).to_string())
        .collect()
}

/// CLI が `default` を返さなかったときに補う既定の選択肢。
fn default_choice() -> ModelChoice {
    let (id, name) = MODEL_ALIASES[0];
    ModelChoice {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        effort_levels: fallback_effort_levels(),
    }
}

/// 現在の設定を Zed へ見せる `SessionConfigOption` の一覧に変換する。
///
/// `session/new` / `session/load` / `session/set_config_option` の3箇所で同じ関数を使う。
/// `SetSessionConfigOptionResponse` は「全オプションの現在値」を返す規約なので、
/// ここを共通化しないと片方だけ古い値を返しかねない。モデルを変えると effort の
/// 選択肢も変わりうるが、この応答で effort 側も一緒に出し直される。
#[instrument]
pub(crate) fn to_config_options(
    config: &SessionConfig,
    models: &ModelCatalog,
) -> Vec<SessionConfigOption> {
    let model_current = config.model.as_deref().unwrap_or(DEFAULT_VALUE_ID);
    let model_options: Vec<SessionConfigSelectOption> = models.select_options();

    let levels = models.effort_levels_for(config.model.as_deref());
    let effort_current = config
        .effort
        .as_deref()
        .filter(|e| levels.iter().any(|l| l == e))
        .unwrap_or(DEFAULT_VALUE_ID);
    let effort_options: Vec<SessionConfigSelectOption> =
        std::iter::once(SessionConfigSelectOption::new(
            SessionConfigValueId::new(DEFAULT_VALUE_ID),
            "既定(CLI 任せ)",
        ))
        .chain(levels.iter().map(|l| {
            SessionConfigSelectOption::new(SessionConfigValueId::new(l.as_str()), effort_label(l))
        }))
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

/// `session/set_config_option` で受け取った選択を `config` に適用する。
///
/// 未知の `config_id` / 未知の value id はエラー文字列を返す(呼び出し側でログに落とす)。
/// モデルは `models`(Zed に見せたのと同じ一覧)に載っているものだけ、effort は
/// いま選ばれているモデルで使える段階だけを受け付ける。
pub(crate) fn apply(
    config: &mut SessionConfig,
    config_id: &str,
    value: &SessionConfigOptionValue,
    models: &ModelCatalog,
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
            if !models.contains(value_id) {
                return Err(format!("未知のモデルです: {}", value_id));
            }
            config.model = if value_id == DEFAULT_VALUE_ID {
                None
            } else {
                Some(value_id.to_string())
            };
            // 新しいモデルで使えない effort が残っていたら既定へ戻す。
            drop_unsupported_effort(config, models);
            Ok(())
        }
        CONFIG_ID_EFFORT => {
            if value_id == DEFAULT_VALUE_ID {
                config.effort = None;
                return Ok(());
            }
            let levels = models.effort_levels_for(config.model.as_deref());
            if !levels.iter().any(|l| l == value_id) {
                return Err(format!(
                    "このモデルでは使えない思考レベルです: {}",
                    value_id
                ));
            }
            config.effort = Some(value_id.to_string());
            Ok(())
        }
        other => Err(format!("未知の config_id です: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::SessionConfigSelectOptions;

    fn select(option: &SessionConfigOption) -> &SessionConfigSelect {
        let SessionConfigKind::Select(select) = &option.kind else {
            panic!("expected select");
        };
        select
    }

    fn value_ids(select: &SessionConfigSelect) -> Vec<String> {
        let SessionConfigSelectOptions::Ungrouped(options) = &select.options else {
            panic!("expected ungrouped options");
        };
        options.iter().map(|o| o.value.0.to_string()).collect()
    }

    fn set(config: &mut SessionConfig, id: &str, value: &str, models: &ModelCatalog) {
        apply(
            config,
            id,
            &SessionConfigOptionValue::value_id(value.to_string()),
            models,
        )
        .unwrap();
    }

    #[test]
    fn test_to_config_options_reflects_current_values() {
        let config = SessionConfig {
            model: Some("opus".to_string()),
            effort: Some("medium".to_string()),
        };
        let options = to_config_options(&config, &ModelCatalog::fallback());
        assert_eq!(options.len(), 2);
        assert_eq!(
            select(&options[0]).current_value,
            SessionConfigValueId::new("opus")
        );
        assert_eq!(
            select(&options[1]).current_value,
            SessionConfigValueId::new("medium")
        );
    }

    #[test]
    fn test_to_config_options_default_is_default_id() {
        let options = to_config_options(&SessionConfig::default(), &ModelCatalog::fallback());
        assert_eq!(
            select(&options[0]).current_value,
            SessionConfigValueId::new("default")
        );
        assert_eq!(
            select(&options[1]).current_value,
            SessionConfigValueId::new("default")
        );
    }

    #[test]
    fn test_apply_model_alias() {
        let mut config = SessionConfig::default();
        set(
            &mut config,
            CONFIG_ID_MODEL,
            "sonnet",
            &ModelCatalog::fallback(),
        );
        assert_eq!(config.model, Some("sonnet".to_string()));
    }

    #[test]
    fn test_apply_model_default_clears() {
        let mut config = SessionConfig {
            model: Some("opus".to_string()),
            effort: None,
        };
        set(
            &mut config,
            CONFIG_ID_MODEL,
            "default",
            &ModelCatalog::fallback(),
        );
        assert_eq!(config.model, None);
    }

    #[test]
    fn test_apply_unknown_model_is_rejected() {
        let mut config = SessionConfig::default();
        let err = apply(
            &mut config,
            CONFIG_ID_MODEL,
            &SessionConfigOptionValue::value_id("gpt-5"),
            &ModelCatalog::fallback(),
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
            &ModelCatalog::fallback(),
        )
        .unwrap_err();
        assert!(err.contains("not-a-real-option"));
    }

    /// フォールバックでは `claude --help` の5段階がそのまま選べ、`default` で既定へ戻ること。
    #[test]
    fn test_fallback_effort_levels_are_selectable() {
        let models = ModelCatalog::fallback();
        let mut config = SessionConfig::default();
        for level in FALLBACK_EFFORT_LEVELS {
            set(&mut config, CONFIG_ID_EFFORT, level, &models);
            assert_eq!(config.effort.as_deref(), Some(*level));
        }
        set(&mut config, CONFIG_ID_EFFORT, "default", &models);
        assert_eq!(config.effort, None);
    }

    #[test]
    fn test_apply_unknown_effort_is_rejected() {
        let mut config = SessionConfig::default();
        let err = apply(
            &mut config,
            CONFIG_ID_EFFORT,
            &SessionConfigOptionValue::value_id("ultra"),
            &ModelCatalog::fallback(),
        )
        .unwrap_err();
        assert!(err.contains("ultra"));
    }

    fn cli_model(value: &str, name: Option<&str>) -> CliModel {
        CliModel {
            value: value.to_string(),
            display_name: name.map(str::to_string),
            description: Some(format!("{value} の説明")),
            supports_effort: Some(true),
            supported_effort_levels: fallback_effort_levels(),
        }
    }

    fn cli_model_with_effort(value: &str, supports: Option<bool>, levels: &[&str]) -> CliModel {
        CliModel {
            supports_effort: supports,
            supported_effort_levels: levels.iter().map(|l| (*l).to_string()).collect(),
            ..cli_model(value, None)
        }
    }

    fn ids(catalog: &ModelCatalog) -> Vec<String> {
        catalog.choices.iter().map(|c| c.id.clone()).collect()
    }

    /// CLI の一覧はそのまま(順序・表示名・説明とも)使い、静的エイリアスを混ぜないこと。
    #[test]
    fn test_from_cli_uses_cli_list_as_is() {
        let catalog = ModelCatalog::from_cli(vec![
            cli_model("default", Some("Default (recommended)")),
            cli_model("opus", Some("Opus")),
            cli_model("claude-fable-5-1[1m]", Some("Fable")),
            cli_model("haiku", None),
        ]);
        assert_eq!(
            ids(&catalog),
            ["default", "opus", "claude-fable-5-1[1m]", "haiku"]
        );
        let options = catalog.select_options();
        assert_eq!(options[0].name, "Default (recommended)");
        assert_eq!(options[3].name, "haiku", "表示名が無ければ value を出す");
        assert_eq!(
            options[2].description.as_deref(),
            Some("claude-fable-5-1[1m] の説明")
        );
    }

    /// `default` が無ければ先頭に補い、別の位置にあれば先頭へ寄せ、重複は除くこと。
    #[test]
    fn test_from_cli_normalizes_default_and_duplicates() {
        let missing = ModelCatalog::from_cli(vec![cli_model("opus", None)]);
        assert_eq!(ids(&missing), ["default", "opus"]);

        let misplaced = ModelCatalog::from_cli(vec![
            cli_model("opus", None),
            cli_model("default", Some("Default")),
            cli_model("opus", Some("dup")),
        ]);
        assert_eq!(ids(&misplaced), ["default", "opus"]);
        assert_eq!(
            misplaced.choices[0].name, "Default",
            "CLI の default を残す"
        );
    }

    /// CLI 由来の一覧に載った値(`[1m]` 付きなど)を選べ、載っていない静的エイリアスは弾くこと。
    #[test]
    fn test_apply_validates_against_given_catalog() {
        let catalog = ModelCatalog::from_cli(vec![
            cli_model("default", None),
            cli_model("claude-fable-5-1[1m]", None),
        ]);
        let mut config = SessionConfig::default();
        set(
            &mut config,
            CONFIG_ID_MODEL,
            "claude-fable-5-1[1m]",
            &catalog,
        );
        assert_eq!(config.model.as_deref(), Some("claude-fable-5-1[1m]"));

        let err = apply(
            &mut config,
            CONFIG_ID_MODEL,
            &SessionConfigOptionValue::value_id("fable"),
            &catalog,
        )
        .unwrap_err();
        assert!(err.contains("fable"));
    }

    /// effort の選択肢は、いま選ばれているモデルの段階(+既定)だけになること。
    /// `supportsEffort: false` のモデルは既定しか出ないこと。
    #[test]
    fn test_effort_options_follow_selected_model() {
        let catalog = ModelCatalog::from_cli(vec![
            cli_model_with_effort(
                "default",
                Some(true),
                &["low", "medium", "high", "xhigh", "max"],
            ),
            cli_model_with_effort("sonnet", None, &["low", "high"]),
            cli_model_with_effort("haiku", Some(false), &["low"]),
        ]);
        let effort_ids = |model: Option<&str>| {
            let config = SessionConfig {
                model: model.map(str::to_string),
                effort: None,
            };
            value_ids(select(&to_config_options(&config, &catalog)[1]))
        };
        assert_eq!(
            effort_ids(None),
            ["default", "low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(effort_ids(Some("sonnet")), ["default", "low", "high"]);
        assert_eq!(effort_ids(Some("haiku")), ["default"]);

        let options = to_config_options(&SessionConfig::default(), &catalog);
        let SessionConfigSelectOptions::Ungrouped(effort) = &select(&options[1]).options else {
            panic!("expected ungrouped options");
        };
        assert_eq!(effort[4].name, "特高");
    }

    /// モデルを変えて今の effort が使えなくなったら既定へ戻し、使えるなら残すこと。
    /// 選んだモデルで使えない effort は受け付けないこと。
    #[test]
    fn test_model_change_drops_unsupported_effort() {
        let catalog = ModelCatalog::from_cli(vec![
            cli_model_with_effort(
                "default",
                Some(true),
                &["low", "medium", "high", "xhigh", "max"],
            ),
            cli_model_with_effort("sonnet", Some(true), &["low", "high"]),
            cli_model_with_effort("haiku", Some(false), &[]),
        ]);
        let mut config = SessionConfig::default();

        set(&mut config, CONFIG_ID_EFFORT, "high", &catalog);
        set(&mut config, CONFIG_ID_MODEL, "sonnet", &catalog);
        assert_eq!(
            config.effort.as_deref(),
            Some("high"),
            "sonnet でも使えるので残す"
        );

        let err = apply(
            &mut config,
            CONFIG_ID_EFFORT,
            &SessionConfigOptionValue::value_id("xhigh"),
            &catalog,
        )
        .unwrap_err();
        assert!(err.contains("xhigh"));

        set(&mut config, CONFIG_ID_MODEL, "haiku", &catalog);
        assert_eq!(
            config.effort, None,
            "haiku は effort 非対応なので既定へ戻す"
        );
    }

    /// 取れた一覧 → 前回取れた一覧 → 静的な一覧、の順に使うこと。
    #[test]
    fn test_resolve_catalog_prefers_fresh_then_last_good_then_fallback() {
        let last = std::sync::Arc::new(ModelCatalog::from_cli(vec![cli_model("opus", None)]));

        let (fresh, is_fresh) = resolve_catalog(
            Ok(vec![cli_model("default", None), cli_model("fable", None)]),
            Some(&last),
        );
        assert!(is_fresh);
        assert_eq!(ids(&fresh), ["default", "fable"]);

        let (kept, is_fresh) = resolve_catalog(Err("boom".to_string()), Some(&last));
        assert!(!is_fresh);
        assert!(
            std::sync::Arc::ptr_eq(&kept, &last),
            "前回の一覧をそのまま使う"
        );

        let (fallback, is_fresh) = resolve_catalog(Err("boom".to_string()), None);
        assert!(!is_fresh);
        assert_eq!(*fallback, ModelCatalog::fallback());
    }

    /// 選んでいたモデルが新しい一覧から消えたら、前の選択肢(表示名・effort ごと)を末尾に残すこと。
    /// 消えていなければ、新しい一覧をそのまま使うこと。
    #[test]
    fn test_catalog_for_session_keeps_vanished_selection() {
        // 2.1.318 → 2.1.322 で実際に起きた入れ替わり
        let shown = ModelCatalog::from_cli(vec![
            cli_model("default", None),
            cli_model_with_effort("claude-fable-5-2", Some(true), &["low", "high"]),
            cli_model("fable", Some("Fable 5.1")),
        ]);
        let fresh = std::sync::Arc::new(ModelCatalog::from_cli(vec![
            cli_model("default", None),
            cli_model("fable", Some("Fable 5.2")),
            cli_model("claude-fable-5-1", Some("Fable 5.1")),
        ]));

        let kept = catalog_for_session(&fresh, &shown, &[Some("claude-fable-5-2")]);
        assert_eq!(
            ids(&kept),
            ["default", "fable", "claude-fable-5-1", "claude-fable-5-2"]
        );
        assert_eq!(
            kept.effort_levels_for(Some("claude-fable-5-2")),
            ["low", "high"],
            "残した選択肢の effort もそのまま選べる"
        );

        for selected in [None, Some("fable")] {
            let same = catalog_for_session(&fresh, &shown, &[selected]);
            assert!(std::sync::Arc::ptr_eq(&same, &fresh), "{selected:?}");
        }
    }

    /// effort がそのモデルで使えなければ既定へ戻し、使えれば残すこと。
    #[test]
    fn test_drop_unsupported_effort() {
        let models = ModelCatalog::from_cli(vec![
            cli_model_with_effort("default", Some(true), &["low", "high"]),
            cli_model_with_effort("haiku", Some(false), &[]),
        ]);
        let mut kept = SessionConfig {
            model: None,
            effort: Some("high".to_string()),
        };
        assert!(!drop_unsupported_effort(&mut kept, &models));
        assert_eq!(kept.effort.as_deref(), Some("high"));

        let mut dropped = SessionConfig {
            model: Some("haiku".to_string()),
            effort: Some("high".to_string()),
        };
        assert!(drop_unsupported_effort(&mut dropped, &models));
        assert_eq!(dropped.effort, None);
    }

    /// フォールバックの一覧にエイリアス5件が含まれ、`supported_models()` 由来のIDと重複しないこと。
    #[test]
    fn test_fallback_contains_aliases_without_duplicating_known_ids() {
        let options = ModelCatalog::fallback().select_options();
        for (id, _) in MODEL_ALIASES {
            assert!(
                options
                    .iter()
                    .any(|o| o.value == SessionConfigValueId::new(*id)),
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
