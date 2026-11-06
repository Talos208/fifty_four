//! A. 起動(P0)。`e2e/TEST-ITEMS.md` の A01〜A05・A07 を表示サーバごとに判定する(A06 は `E2E_PARALLEL=N` のときだけ)。
//!
//! 実行: リポジトリのルートで `cargo e2e`(`.cargo/config.toml` の alias)
//!   絞り込み: `cargo e2e A04`、`cargo e2e x11`(名前は `A04::x11` の形)。表示サーバは `E2E_MODES=x11,wayland`。
//! コンテナは表示サーバごとに 1 つ、最初に使われたときに起動して、全試験で共有する(終わると止める)。

use fifty_four_e2e::{
    Sandbox,
    harness::{Case, Fixture, env_num, run, secs},
    image_check, wait,
};
use libtest_mimic::Failed;
use std::process::ExitCode;

const LSP_SPAWN: &str = "starting language server process";

fn lsp_spawned(log: &str) -> bool {
    log.lines()
        .any(|l| l.contains(LSP_SPAWN) && l.contains("fifty_four_lsp"))
}

fn lsp_failure(log: &str) -> Option<String> {
    log.lines()
        .find(|l| {
            l.contains("panicked")
                || l.contains("Failed to start language server")
                || l.contains("Server reset connection")
        })
        .map(|l| l.chars().take(160).collect())
}

fn lsp_alive(sb: &Sandbox) -> bool {
    sb.exec_ok(&["pgrep", "-f", "^/usr/local/bin/fifty_four_lsp"])
}

/// A01: ready になる。起動の失敗(`failed:` や時間切れ)はここで報告する
fn a01(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.get().map_err(Failed::from)?;
    let status = sb.read_out("e2e-status").unwrap_or_default();
    if status.trim() != "ready" {
        return Err(format!("e2e-status が ready でない: {}", status.trim()).into());
    }
    let log = sb.read_out("e2e.log").unwrap_or_default();
    if !log.contains("ready") {
        return Err("e2e.log に ready の行が無い".into());
    }
    Ok(())
}

/// A02: Zed の窓が出ている(紺一色・真っ黒ではない)。色数と最頻色の割合で判定する
fn a02(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    let png = sb.shot("A02")?;
    let stats = image_check::color_stats(&png)?;
    let (min_colors, max_top) = (
        env_num("E2E_A02_MIN_COLORS", 64),
        env_num("E2E_A02_MAX_TOP_PCT", 97),
    );
    if (stats.colors as u32) < min_colors {
        return Err(format!(
            "画面の色数が少ない({} < {min_colors})。窓が出ていないか撮影の失敗: {}",
            stats.colors,
            png.display()
        )
        .into());
    }
    if stats.top_percent > max_top {
        return Err(format!(
            "最頻色が画面の {}% を占める(> {max_top}%)。一色に近い: {}",
            stats.top_percent,
            png.display()
        )
        .into());
    }
    Ok(())
}

/// A03: .txt が FiftyFour 言語になる。
/// この LSP は拡張が持つ FiftyFour のバッファにしか起動しないので、LSP の起動行で代用する
/// (ステータスバーの文字は読まない)。
fn a03(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    if wait::until(secs(20), || lsp_spawned(&sb.zed_log())) {
        Ok(())
    } else {
        Err(
            "Zed.log に fifty_four_lsp の起動行が無い(.txt が FiftyFour と認識されていない?)"
                .into(),
        )
    }
}

/// A04: LSP が起動して落ちない。
/// 「落ちたログが無い」だけだと、起動前に調べて素通りする(実際に起きた panic は ready のあとの initialize で出た)。
/// そこで先に「起動した」ことを肯定的に確かめる(プロセスが居る + 起動時に作る FlightRecorder の DB がある)。
fn a04(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    if !wait::until(secs(20), || lsp_spawned(&sb.zed_log())) {
        return Err("LSP の起動行が無い".into());
    }
    if !wait::until(secs(20), || lsp_alive(sb)) {
        return Err("fifty_four_lsp のプロセスが居ない".into());
    }
    if !wait::until(secs(20), || {
        sb.exec_ok(&["test", "-s", "/usr/local/bin/db/fifty_four.db"])
    }) {
        return Err("FlightRecorder の DB(/usr/local/bin/db/fifty_four.db)ができていない".into());
    }
    // 起動直後の initialize で落ちるのを待つ
    std::thread::sleep(secs(u64::from(env_num("E2E_A04_SETTLE", 4))));
    if !lsp_alive(sb) {
        return Err("起動後に fifty_four_lsp が落ちた".into());
    }
    match lsp_failure(&sb.zed_log()) {
        Some(line) => Err(format!("Zed.log に LSP の失敗が出ている: {line}").into()),
        None => Ok(()),
    }
}

