//! テスト共通のヘルパ(一時ディレクトリは `tempfile` クレートを使う)。

use std::ffi::{OsStr, OsString};

/// 環境変数を差し替え、Drop(panic を含む)で元の値へ戻す。
///
/// 環境変数は process-wide なので、使うテストは呼び出し側で Mutex を取ること。
/// ロックのガードを先に宣言すれば、このガードが先に Drop される(復元してから解放)。
pub(crate) struct EnvGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let original = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }

    pub(crate) fn unset(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        unsafe { std::env::remove_var(key) };
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}
