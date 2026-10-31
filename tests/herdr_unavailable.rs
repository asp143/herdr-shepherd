#[test]
fn regroup_without_herdr_exits_3() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "regroup"])
        .env("HERDR_SOCKET_PATH", "/nonexistent/sock")
        .env("HERDR_BIN_PATH", "/nonexistent/herdr")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "herdr_unavailable");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/nonexistent")
    );
}
