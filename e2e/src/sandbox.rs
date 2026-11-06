use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::wait;

/// 表示サーバ。`E2E_DISPLAY` としてコンテナに渡す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    X11,
    Wayland,
}

impl Display {
    pub fn as_str(self) -> &'static str {
        match self {
            Display::X11 => "x11",
            Display::Wayland => "wayland",
        }
    }

    /// `E2E_MODES`(カンマ区切り。例: `x11,wayland`)で選ぶ。未設定なら Wayland だけ。
    /// X11 は既定では対象にしない(明示したときだけ実行する)。
    pub fn from_env() -> Vec<Display> {
        let spec = std::env::var("E2E_MODES").unwrap_or_else(|_| "wayland".into());
        let modes: Vec<Display> = spec
            .split(',')
            .filter_map(|s| match s.trim() {
                "x11" => Some(Display::X11),
                "wayland" => Some(Display::Wayland),
                "" => None,
                other => panic!("E2E_MODES に未知の表示サーバ: {other}(x11 | wayland)"),
            })
            .collect();
        // 空だと試験が 0 件のまま成功してしまうので止める
        assert!(!modes.is_empty(), "E2E_MODES が空(x11 | wayland)");
        modes
    }
}

/// コンテナの実行系(`wslc` か `docker`)。CLI を呼ぶだけなので、どちらも同じ引数で動く。
#[derive(Clone, Debug)]
pub struct Runtime(String);

impl Runtime {
    /// `E2E_RUNTIME` があればそれ、なければ `wslc` が動けば `wslc`、でなければ `docker`。
    pub fn detect() -> Self {
        if let Ok(r) = std::env::var("E2E_RUNTIME") {
            return Runtime(r);
        }
        let has_wslc = Command::new("wslc").arg("--help").output().is_ok();
        Runtime(if has_wslc { "wslc" } else { "docker" }.into())
    }

    fn command(&self) -> Command {
        Command::new(&self.0)
    }

    pub fn name(&self) -> &str {
        &self.0
    }
}

/// 起動した E2E コンテナ 1 つ。`Drop` で止める。
pub struct Sandbox {
    runtime: Runtime,
    name: String,
    out: PathBuf,
    display: Display,
}

/// ホスト側の設定用で、コンテナには渡さない環境変数
const HOST_ONLY_ENV: &[&str] = &[
    "E2E_RUNTIME",
    "E2E_IMAGE",
    "E2E_OUT_ROOT",
    "E2E_MODES",
    "E2E_START_TIMEOUT",
];

