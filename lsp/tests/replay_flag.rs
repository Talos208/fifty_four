//! `--replay` の分岐の結合テスト。バイナリを実際に起動して終了コードを見る。

#[cfg(not(feature = "replay"))]
#[test]
fn replay_flag_without_feature_exits_with_code_2() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fifty_four_lsp"))
        .arg("--replay")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("バイナリを起動できる");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("replay feature"),
        "stderr にビルド方法の案内が出る: {stderr}"
    );
    // 標準出力は LSP プロトコル用なので、何も書かない
    assert!(output.stdout.is_empty());
}
