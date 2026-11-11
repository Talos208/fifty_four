//! スクリーンショットの文字認識(ホスト側)。
//!
//! コンテナは「撮る」と「押す」だけをして、何が映っているかはホストが読む。`ocrs`(純粋な Rust)で、
//! 画面の文字とその座標を取る。読めるのは英数字と記号だけ(日本語は読めない)なので、
//! モデル名のような英数字の表示を確かめるのに使う。
//!
//! モデルのファイル(約 12MB)は `ocrs-cli` と同じ `~/.cache/ocrs/` に置く。無ければ初回に
//! `curl` で取ってくる(`E2E_OCR_MODELS` で置き場所を変えられる)。

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};
use rten::Model;

const MODEL_BASE_URL: &str = "https://ocrs-models.s3-accelerate.amazonaws.com";
const DETECTION_MODEL: &str = "text-detection.onnx";
const RECOGNITION_MODEL: &str = "text-recognition.onnx";

/// 画面に映っている、空白で区切られた 1 かたまりの文字(単語)と、その位置(画素)。
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub text: String,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// 1 枚の画面から読めた文字。`lines` は行ごと、`words` は単語ごと。
#[derive(Debug, Default)]
pub struct Screen {
    pub lines: Vec<Item>,
    pub words: Vec<Item>,
}

