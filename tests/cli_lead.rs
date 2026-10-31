use std::{fs, process::Command};
#[test]
fn init_cli_writes_only_project_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    fs::create_dir(&project).unwrap();
    let home = dir.path().join("home");
    fs::create_dir(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("HOME", &home)
        .args(["--json", "init", "--project-root"])
        .arg(&project)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        value["data"]["recommendation"]
            .as_str()
            .unwrap()
            .contains("system")
    );
    assert!(project.join(".shepherd.toml").exists());
    assert!(!home.join(".config").exists());
}
