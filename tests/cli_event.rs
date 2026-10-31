use std::process::Command;

fn command(config: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
    cmd.args(["--json", "--config-dir"])
        .arg(config)
        .arg("event")
        .env("HERDR_PLUGIN_STATE_DIR", config.join("state"))
        .env("HERDR_SOCKET_PATH", "/nonexistent/sock")
        .env("HERDR_BIN_PATH", "/nonexistent/herdr")
        .env_remove("HERDR_PLUGIN_EVENT")
        .env_remove("HERDR_PLUGIN_EVENT_JSON");
    cmd
}

#[test]
fn event_requires_environment() {
    let cfg = tempfile::tempdir().unwrap();
    let out = command(cfg.path()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "usage");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("HERDR_PLUGIN_EVENT")
    );
}

#[test]
fn active_event_without_herdr_exits_3() {
    let cfg = tempfile::tempdir().unwrap();
    let out = command(cfg.path())
        .env("HERDR_PLUGIN_EVENT", "worktree.created")
        .env("HERDR_PLUGIN_EVENT_JSON", "{}")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "herdr_unavailable");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/sock")
    );
}

#[test]
fn disabled_event_does_not_require_herdr() {
    let cfg = tempfile::tempdir().unwrap();
    std::fs::write(
        cfg.path().join("shepherd.toml"),
        "[upkeep]\nregroup = false\n",
    )
    .unwrap();
    let out = command(cfg.path())
        .env("HERDR_PLUGIN_EVENT", "worktree.created")
        .env("HERDR_PLUGIN_EVENT_JSON", "{}")
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"], "disabled");
}
