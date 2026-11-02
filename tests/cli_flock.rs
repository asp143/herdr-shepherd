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
    let rec = &v["data"];
    let task_id = rec["task_id"]
        .as_str()
        .expect("new launches need a task ID");
    assert!(rec["attempt_id"].as_str().is_some());
    assert_eq!(rec["launch_profile"]["name"], "codex-sol-high");
    assert_eq!(rec["launch_profile"]["harness"], "codex");
    let prompt = rec["task"].as_str().unwrap();
    assert!(prompt.contains(&format!("--task-id '{task_id}'")));
    assert!(prompt.contains(&format!(
        ".shepherd-report-{}-{task_id}.json",
        rec["name"].as_str().unwrap()
    )));
    assert!(!prompt.contains("{{task_id}}"));
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

#[test]
fn report_requires_calling_pane_identity() {
    use herdr_shepherd::store::{AgentRecord, AgentStatus, Store};
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&state, dir.path()).unwrap();
    store
        .update(|d| {
            d.agents.push(AgentRecord {
                name: "sh-fixture-coder-1".into(),
                ticket: "T-1".into(),
                role: "coder".into(),
                runner: "codex-sol-high".into(),
                status: AgentStatus::Working,
                pane_id: Some("owned-pane".into()),
                tab_id: Some("owned-tab".into()),
                worktree: dir.path().into(),
                task: "implement".into(),
                report: None,
                observed: None,
                last_progress_at: "0".into(),
                nudges: 0,
                restarts: 0,
                prompt_pending: false,
                task_id: None,
                attempt_id: None,
                unit: None,
                launch_profile: None,
                completed_tasks: vec![],
                dispatch_reason: None,
            });
            Ok(())
        })
        .unwrap();
    let report = dir.path().join("report.json");
    std::fs::write(&report,r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
    let fake = dir.path().join("fake.json");
    std::fs::write(&fake, "{}").unwrap();
    let run = |pane: &str| {
        Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("HERDR_PANE_ID", pane)
            .env("SHEPHERD_FAKE_HERDR", &fake)
            .args(["--json", "--state-dir"])
            .arg(&state)
            .args(["--config-dir"])
            .arg(dir.path().join("cfg"))
            .args(["agent", "--project-root"])
            .arg(dir.path())
            .args(["report", "--agent", "sh-fixture-coder-1", "--report-file"])
            .arg(&report)
            .output()
            .unwrap()
    };
    let out = run("unowned-pane");
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(store.read().unwrap().agents[0].report.is_none());
    let out = run("owned-pane");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(store.read().unwrap().agents[0].report.is_some());
}

#[test]
fn direct_spawn_reuses_reported_agent_with_waited_prompt() {
    use herdr_shepherd::{
        flock,
        store::{Store, Ticket, TicketState},
    };
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/reuse-cli-fixtures");
    std::fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let state = dir.path().join("state");
    let store = Store::open(&state, dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                state: TicketState::Building,
                worktree: Some(dir.path().into()),
                workspace_id: Some("w1".into()),
                branch: "feat/test".into(),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let fake = dir.path().join("fake.json");
    std::fs::write(&fake, "{}").unwrap();
    let task = dir.path().join("task.txt");
    std::fs::write(&task, "perform current task").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("SHEPHERD_FAKE_HERDR", &fake)
            .args(["--json", "--state-dir"])
            .arg(&state)
            .args(["--config-dir"])
            .arg(dir.path().join("cfg"))
            .args(["agent", "--project-root"])
            .arg(dir.path())
            .args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
            .arg(&task)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let old = store.read().unwrap().agents[0].clone();
    flock::store_report_for_task(&store, &old.name, old.task_id.as_deref(), r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
    store
        .update(|data| {
            let rec = &mut data.agents[0];
            let mut observation: serde_json::Value =
                serde_json::from_str(rec.observed.as_deref().unwrap()).unwrap();
            observation["session_ref"] = "cli-session".into();
            rec.observed = Some(observation.to_string());
            Ok(())
        })
        .unwrap();
    std::fs::write(&fake, serde_json::to_vec(&serde_json::json!({
        "agents": [{"name":old.name,"agent":"codex","agent_status":"idle","cwd":dir.path(),"pane_id":old.pane_id,"workspace_id":"w1"}],
        "pane_sessions": {old.pane_id.as_deref().unwrap(): "cli-session"}
    })).unwrap()).unwrap();
    let second = run();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stdout)
    );
    let output: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(output["data"]["name"], old.name);
    assert!(
        output["data"]["dispatch_reason"]
            .as_str()
            .unwrap()
            .starts_with("reused:")
    );
    assert_eq!(store.read().unwrap().agents.len(), 1);
    assert_ne!(output["data"]["task_id"], old.task_id.unwrap());
}
