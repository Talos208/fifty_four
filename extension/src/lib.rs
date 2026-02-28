use zed_extension_api as zed;
use zed_extension_api::{Command, Extension, LanguageServerId, Worktree};

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