/// A05: 拡張が読み込まれる(`extensions updated. loading N`、N ≥ 1)
fn a05(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    let loaded = |log: &str| {
        log.lines().any(|l| {
            l.split_once("extensions updated. loading ")
                .and_then(|(_, rest)| rest.split(',').next())
                .and_then(|n| n.trim().parse::<u32>().ok())
                .is_some_and(|n| n >= 1)
        })
    };
    if wait::until(secs(20), || loaded(&sb.zed_log())) {
        Ok(())
    } else {
        Err("Zed.log に拡張を読み込んだ行が無い".into())
    }
}

/// A07: 起動し直さずに済む。起動し直しは 2 段階ある。
/// - entrypoint が、Zed が起動途中で落ちたら Zed だけ起動し直す(回数は `/out/zed-attempts`)
/// - ハーネスが、コンテナの起動に失敗したらコンテナごと作り直す(`Fixture::container_attempts`)
///
/// どちらも Wayland の panic 対策なので、起動し直した事実は警告にして残す。既定では警告にとどめ、
/// `E2E_STRICT_A07=1` なら失敗にする。
fn a07(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;
    let zed_attempts: u32 = sb
        .read_out("zed-attempts")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1);
    let container_attempts = fx.container_attempts();
    if zed_attempts <= 1 && container_attempts <= 1 {
        return Ok(());
    }
    let msg = format!(
        "起動し直した: コンテナ {container_attempts} 回目で起動、Zed の起動 {zed_attempts} 回(落ちた回のログ: {}/Zed.attempt*.log、前のコンテナは retryN/)",
        sb.out_dir().display()
    );
    if std::env::var("E2E_STRICT_A07").is_ok_and(|v| v == "1") {
        Err(msg.into())
    } else {
        eprintln!("WARN A07 [{}]: {msg}", sb.display().as_str());
        Ok(())
    }
}

/// A06: 同時に起動しても全部が ready になる。`E2E_PARALLEL=N`(N ≥ 2)のときだけ実行する。
/// 共有のコンテナとは別に、N 本を同時に起動して、それぞれ ready まで見る(負荷で起きる競合を探す)。
fn a06(fx: &Fixture) -> Result<(), Failed> {
    let n = env_num("E2E_PARALLEL", 0);
    if n < 2 {
        return Ok(()); // 登録時に外してあるが、念のため
    }
    let handles: Vec<_> = (1..=n)
        .map(|i| {
            let (runtime, display) = (fx.runtime.clone(), fx.display);
            let dir = fx.run_dir.join(format!("parallel{i}"));
            std::thread::spawn(move || Sandbox::start(&runtime, display, &dir).map(drop))
        })
        .collect();
    let failures: Vec<String> = handles
        .into_iter()
        .enumerate()
        .filter_map(|(i, h)| match h.join() {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("#{}: {e}", i + 1)),
            Err(_) => Some(format!("#{}: スレッドが panic", i + 1)),
        })
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{n} 本中 {} 本が失敗: {}",
            failures.len(),
            failures.join(" / ")
        )
        .into())
    }
}

const CASES: &[(&str, Case)] = &[
    ("A01", a01),
    ("A02", a02),
    ("A03", a03),
    ("A04", a04),
    ("A05", a05),
    ("A06", a06), // E2E_PARALLEL が 2 以上のときだけ登録する
    ("A07", a07),
];

fn main() -> ExitCode {
    run("A", CASES, |id| {
        id != "A06" || env_num("E2E_PARALLEL", 0) >= 2
    })
}
