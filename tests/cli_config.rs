use std::process::Command;

#[test]
fn plugin_context_selects_invoking_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("checkout");
    std::fs::create_dir(&project).unwrap();
    let path = project.join(".shepherd.toml");
    std::fs::write(&path, "[invalid").unwrap();
    let context = serde_json::json!({
        "worktree": {"checkout_path": project, "repo_root": dir.path()},
        "workspace_cwd": dir.path(), "focused_pane_cwd": dir.path(),
    });
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .current_dir(dir.path())
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--json", "validate-config"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "config_invalid");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap())
    );
}

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

#[test]
fn missing_config_uses_claude_lead_default() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--config-dir"])
        .arg(dir.path())
        .args(["validate-config", "--project-root"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["data"]["shepherd"]["runner"], "claude-opus");
    assert_eq!(value["data"]["runners"]["claude-opus"]["harness"], "claude");
}
