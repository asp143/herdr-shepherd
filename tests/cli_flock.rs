use std::process::Command;

#[test]
fn report_missing_status_exits_usage() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    std::fs::write(&report, r#"{"summary":"done"}"#).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(dir.path().join("state"))
        .args(["agent", "report", "--project-root"])
        .arg(dir.path())
        .args(["--agent", "sh-fixture-coder-1", "--report-file"])
        .arg(report)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "usage");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid report")
    );
}

#[test]
fn fake_herdr_spawn_and_list_share_durable_state() {
    use herdr_shepherd::store::{Store, Ticket, TicketState};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&state, dir.path()).unwrap();
    store
        .update(|d| {
            d.tickets.push(Ticket {
                id: "T-1".into(),
                state: TicketState::Building,
                branch: "feat/t-1".into(),
                worktree: Some(dir.path().into()),
                workspace_id: Some("w1".into()),
                ..Ticket::default()
            });
            Ok(())
        })
        .unwrap();
    let fake = dir.path().join("fake.json");
    std::fs::write(&fake, "{}").unwrap();
    let task = dir.path().join("task.txt");
    std::fs::write(&task, "Implement tiny task").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
    cmd.env("SHEPHERD_FAKE_HERDR", fake)
        .args(["--json", "--state-dir"])
        .arg(&state)
        .args(["--config-dir"])
        .arg(dir.path().join("cfg"))
        .args(["agent", "--project-root"])
        .arg(dir.path())
        .args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
        .arg(task);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["status"], "working");
    assert_eq!(store.read().unwrap().agents.len(), 1);
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(&state)
        .args(["agent", "--project-root"])
        .arg(dir.path())
        .args(["list", "--ticket", "T-1"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
}
