use zed_extension_api as zed;
use zed_extension_api::{serde_json, Command, Extension, LanguageServerId, Worktree};

struct FiftyFour {}

impl Extension for FiftyFour {
    fn new() -> Self {
        FiftyFour {}
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        _worktree: &Worktree,
    ) -> Result<Command, String> {
        Ok(Command {
            command: get_path_to_language_server_executable()?,
            args: get_args_for_language_server()?,
            env: get_env_for_language_server()?,
        })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>, String> {
        let mut options =
            zed::settings::LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                .ok()
                .and_then(|lsp_settings| lsp_settings.initialization_options.clone())
                .unwrap_or_else(|| zed::serde_json::json!({}));

        let options_obj = options
            .as_object_mut()
            .expect("initialization_options must be an object");

        let enabled_features = options_obj
            .entry("enabledFeatures")
            .or_insert_with(|| zed::serde_json::json!({}));

        if let Some(features_obj) = enabled_features.as_object_mut() {
            // TODO
            features_obj
                .entry("onTypeFormatting")
                .or_insert(zed::serde_json::Value::Bool(false));
        }

        Ok(Some(options))
    }
    /*
    fn language_server_workspace_configuration(
        &mut self,
        _language_server_id: &LanguageServerId,
        _worktree: &Worktree,
    ) -> zed_extension_api::Result<Option<serde_json::Value>> {
        // language_server_workspace_configuration fifty-four.Worktree { handle: Resource { handle: 1 } }

        return Err(format!(
            "language_server_workspace_configuration {}.{:?}",
            _language_server_id, _worktree
        ));
    }
    */
}

fn get_path_to_language_server_executable() -> Result<String, String> {
    // Implementation to get the path to the language server executable
    Ok(
        "C:\\Users\\talos\\RustroverProjects\\fifty_four\\target\\debug\\fifty_four_lsp.exe"
            .to_string(),
    )
}

fn get_args_for_language_server() -> Result<Vec<String>, String> {
    // Implementation to get the arguments for the language server
    Ok(vec![])
}

fn get_env_for_language_server() -> Result<Vec<(String, String)>, String> {
    // Implementation to get the environment variables for the language server
    Ok(vec![])
}

zed::register_extension!(FiftyFour);