/// 結果(ログ・スクリーンショット)を残すフォルダ。実行ごとに新しく作り、既存のものは消さない。
pub fn new_run_dir() -> PathBuf {
    let root = std::env::var("E2E_OUT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("out"));
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    root.join(format!("run-{secs}-{}", std::process::id()))
}

/// コンテナの `-v` に渡すホスト側のパス。Windows の `\\?\C:\...` や `\` は実行系が解釈できないので `C:/...` にする。
fn mount_path(p: &Path) -> String {
    let s = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = s.to_string_lossy().replace('\\', "/");
    s.strip_prefix("//?/").map(str::to_owned).unwrap_or(s)
}

impl Sandbox {
    /// コンテナを起動し、`/out/e2e-status` が `ready` になるまで待つ。
    /// 失敗(起動できない・`failed:`・時間切れ)は、理由の文字列を返す(コンテナは止める)。
    pub fn start(runtime: &Runtime, display: Display, run_dir: &Path) -> Result<Sandbox, String> {
        let sandbox = Sandbox::launch(runtime, display, run_dir, &[])?;
        sandbox.wait_ready()?;
        Ok(sandbox)
    }

    /// コンテナを起動するだけで、ready は待たない。`extra_env` はコンテナに渡す環境変数(例: 待ち時間の短縮)。
    /// 起動の失敗そのものを調べる試験(E 群)が使う。普通は [`Sandbox::start`]。
    pub fn launch(
        runtime: &Runtime,
        display: Display,
        run_dir: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<Sandbox, String> {
        let out = run_dir.join(display.as_str());
        fs::create_dir_all(&out).map_err(|e| format!("{} を作れない: {e}", out.display()))?;
        let image = std::env::var("E2E_IMAGE").unwrap_or_else(|_| "fifty-four-e2e:latest".into());
        // コンテナ名は実行ごとに一意にする(A06 の parallelN は run-<時刻>-<pid> の下なので、親の名前も含める)。
        // 前回の中断で残ったコンテナや、同時に走る別の実行と名前がぶつからないように
        let leaf = |p: &Path| p.file_name().and_then(|s| s.to_str()).map(str::to_owned);
        let id = match run_dir.parent().and_then(leaf) {
            Some(parent) if parent.starts_with("run-") => {
                format!("{parent}-{}", leaf(run_dir).unwrap_or_default())
            }
            _ => leaf(run_dir).unwrap_or_else(|| "run".into()),
        };
        let name = format!("e2e-{id}-{}", display.as_str());

        let mut cmd = runtime.command();
        cmd.args(["run", "-d", "--rm", "--name", &name])
            .args(["-e", &format!("E2E_DISPLAY={}", display.as_str())])
            // 起動に失敗したらコンテナを止めたままにせず終了させる(ログは /out に残る)
            .args(["-e", "E2E_HOLD_ON_FAILURE=0"]);
        for (k, v) in std::env::vars() {
            if k.starts_with("E2E_") && !HOST_ONLY_ENV.contains(&k.as_str()) && k != "E2E_DISPLAY" {
                cmd.args(["-e", &format!("{k}={v}")]);
            }
        }
        for (k, v) in extra_env {
            cmd.args(["-e", &format!("{k}={v}")]);
        }
        cmd.args(["-v", &format!("{}:/out", mount_path(&out))])
            .arg(&image);

        let started = cmd
            .output()
            .map_err(|e| format!("{} run を実行できない: {e}", runtime.name()))?;
        if !started.status.success() {
            return Err(format!(
                "{} run に失敗: {}",
                runtime.name(),
                String::from_utf8_lossy(&started.stderr).trim()
            ));
        }
        let sandbox = Sandbox {
            runtime: runtime.clone(),
            name,
            out,
            display,
        };

        Ok(sandbox)
    }

    /// `/out/e2e-status` が `ready` になるまで待つ。`failed:` や時間切れは理由を返す。
    pub fn wait_ready(&self) -> Result<(), String> {
        let timeout = std::env::var("E2E_START_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        let mut outcome: Option<Result<(), String>> = None;
        wait::until(Duration::from_secs(timeout), || {
            let status = self.read_out("e2e-status").unwrap_or_default();
            let status = status.trim();
            outcome = match status {
                "ready" => Some(Ok(())),
                s if s.starts_with("failed") => Some(Err(format!(
                    "起動に失敗: {s} (ログ: {})",
                    self.out.join("e2e.log").display()
                ))),
                _ => None,
            };
            outcome.is_some()
        });
        outcome.unwrap_or_else(|| {
            Err(format!(
                "{timeout} 秒待っても起動しない(e2e-status: {})",
                self.read_out("e2e-status").unwrap_or_default().trim()
            ))
        })?;
        Ok(())
    }

    pub fn display(&self) -> Display {
        self.display
    }

    /// 結果を残すフォルダ(コンテナの `/out`)
    pub fn out_dir(&self) -> &Path {
        &self.out
    }

    /// `/out` 内のファイルを、ホストから直接読む
    pub fn read_out(&self, file: &str) -> Option<String> {
        fs::read_to_string(self.out.join(file)).ok()
    }

    /// コンテナ内でコマンドを実行する
    pub fn exec(&self, args: &[&str]) -> std::io::Result<Output> {
        self.runtime
            .command()
            .arg("exec")
            .arg(&self.name)
            .args(args)
            .output()
    }

    /// コンテナ内のコマンドが成功(終了コード 0)したか
    pub fn exec_ok(&self, args: &[&str]) -> bool {
        self.exec(args).map(|o| o.status.success()).unwrap_or(false)
    }

    /// コンテナ内のコマンドの標準出力
    pub fn exec_stdout(&self, args: &[&str]) -> String {
        self.exec(args)
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// Zed 本体のログ(`~/.local/share/zed/logs/Zed.log`)
    pub fn zed_log(&self) -> String {
        self.exec_stdout(&["cat", "/home/e2e/.local/share/zed/logs/Zed.log"])
    }

    /// スクリーンショットを撮り、ホスト側のパスを返す
    pub fn shot(&self, name: &str) -> Result<PathBuf, String> {
        let file = format!("{name}.png");
        if !self.exec_ok(&["e2e-shot", &format!("/out/{file}")]) {
            return Err("e2e-shot に失敗".into());
        }
        Ok(self.out.join(file))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // --rm で付けているので、止めれば消える。既に終わっていてもよい
        let _ = self.runtime.command().args(["kill", &self.name]).output();
    }
}
