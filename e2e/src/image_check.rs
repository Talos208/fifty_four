use std::{collections::HashMap, path::Path};

/// 画面が「Zed の窓が映っている」ものかを判定するための統計。
#[derive(Debug, PartialEq, Eq)]
pub struct ColorStats {
    /// 画像にある色の種類数
    pub colors: usize,
    /// 最頻色が画面に占める割合(%、切り捨て)
    pub top_percent: u32,
}

/// 画像の大きさ(幅, 高さ)。デコードはしない
pub fn dimensions(png: &Path) -> Result<(u32, u32), String> {
    image::image_dimensions(png).map_err(|e| format!("{} を読めない: {e}", png.display()))
}

/// 実画面は 3000 色超・最頻色は約 73%。紺一色(窓が出ていない)・真っ黒(撮影の失敗)は 1 色・100%。
pub fn color_stats(png: &Path) -> Result<ColorStats, String> {
    let img = image::open(png)
        .map_err(|e| format!("{} を読めない: {e}", png.display()))?
        .to_rgb8();
    let total = u64::from(img.width()) * u64::from(img.height());
    if total == 0 {
        return Err(format!("{} が空の画像", png.display()));
    }
    let mut counts: HashMap<[u8; 3], u64> = HashMap::new();
    for p in img.pixels() {
        *counts.entry(p.0).or_default() += 1;
    }
    let top = counts.values().copied().max().unwrap_or(0);
    Ok(ColorStats {
        colors: counts.len(),
        top_percent: u32::try_from(top * 100 / total).unwrap_or(100),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(name: &str, img: image::RgbImage) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ff4_e2e_{}_{name}.png", std::process::id()));
        img.save(&p).unwrap();
        p
    }

    #[test]
    fn solid_color_is_one_color_and_100_percent() {
        let p = write(
            "solid",
            image::RgbImage::from_pixel(10, 10, image::Rgb([0x20, 0x30, 0x50])),
        );
        assert_eq!(
            color_stats(&p).unwrap(),
            ColorStats {
                colors: 1,
                top_percent: 100
            }
        );
    }

    #[test]
    fn mixed_image_counts_colors_and_top_share() {
        let mut img = image::RgbImage::from_pixel(10, 10, image::Rgb([250, 250, 250]));
        for x in 0..10 {
            img.put_pixel(x, 0, image::Rgb([x as u8, 0, 0])); // 上の 1 行(10 画素)は別々の色
        }
        let p = write("mixed", img);
        // 白は 90 画素、赤系は 10 色(x=0 は黒に近い赤)
        assert_eq!(
            color_stats(&p).unwrap(),
            ColorStats {
                colors: 11,
                top_percent: 90
            }
        );
    }
}
