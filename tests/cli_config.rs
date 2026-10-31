use std::process::Command;

#[test]
fn bad_project_config_exits_2() {
    let global = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(".shepherd.toml");
    std::fs::write(&path, "[invalid").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--config-dir"])
        .arg(global.path())
        .args(["validate-config", "--project-root"])
        .arg(project.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "config_invalid");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap())
    );
}