impl Screen {
    /// 読めた行を、見やすい複数行の文字列にする(失敗の報告に使う)
    pub fn dump(&self) -> String {
        self.lines
            .iter()
            .map(|l| format!("  ({:>4},{:>4}) {}", l.left, l.top, l.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `needle` に(OCR の読み間違いを許して)近い単語のうち、条件 `pred` を満たす最初のもの
    pub fn find_word(&self, needle: &str, pred: impl Fn(&Item) -> bool) -> Option<&Item> {
        self.words
            .iter()
            .find(|w| pred(w) && fuzzy_contains(&normalize(&w.text), &normalize(needle)))
    }

    /// `needle`(複数の単語でもよい)が読めた行に含まれるか。条件 `pred` を満たす行だけを見る
    pub fn has_line_with(&self, needle: &str, pred: impl Fn(&Item) -> bool) -> bool {
        let needle = normalize(needle);
        self.lines
            .iter()
            .any(|l| pred(l) && fuzzy_contains(&normalize(&l.text), &needle))
    }
}

/// 比べやすいように、英数字だけを小文字にして残す(空白・記号の読み違いに左右されないため)
pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `needle` が `haystack` に含まれるか。OCR は 1 文字ほど読み間違えることがある
/// (例: `fifty-four` が `fiftv-four`)ので、長さの 1/6(最低 1 文字)までの違いを許す。
pub fn fuzzy_contains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.contains(needle) {
        return true;
    }
    let h: Vec<char> = haystack.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    let allowed = (n.len() / 6).max(1);
    // 長さの違う窓も見る(1 文字の抜け・増えにも対応する)
    for len in n.len().saturating_sub(allowed)..=n.len() + allowed {
        if len == 0 || len > h.len() {
            continue;
        }
        for start in 0..=h.len() - len {
            if levenshtein(&h[start..start + len], &n) <= allowed {
                return true;
            }
        }
    }
    false
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// モデルのファイルの置き場。`E2E_OCR_MODELS`、無ければ `~/.cache/ocrs`(`ocrs-cli` と同じ)
pub fn models_dir() -> Result<PathBuf, String> {
    if let Some(d) = std::env::var_os("E2E_OCR_MODELS") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or(
            "ホームのフォルダが分からない(HOME / USERPROFILE)。E2E_OCR_MODELS で置き場所を指定する",
        )?;
    Ok(PathBuf::from(home).join(".cache").join("ocrs"))
}

/// モデルのファイルがあればそのパスを、無ければ `curl` で取ってきて返す
fn ensure_model(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let path = dir.join(name);
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{} を作れない: {e}", dir.display()))?;
    let url = format!("{MODEL_BASE_URL}/{name}");
    eprintln!("e2e: OCR のモデルを取得: {url} → {}", path.display());
    // 途中で止まったファイルを本物と取り違えないよう、別名で取ってから置き換える
    let part = dir.join(format!("{name}.part"));
    let status = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(&part)
        .arg(&url)
        .status()
        .map_err(|e| {
            format!(
                "curl を実行できない({e})。{url} を {} に置く",
                path.display()
            )
        })?;
    if !status.success() {
        return Err(format!(
            "OCR のモデルを取得できない: {url}。手で {} に置く",
            path.display()
        ));
    }
    std::fs::rename(&part, &path).map_err(|e| format!("{} を置けない: {e}", path.display()))?;
    Ok(path)
}

/// スクリーンショット(PNG)を読んで、画面の文字と座標を返す。呼ぶたびにモデルを読み込む
/// (試験は 1 つずつ動き、読む回数も少ないので、使い回しは要らない)。
pub fn read(png: &Path) -> Result<Screen, String> {
    let dir = models_dir()?;
    let load = |name: &str| -> Result<Model, String> {
        let p = ensure_model(&dir, name)?;
        Model::load_file(&p).map_err(|e| format!("{} を読めない: {e}", p.display()))
    };
    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: Some(load(DETECTION_MODEL)?),
        recognition_model: Some(load(RECOGNITION_MODEL)?),
        ..Default::default()
    })
    .map_err(|e| format!("OCR を初期化できない: {e}"))?;

    let img = image::open(png)
        .map_err(|e| format!("{} を読めない: {e}", png.display()))?
        .into_rgb8();
    let source = ImageSource::from_bytes(img.as_raw(), img.dimensions())
        .map_err(|e| format!("画像を OCR に渡せない: {e}"))?;
    let input = engine
        .prepare_input(source)
        .map_err(|e| format!("画像を OCR に渡せない: {e}"))?;
    let word_rects = engine
        .detect_words(&input)
        .map_err(|e| format!("文字の検出に失敗: {e}"))?;
    let line_rects = engine.find_text_lines(&input, &word_rects);
    let lines = engine
        .recognize_text(&input, &line_rects)
        .map_err(|e| format!("文字の認識に失敗: {e}"))?;

    let mut screen = Screen::default();
    for line in lines.into_iter().flatten() {
        let r = line.bounding_rect();
        screen.lines.push(Item {
            text: line.to_string(),
            left: r.left(),
            top: r.top(),
            right: r.right(),
            bottom: r.bottom(),
        });
        for w in line.words() {
            let r = w.bounding_rect();
            screen.words.push(Item {
                text: w.chars().iter().map(|c| c.char).collect(),
                left: r.left(),
                top: r.top(),
                right: r.right(),
                bottom: r.bottom(),
            });
        }
    }
    Ok(screen)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> String {
        normalize(s)
    }

    #[test]
    fn normalize_keeps_only_lowercase_alphanumerics() {
        assert_eq!(n("Default (recommended)"), "defaultrecommended");
        assert_eq!(n("Opus 5.5"), "opus55");
    }

    #[test]
    fn fuzzy_contains_tolerates_one_misread() {
        assert!(fuzzy_contains(
            &n("New fiftv-four Thread"),
            &n("fifty-four")
        ));
        assert!(fuzzy_contains(
            &n("Claude 3,5 Sonnet"),
            &n("Claude 3.5 Sonnet")
        ));
        assert!(fuzzy_contains(&n("Fable"), &n("Fable")));
    }

    #[test]
    fn fuzzy_contains_rejects_different_names() {
        assert!(!fuzzy_contains(&n("Sonnet"), &n("Haiku")));
        // 版の違い(5.5 と 5.2)は別のモデル。短い名前でも取り違えない
        assert!(!fuzzy_contains(&n("Fable 5.1"), &n("Fable 5.2 extra")));
        assert!(!fuzzy_contains(&n("Opus"), &n("Fable")));
    }
}
