//! E. ハーネス自身(`e2e/TEST-ITEMS.md` の E01〜E07)。
//!
//! 起動の失敗を調べる試験(E01〜E03)は、共有のコンテナとは別に、わざと失敗させたコンテナを立てる。
//! E06 はバッファを書き換えて保存するので最後に置き、終わったらファイルを元に戻す。

use fifty_four_e2e::{
    Sandbox,
    harness::{Case, Fixture, env_num, run, secs},
    image_check, wait,
};
use libtest_mimic::Failed;
use std::process::ExitCode;

/// わざと起動に失敗させたコンテナを返す(失敗の理由つき)。ready になってしまったら試験の失敗
fn launch_failing(
    fx: &Fixture,
    tag: &str,
    env: &[(&str, &str)],
) -> Result<(Sandbox, String), Failed> {
    let dir = fx.run_dir.join(tag);
    let sb = Sandbox::launch(&fx.runtime, fx.display, &dir, env)?;
    match sb.wait_ready() {
        Ok(()) => Err("失敗させたのに ready になった".into()),
        Err(reason) => Ok((sb, reason)),
    }
}

/// E01: 起動に失敗したら、`e2e-status` に理由、`e2e.log` にプロセス一覧と各ログの末尾が残る
fn e01(fx: &Fixture) -> Result<(), Failed> {
    // Zed の窓を 0 秒しか待たない = 必ず時間切れ
    let (sb, reason) = launch_failing(fx, "e01", &[("E2E_ZED_WINDOW_TIMEOUT", "0")])?;
    let status = sb.read_out("e2e-status").unwrap_or_default();
    if !status.trim().starts_with("failed:") {
        return Err(format!("e2e-status が failed: で始まらない: {status}").into());
    }
    if !reason.contains("起動に失敗") {
        return Err(format!("理由の形が想定と違う: {reason}").into());
    }
    // 診断の収集は status を書いたあとなので、出そろうのを待つ
    let collected = wait::until(secs(20), || {
        sb.read_out("e2e.log")
            .is_some_and(|l| l.contains("---- processes") && l.contains("(last 20 lines)"))
    });
    if collected {
        Ok(())
    } else {
        Err("e2e.log にプロセス一覧と各ログの末尾が出ていない".into())
    }
}

/// E02: 起動に失敗したら(`E2E_HOLD_ON_FAILURE=0`)、コンテナが止まったままにならず終了する
fn e02(fx: &Fixture) -> Result<(), Failed> {
    let (sb, _) = launch_failing(fx, "e02", &[("E2E_ZED_WINDOW_TIMEOUT", "0")])?;
    if wait::until(secs(40), || !sb.exec_ok(&["true"])) {
        Ok(())
    } else {
        Err("起動に失敗してもコンテナが終了しない(待機したまま)".into())
    }
}

/// E03: 待ち時間を環境変数で変えられる。IME の待ちを 0 秒にすると、その待ちで時間切れになる
fn e03(fx: &Fixture) -> Result<(), Failed> {
    let (sb, _) = launch_failing(
        fx,
        "e03",
        &[("E2E_IME_TIMEOUT", "0"), ("E2E_ZED_START_ATTEMPTS", "1")],
    )?;
    let log = sb.read_out("e2e.log").unwrap_or_default();
    if log.contains("timeout waiting for IME connection") {
        Ok(())
    } else {
        Err(format!(
            "IME の待ちの時間切れが e2e.log に無い(別の理由で失敗?): {}",
            sb.read_out("e2e-status").unwrap_or_default().trim()
        )
        .into())
    }
}

/// E04: 同じイメージで連続して起動しても、毎回 ready になる(待ち合わせが足りないと揺れる)。
/// `E2E_REPEAT=N`(N ≥ 2)のときだけ。N 本を続けて(同時ではなく)起動する。
fn e04(fx: &Fixture) -> Result<(), Failed> {
    let n = env_num("E2E_REPEAT", 0);
    let failures: Vec<String> = (1..=n)
        .filter_map(|i| {
            let dir = fx.run_dir.join(format!("repeat{i}"));
            Sandbox::start(&fx.runtime, fx.display, &dir)
                .err()
                .map(|e| format!("{i} 回目: {e}"))
        })
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{n} 回中 {} 回が失敗: {}",
            failures.len(),
            failures.join(" / ")
        )
        .into())
    }
}

/// E05: スクリーンショットが撮れて、画面の大きさ(1600x1000)で読める
fn e05(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    let png = sb.shot("E05")?;
    let size = image_check::dimensions(&png)?;
    if size == (1600, 1000) {
        Ok(())
    } else {
        Err(format!("画面の大きさが 1600x1000 でない: {size:?}").into())
    }
}

/// E07: IME の入力コンテキストが作られている(`e2e-ime ready`。fcitx5 の DebugInfo に `focus:1`)
fn e07(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    if wait::until(secs(10), || sb.exec_ok(&["e2e-ime", "ready"])) {
        Ok(())
    } else {
        Err("fcitx5 にフォーカスを持つ入力コンテキストが無い".into())
    }
}

const TEST_TXT: &str = "/home/e2e/workspace/test.txt";
const BACKUP: &str = "/tmp/test.txt.orig";

/// E06: 入力経路が生きている。IME を ON にして `wagahaiha` → 変換 → 確定で、バッファに「吾輩は」が入る。
/// バッファの文字は画面から読まずに、保存(ctrl+s)したファイルの中身で判定する。
fn e06(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    if !wait::until(secs(10), || sb.exec_ok(&["e2e-ime", "ready"])) {
        return Err("IME に繋がっていない(E07 を参照)".into());
    }
    if !sb.exec_ok(&["cp", TEST_TXT, BACKUP]) {
        return Err("test.txt を退避できない".into());
    }

    let result = (|| {
        let step = |args: &[&str]| -> Result<(), Failed> {
            if sb.exec_ok(args) {
                Ok(())
            } else {
                Err(format!("{args:?} に失敗").into())
            }
        };
        step(&["e2e-ime", "on"])?;
        step(&["e2e-type", "wagahaiha"])?;
        std::thread::sleep(secs(1)); // 変換候補が出るのを待つ
        step(&["e2e-key", "space"])?;
        std::thread::sleep(secs(1));
        step(&["e2e-key", "Return"])?;
        // 保存して、ファイルに入るのを待つ(保存のキーが取りこぼされることがあるので、毎回押す)
        let saved = wait::until(secs(15), || {
            sb.exec_ok(&["e2e-key", "ctrl+s"]);
            sb.exec_stdout(&["cat", TEST_TXT]).contains("吾輩は")
        });
        if saved {
            Ok(())
        } else {
            Err(format!(
                "保存したファイルに「吾輩は」が無い: {:?}",
                sb.exec_stdout(&["cat", TEST_TXT])
            )
            .into())
        }
    })();

    // 後始末(成功でも失敗でも): IME を切り、ファイルを元に戻す。Zed は外部の変更を読み直す
    sb.exec_ok(&["e2e-ime", "off"]);
    sb.exec_ok(&["cp", BACKUP, TEST_TXT]);
    result
}

const CASES: &[(&str, Case)] = &[
    ("E01", e01),
    ("E02", e02),
    ("E03", e03),
    ("E04", e04), // E2E_REPEAT が 2 以上のときだけ登録する
    ("E05", e05),
    ("E07", e07),
    ("E06", e06), // バッファを書き換えるので最後
];

fn main() -> ExitCode {
    run("E", CASES, |id| {
        id != "E04" || env_num("E2E_REPEAT", 0) >= 2
    })
}
