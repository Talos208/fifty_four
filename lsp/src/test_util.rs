//! テスト共通のヘルパ。`tempfile` クレートは使わない方針なので、同等の最小限をここに持つ。

use std::ffi::{OsStr, OsString};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// テストごとに一意な一時ディレクトリ。Drop(assert 失敗による panic を含む)で削除する。
///
/// 名前に PID と連番を含めるので、並列テストや複数の `cargo test` が同時に走っても
/// 衝突しない(固定名だと、同名を使うテストが増えたときに互いのファイルを消し合う)。
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub(crate) fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ff_{}_{}_{}",
            tag,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
