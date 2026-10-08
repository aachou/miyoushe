use std::process::Command;

fn run(args: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_miyoushe"))
        .args(args)
        .output()
        .expect("运行二进制失败");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn help_lists_all_options() {
    let (stdout, stderr, code) = run(&["--help"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    for opt in [
        "--min-size",
        "--concurrency",
        "--overwrite",
        "--limit",
        "-o",
    ] {
        assert!(stdout.contains(opt), "帮助中缺少 {opt}:\n{stdout}");
    }
}

#[test]
fn rejects_unsupported_size_unit() {
    let (_, stderr, code) = run(&["76438443", "--min-size", "500XYZ"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("大小单位"), "stderr: {stderr}");
}

#[test]
fn rejects_target_without_user_id() {
    let (_, stderr, code) = run(&["https://www.miyoushe.com/sr/accountCenter/postList?foo=1"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("无法从"), "stderr: {stderr}");
}

#[test]
fn rejects_missing_target() {
    let (_, stderr, code) = run(&[]);
    assert_ne!(code, 0);
    assert!(!stderr.is_empty(), "应给出用法错误");
}

#[test]
fn version_flag_prints_version() {
    let (stdout, _, code) = run(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")), "{stdout}");
}
