use std::path::{Path, PathBuf};
use zed_extension_api as zed;
use zed_extension_api::{Command, Extension, LanguageServerId, Worktree, serde_json};

/// 探索するLSPバイナリ名(拡張子なし)
const LSP_BINARY_NAME: &str = "fifty_four_lsp";

struct FiftyFour {}

impl Extension for FiftyFour {
    fn new() -> Self {
        FiftyFour {}
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command, String> {
        // 1. settings.json の lsp.<id>.binary.path を最優先で使う。
        //    (Zed 本体も同設定を拡張より優先するため通常ここへは来ないが、拡張単体でも自己完結させる)
        if let Ok(lsp_settings) =
            zed::settings::LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            && let Some(binary) = lsp_settings.binary
            && let Some(path) = binary.path
        {
            return Ok(Command {
                command: path,
                args: binary.arguments.unwrap_or_default(),
                env: binary
                    .env
                    .map(|e| e.into_iter().collect())
                    .unwrap_or_default(),
            });
        }

        let (os, _arch) = zed::current_platform();
        let binary_file_name = match os {
            zed::Os::Windows => format!("{LSP_BINARY_NAME}.exe"),
            zed::Os::Mac | zed::Os::Linux => LSP_BINARY_NAME.to_string(),
        };

        // 2. PATH 上のバイナリ
        if let Some(path) = worktree.which(&binary_file_name) {
            return Ok(Command {
                command: path,
                args: vec![],
                env: vec![],
            });
        }

        // 3. 作業ディレクトリ(extensions/work/<id>)配下の再帰探索
        let start_dir = std::env::current_dir()
            .map_err(|e| format!("拡張の作業ディレクトリを取得できません: {}", e))?;

        find_binary_recursive(&start_dir, &binary_file_name)
            .ok_or_else(|| {
                format!(
                    "`{}` が見つかりません(settings.json の `lsp.{}.binary.path` 未設定・PATH になし・{:?} 以下にもなし)。\
                     settings.json に配置先バイナリの絶対パスを設定してください。",
                    binary_file_name,
                    language_server_id.as_ref(),
                    start_dir
                )
            })
            .map(|path| Command {
                command: path.to_string_lossy().to_string(),
                args: vec![],
                env: vec![],
            })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>, String> {
        let options =
            zed::settings::LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                .ok()
                .and_then(|lsp_settings| lsp_settings.initialization_options.clone())
                .unwrap_or_else(|| zed::serde_json::json!({}));

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

/// `dir` 配下を子方向へ再帰的に探索し、`file_name` と一致する最初のファイルの絶対パスを返す。
/// 深さ優先。シンボリックリンクは辿らない(ループ防止)。
/// デバッグ用に、調べたファイル/ディレクトリのパスを stderr へ逐次出力する
/// (`zed_extension_api` に専用のロギングAPIが無いため、WASIの標準エラー出力を使う。
/// Zed側の拡張ログ表示で確認する想定)。
fn find_binary_recursive(dir: &Path, file_name: &str) -> Option<PathBuf> {
    eprintln!("fifty-four: scanning {:?}", dir);

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fifty-four: failed to read_dir {:?}: {}", dir, e);
            return None;
        }
    };
    let mut subdirs = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };

        eprintln!("fifty-four: checking {:?}", path);

        if file_type.is_file() && entry.file_name().to_str() == Some(file_name) {
            eprintln!("fifty-four: found binary at {:?}", path);
            return Some(path);
        }
        if file_type.is_dir() {
            subdirs.push(path);
        }
        // symlink はどちらでもないので無視(ループ防止)
    }

    for subdir in subdirs {
        if let Some(found) = find_binary_recursive(&subdir, file_name) {
            return Some(found);
        }
    }
    None
}

zed::register_extension!(FiftyFour);
