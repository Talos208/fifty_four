use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(300);

/// 条件が成り立つまで待つ。成り立てば true、時間切れなら false。
pub fn until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(INTERVAL);
    }
}
