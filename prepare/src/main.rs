//! 開発用タスクランナー。`cargo prepare package [--release]` で
//! `fifty_four_lsp` をビルドし、配布用の 1 ディレクトリ `dist/fifty-four/` に
//! 拡張一式(extension.toml / extension.wasm / languages/)+ LSP バイナリ + data/ を
//! まとめる。利用マシンでは任意の場所へコピーし、Zed の Extensions ページから
//! 「Install Dev Extension」でそのディレクトリを選ぶだけでインストールできる
//! (Cargo.toml を含めないため利用マシンで再ビルドは走らず、Rust ツールチェーン不要)。
//!
//! extension.wasm は cargo の生成物をそのまま使わず、Zed が「Install Dev Extension」時に
//! 行う処理(デバッグ用カスタムセクションの strip + `zed:api-version` セクションの検証。
//! zed-industries/zed の crates/extension/src/extension_builder.rs より)を再現して
//! 毎回生成する。これにより Zed 側では検証だけが走りビルドは発生しない
//! (Install Dev Extension 自体は必要、そこでの再ビルドが不要という意味)。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const EXTENSION_ID: &str = "fifty-four";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let release = args.iter().any(|a| a == "--release");
    match args.first().map(|s| s.as_str()) {
        Some("package") => package(release),
        // extension.wasm の生成(ビルド + strip + 検証)だけを行う。
        // cargo に post-build フックが無いため `cargo build` へは統合できず、
        // このサブコマンドが「wasm のビルド」に相当する。
        Some("wasm") => {
            build_extension_wasm(&repo_root().join("extension"), release);
        }
        _ => {
            eprintln!("Usage: cargo prepare <package|wasm> [--release]");
            std::process::exit(1);
        }
    }
}

fn package(release: bool) {
    let repo_root = repo_root();
    let profile_dir = if release { "release" } else { "debug" };
    let profile_label = if release { "release" } else { "debug" };

    println!("Building fifty_four_lsp ({})...", profile_label);
    run_cargo_build(&repo_root, &["-p", "fifty_four_lsp"], release);

    // ---- extension.wasm: Zed の Install Dev Extension と同じ処理で生成する ----
    let ext_dir = repo_root.join("extension");
    let src_wasm = build_extension_wasm(&ext_dir, release);

    // ---- dist/fifty-four/: 配布用ディレクトリを組み立てる ----
    let dist_dir = repo_root.join("dist").join(EXTENSION_ID);
    if dist_dir.exists() {
        fs::remove_dir_all(&dist_dir)
            .unwrap_or_else(|e| panic!("削除に失敗 {:?}: {}", dist_dir, e));
    }
    fs::create_dir_all(&dist_dir).unwrap_or_else(|e| panic!("作成に失敗 {:?}: {}", dist_dir, e));

    copy_file(&src_wasm, &dist_dir.join("extension.wasm"));
    copy_file(
        &ext_dir.join("extension.toml"),
        &dist_dir.join("extension.toml"),
    );
    copy_dir_recursive(&ext_dir.join("languages"), &dist_dir.join("languages"));

    let exe_name = if cfg!(windows) {
        "fifty_four_lsp.exe"
    } else {
        "fifty_four_lsp"
    };
    let src_exe = repo_root.join("target").join(profile_dir).join(exe_name);
    copy_file(&src_exe, &dist_dir.join(exe_name));

    copy_dir_recursive(&repo_root.join("data"), &dist_dir.join("data"));

    // Cargo.toml / src / target は含めない: あると利用マシンで再ビルドが走り、
    // ツールチェーンがなければインストール自体が失敗する。

    println!();
    println!("package 完了: {:?}", dist_dir);
    println!("利用マシンでの手順:");
    println!("  1. {:?} を利用マシンの任意の場所にコピー", dist_dir);
    println!("  2. Zed → Extensions → Install Dev Extension でそのディレクトリを選択");
    println!(
        "  3. %APPDATA%\\Zed\\settings.json に lsp.{}.binary.path = コピー先の {} の絶対パスを設定",
        EXTENSION_ID, exe_name
    );
    println!(
        "  4. API キーは Windows のユーザー環境変数に設定し(settings.json の binary.env は平文なので不可)、Zed を完全再起動"
    );
}

/// 拡張を wasm32-wasip2 でビルドし、Zed の「Install Dev Extension」と同じ後処理
/// (デバッグ用カスタムセクションの strip + `zed:api-version` の検証)を行って
/// `extension/extension.wasm` に書き出し、そのパスを返す。
/// 処理内容は zed-industries/zed の crates/extension/src/extension_builder.rs に準拠。
fn build_extension_wasm(ext_dir: &Path, release: bool) -> PathBuf {
    let profile_dir = if release { "release" } else { "debug" };
    println!(
        "Building fifty_four_extension (wasm32-wasip2, {})...",
        profile_dir
    );
    run_cargo_build(ext_dir, &["--target", "wasm32-wasip2"], release);

    let cargo_wasm = ext_dir
        .join("target")
        .join("wasm32-wasip2")
        .join(profile_dir)
        .join("fifty_four_extension.wasm");
    let input =
        fs::read(&cargo_wasm).unwrap_or_else(|e| panic!("読み込みに失敗 {:?}: {}", cargo_wasm, e));

    let stripped = strip_custom_sections(&input)
        .unwrap_or_else(|e| panic!("カスタムセクションの strip に失敗: {}", e));

    let (major, minor, patch) = parse_wasm_extension_version(&stripped).unwrap_or_else(|| {
        panic!("生成された wasm に有効な zed:api-version セクションがありません")
    });
    println!("zed:api-version = {}.{}.{}", major, minor, patch);

    let out = ext_dir.join("extension.wasm");
    fs::write(&out, &stripped).unwrap_or_else(|e| panic!("書き込みに失敗 {:?}: {}", out, e));
    println!("wrote {:?} ({} bytes, stripped from {})", out, stripped.len(), input.len());
    out
}

