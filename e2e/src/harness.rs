//! 試験(`tests/*.rs`)の共通部分。表示サーバごとのコンテナを共有し、`libtest-mimic` で実行する。

use std::{
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use libtest_mimic::{Arguments, Failed, Trial};

use crate::{Display, Runtime, Sandbox, sandbox::new_run_dir};

/// 表示サーバごとのコンテナ。最初に使う試験が起動する(絞り込みで使わないものは起動しない)。
pub struct Fixture {
    pub runtime: Runtime,
    pub display: Display,
    /// この実行の結果の置き場。専用のコンテナを立てる試験は、この下に別のフォルダを作る
    pub run_dir: PathBuf,
    sandbox: OnceLock<Result<Sandbox, String>>,
    /// 共有のコンテナを起動した回数(起動に失敗して作り直した分を含む)
    start_attempts: AtomicU32,
}

impl Fixture {
    /// 共有のコンテナ。起動に失敗していれば、その理由を返す。
    ///
    /// 起動に失敗したら、`E2E_START_ATTEMPTS`(既定 2)回までコンテナごと作り直す。Wayland で Zed が起動直後に
    /// panic する問題(README 参照)は、同じコンテナの中で Zed を起動し直しても直らないが、新しいコンテナなら
    /// 通ることが多いため。作り直した回数は A07 が警告する(不安定さを隠さない)。
    pub fn get(&self) -> Result<&Sandbox, String> {
        self.sandbox
            .get_or_init(|| {
                let max = env_num("E2E_START_ATTEMPTS", 2).max(1);
                let mut last = String::new();
                for attempt in 1..=max {
                    self.start_attempts.store(attempt, Ordering::Relaxed);
                    // 作り直す回は、結果の置き場も分ける(前の回のログを上書きしない)
                    let dir = if attempt == 1 {
                        self.run_dir.clone()
                    } else {
                        self.run_dir.join(format!("retry{attempt}"))
                    };
                    match Sandbox::start(&self.runtime, self.display, &dir) {
                        Ok(sb) => return Ok(sb),
                        Err(e) => {
                            eprintln!(
                                "WARN [{}] コンテナの起動に失敗({attempt}/{max} 回目): {e}",
                                self.display.as_str()
                            );
                            last = e;
                        }
                    }
                }
                Err(last)
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    /// 共有のコンテナを起動した回数(1 なら 1 回で起動できた)。起動していなければ 0
    pub fn container_attempts(&self) -> u32 {
        self.start_attempts.load(Ordering::Relaxed)
    }

    /// A01 以外の試験が使う。起動できていなければ、A01 を見るよう促す
    pub fn running(&self) -> Result<&Sandbox, Failed> {
        self.get()
            .map_err(|_| Failed::from("コンテナが起動していない(A01 を参照)"))
    }
}

pub type Case = fn(&Fixture) -> Result<(), Failed>;

pub fn env_num(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

pub fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// 試験を実行する。名前は `<ID>::<表示サーバ>`。`enabled` が false を返す ID は登録しない。
/// コンテナは共有するので、試験は 1 つずつ実行する。
pub fn run(kind: &str, cases: &[(&str, Case)], enabled: impl Fn(&str) -> bool) -> ExitCode {
    let mut args = Arguments::from_args();
    args.test_threads.get_or_insert(1);

    let runtime = Runtime::detect();
    let run_dir = new_run_dir();
    eprintln!("e2e: runtime={} 結果={}", runtime.name(), run_dir.display());

    let displays = Display::from_env();
    // 空だと試験が 0 件のまま成功してしまう
    assert!(!displays.is_empty(), "E2E_MODES に表示サーバが 1 つも無い");
    let fixtures: Vec<Arc<Fixture>> = displays
        .into_iter()
        .map(|display| {
            Arc::new(Fixture {
                runtime: runtime.clone(),
                display,
                run_dir: run_dir.clone(),
                sandbox: OnceLock::new(),
                start_attempts: AtomicU32::new(0),
            })
        })
        .collect();

    let mut trials = Vec::new();
    for fx in &fixtures {
        for &(id, case) in cases {
            if !enabled(id) {
                continue;
            }
            let fx = Arc::clone(fx);
            trials.push(
                Trial::test(format!("{id}::{}", fx.display.as_str()), move || case(&fx))
                    .with_kind(kind),
            );
        }
    }

    let code = libtest_mimic::run(&args, trials).exit_code();
    drop(fixtures); // コンテナを止める(process::exit だと Drop が走らない)
    code
}
