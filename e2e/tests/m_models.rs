//! M. ACP エージェントのモデルセレクタ(`e2e/TEST-ITEMS.md` の M01)。
//!
//! Agent Panel で `fifty-four`(`fifty_four_lsp --acp`)の会話を作り、モデルのセレクタに出る一覧が、
//! コンテナの `claude` が「選べる」と答える一覧と一致するかを、**画面の文字を読んで**確かめる。
//! 正解は、本体と同じ `claude` に `initialize` を送って得る(`e2e-claude-models`)。偽物は使わない。
//!
//! 操作はキーボードだけ(コマンドパレットのアクションと、メニューの Down / Return)。マウスは使わない:
//! Wayland の headless な sway にはポインタ機器が無く、クリックが Zed に届かないため。
//! 文字を読むのはホスト側(`ocr`)で、コンテナは「撮る」と「押す」だけをする。
//!
//! 実行: `cargo e2e M01`。ログイン情報は要らない(未ログインでも一覧は出る。ホストの
//! `E2E_CLAUDE_OAUTH_TOKEN` があればコンテナに渡して、ログイン済みの一覧で確かめる)。

use fifty_four_e2e::{
    Sandbox,
    harness::{Case, Fixture, run, secs},
    ocr::{self, Screen},
    wait,
};
use libtest_mimic::Failed;
use std::{process::ExitCode, time::Duration};

/// Agent Panel は画面の左側。その外(エディタ・ファイル一覧)の文字は対象にしない
const PANEL_RIGHT: i32 = 700;
/// 会話の入力欄の下にあるセレクタの行は、画面の下の方
const SELECTOR_ROW_TOP: i32 = 800;
/// セレクタを開いたときの一覧は、入力欄より下の、パネルの中に出る
const DROPDOWN_TOP: i32 = 600;

/// 画面を撮って読み、`ready` が `Some` を返すまで繰り返す。
/// 時間切れなら、最後に読めた画面の文字を添えて失敗にする(何が映っていたかが分かるように)。
fn wait_screen<T>(
    sb: &Sandbox,
    shot: &str,
    timeout: Duration,
    what: &str,
    mut ready: impl FnMut(&Screen) -> Option<T>,
) -> Result<T, Failed> {
    let mut last = String::from("(まだ読めていない)");
    let mut found = None;
    wait::until(timeout, || {
        let Ok(png) = sb.shot(shot) else {
            return false;
        };
        match ocr::read(&png) {
            Ok(screen) => {
                found = ready(&screen);
                if found.is_none() {
                    last = screen.dump();
                }
            }
            Err(e) => last = e,
        }
        found.is_some()
    });
    found.ok_or_else(|| {
        Failed::from(format!(
            "{what}: {}秒待っても見つからない。最後に読めた画面({shot}.png):\n{last}",
            timeout.as_secs()
        ))
    })
}

/// M01: セレクタの一覧が、コンテナの claude が答えるモデルの一覧と一致する
fn m01(fx: &Fixture) -> Result<(), Failed> {
    let sb = fx.running()?;

    // 正解: 本体が問い合わせるのと同じ claude が、選べると答えるモデルの表示名
    let expected: Vec<String> = sb
        .exec_stdout(&["e2e-claude-models"])
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let Some(default_name) = expected.first() else {
        return Err("コンテナの claude からモデル一覧を取れない(e2e-claude-models)".into());
    };
    // 先頭は `default`(例: `Default (recommended)`)。会話を開いたあと、セレクタの行にこの最後の単語が出る
    let default_token = default_name
        .split_whitespace()
        .last()
        .unwrap_or(default_name)
        .to_string();

    // Agent Panel を開き、「+」メニューから fifty-four の会話を作る
    // (パネルが閉じているときのメニューは、パネルを開くだけで出ないので、先にパネルを開く)。
    // メニューは上から Zed Agent・Terminal・fifty-four の順なので、Down を 3 回で fifty-four に着く
    sb.run_action("agent: toggle focus")?;
    std::thread::sleep(secs(2));
    sb.run_action("agent: toggle new thread menu")?;
    std::thread::sleep(secs(1));
    sb.keys(&["Down", "Down", "Down", "Return"])?;

    // 会話ができるのを待つ(session/new は、普段 1〜2 秒、遅いときは 30 秒ほどかかる)。
    // セレクタの行(画面の下の方)に、既定の表示が出る。それが claude の答えと違う
    // (例: 「既定(CLI 任せ)」)なら、claude に問い合わせられず静的な一覧に戻っている
    wait_screen(
        sb,
        "M01-thread",
        secs(90),
        &format!(
            "セレクタの既定の表示「{default_name}」が出ない\
             (fifty-four の会話を作れていない?静的な一覧に戻っている?)"
        ),
        |s| {
            s.find_word(&default_token, |w| {
                w.left < PANEL_RIGHT && w.top > SELECTOR_ROW_TOP
            })
            .map(|_| ())
        },
    )?;

    // セレクタを開いて、一覧を読む
    sb.run_action("agent: toggle model selector")?;
    let missing = wait_screen(
        sb,
        "M01-selector",
        secs(30),
        "セレクタの一覧が開かない",
        |s| {
            // 一覧が開くと「Select an option…」の入力欄が出る。読めたら、その回の判定を返す
            let opened = s.has_line_with("Select an option", |l| l.left < PANEL_RIGHT);
            if !opened {
                return None;
            }
            Some(
                expected
                    .iter()
                    .filter(|name| {
                        !s.has_line_with(name, |l| l.left < PANEL_RIGHT && l.top > DROPDOWN_TOP)
                    })
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        },
    )?;

    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "セレクタに出ていないモデルがある: {missing:?}\n\
             claude の答え: {expected:?}\n\
             読めた画面は M01-selector.png を見る"
        )
        .into())
    }
}

const CASES: &[(&str, Case)] = &[("M01", m01)];

fn main() -> ExitCode {
    run("M", CASES, |_| true)
}