/// Zed の extension_builder.rs と同じ基準でカスタムセクションを strip する。
/// `name` / `component-type:*` / `dylink.0` / `zed:api-version` のみ保持し、
/// `.debug_*` 等のデバッグ情報を落とす(サイズ削減。ロード動作には影響しない)。
fn strip_custom_sections(input: &[u8]) -> Result<Vec<u8>, String> {
    use wasm_encoder::{ComponentSectionId, Encode, RawSection, Section};
    use wasmparser::Payload::*;

    let strip_custom_section = |name: &str| {
        name != "name"
            && !name.starts_with("component-type:")
            && name != "dylink.0"
            && name != "zed:api-version"
    };

    let mut output: Vec<u8> = Vec::new();
    let mut stack: Vec<Vec<u8>> = Vec::new();

    for payload in wasmparser::Parser::new(0).parse_all(input) {
        let payload = payload.map_err(|e| format!("wasm のパースに失敗: {}", e))?;

        match payload {
            Version { encoding, .. } => {
                output.extend_from_slice(match encoding {
                    wasmparser::Encoding::Component => &wasm_encoder::Component::HEADER,
                    wasmparser::Encoding::Module => &wasm_encoder::Module::HEADER,
                });
            }
            ModuleSection { .. } | ComponentSection { .. } => {
                stack.push(std::mem::take(&mut output));
                continue;
            }
            End { .. } => {
                let mut parent = match stack.pop() {
                    Some(c) => c,
                    None => break,
                };
                if output.starts_with(&wasm_encoder::Component::HEADER) {
                    parent.push(ComponentSectionId::Component as u8);
                    output.encode(&mut parent);
                } else {
                    parent.push(ComponentSectionId::CoreModule as u8);
                    output.encode(&mut parent);
                }
                output = parent;
            }
            _ => {}
        }

        if let CustomSection(c) = &payload
            && strip_custom_section(c.name())
        {
            continue;
        }
        if let Some((id, range)) = payload.as_section() {
            RawSection {
                id,
                data: &input[range],
            }
            .append_to(&mut output);
        }
    }

    Ok(output)
}

/// wasm 内の `zed:api-version` カスタムセクション(6バイト: BE u16 × major/minor/patch)を
/// デコードする。無効・欠如時は `None`。
fn parse_wasm_extension_version(wasm_bytes: &[u8]) -> Option<(u16, u16, u16)> {
    let mut version = None;
    for part in wasmparser::Parser::new(0).parse_all(wasm_bytes) {
        if let Ok(wasmparser::Payload::CustomSection(s)) = part
            && s.name() == "zed:api-version"
        {
            let data = s.data();
            if data.len() != 6 {
                return None;
            }
            version = Some((
                u16::from_be_bytes([data[0], data[1]]),
                u16::from_be_bytes([data[2], data[3]]),
                u16::from_be_bytes([data[4], data[5]]),
            ));
        }
    }
    version
}

fn run_cargo_build(cwd: &Path, extra_args: &[&str], release: bool) {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    for a in extra_args {
        cmd.arg(a);
    }
    if release {
        cmd.arg("--release");
    }
    let status = cmd
        .current_dir(cwd)
        .status()
        .expect("cargo build の起動に失敗しました");
    if !status.success() {
        eprintln!("cargo build に失敗しました ({:?})", cwd);
        std::process::exit(1);
    }
}

/// prepare クレート自身の親ディレクトリ = リポジトリルート。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn copy_file(src: &Path, dst: &Path) {
    fs::copy(src, dst).unwrap_or_else(|e| panic!("コピーに失敗 {:?} -> {:?}: {}", src, dst, e));
    println!("copied {:?} -> {:?}", src, dst);
}

/// `src` 配下を `dst` へ丸ごと再帰コピーする(シンボリックリンクは辿らない)。
fn copy_dir_recursive(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap_or_else(|e| panic!("ディレクトリの作成に失敗 {:?}: {}", dst, e));
    for entry in fs::read_dir(src)
        .unwrap_or_else(|e| panic!("ディレクトリの読み取りに失敗 {:?}: {}", src, e))
        .flatten()
    {
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let name = entry.file_name();
        // IDE等のローカル専用メタデータはバンドルに含めない
        if name == ".idea" {
            continue;
        }
        let dst_path = dst.join(&name);
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path);
        } else if file_type.is_file() {
            fs::copy(entry.path(), &dst_path).unwrap_or_else(|e| {
                panic!(
                    "ファイルのコピーに失敗 {:?} -> {:?}: {}",
                    entry.path(),
                    dst_path,
                    e
                )
            });
        }
    }
}
