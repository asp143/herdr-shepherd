use herdr_shepherd::store::AgentRecord;
use serde_json::{Value, json};

fn legacy_agent(status: &str) -> Value {
    json!({
        "name":"sh-fixture-qa-1", "ticket":"T-1", "role":"qa",
        "runner":"codex-sol-high", "status":status, "pane_id":"p1", "tab_id":"t1",
        "worktree":".", "task":"check the diff", "report":null, "observed":null,
        "last_progress_at":"0", "nudges":0, "restarts":0, "prompt_pending":false
    })
}

#[test]
fn assignment_schema_round_trips_and_legacy_records_stay_compatible() {
    for status in ["working", "reported", "retired"] {
        let legacy = legacy_agent(status);
        let agent: AgentRecord = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(&agent).unwrap(), legacy);
    }
    let mut value = legacy_agent("reported");
    value["task_id"] = json!("task-2");
    value["attempt_id"] = json!("attempt-2");
    value["unit"] = json!(3);
    value["launch_profile"] =
        json!({"name":"codex-sol-high", "harness":"codex", "args":["--model", "sol"]});
    value["completed_tasks"] = json!([{
        "task_id":"task-1", "attempt_id":"attempt-1", "task":"old check",
        "report":{"status":"completed", "summary":"done", "tests":[], "changed_paths":[], "blockers":[], "findings":[], "commit_sha":null},
        "completed_at":"10"
    }]);
    let agent: AgentRecord = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(agent).unwrap(), value);
}

fn report(task_id: Option<&str>) -> Value {
    let mut value = json!({"status":"completed", "summary":"done", "tests":["checked"],
        "changed_paths":[], "blockers":[], "findings":[{"severity":"low", "text":"finding", "location":null}],
        "commit_sha":"new-sha"});
    if let Some(task_id) = task_id {
        value["task_id"] = json!(task_id);
    }
    value
}

fn assigned_store(root: &std::path::Path, reused: bool) -> herdr_shepherd::store::Store {
    use herdr_shepherd::store::{Store, Ticket};
    let store = Store::open(&root.join("state"), root).unwrap();
    let mut value = legacy_agent("working");
    value["worktree"] = json!(root);
    value["task_id"] = json!(if reused { "task-2" } else { "task-1" });
    value["attempt_id"] = json!(if reused { "attempt-2" } else { "attempt-1" });
    value["prompt_pending"] = json!(true);
    value["observed"] = json!(json!({"prompt_delivery_uncertain":true, "prompt_claim":{
        "kind":"initial", "text":"check the diff", "task_id":value["task_id"], "attempt_id":value["attempt_id"]
    }}).to_string());
    if reused {
        value["completed_tasks"] = json!([{
            "task_id":"task-1", "attempt_id":"attempt-1", "task":"old check",
            "report":report(Some("task-1")), "completed_at":"10"
        }]);
    }
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                qa_commit_sha: Some("previous-sha".into()),
                ..Default::default()
            });
            data.agents.push(serde_json::from_value(value).unwrap());
            Ok(())
        })
        .unwrap();
    store
}

fn snapshot(store: &herdr_shepherd::store::Store) -> Value {
    serde_json::to_value(store.read().unwrap()).unwrap()
}

#[test]
fn stale_report_cannot_resolve_current_claim_or_change_ticket_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = assigned_store(dir.path(), true);
    let before = snapshot(&store);
    assert!(
        herdr_shepherd::flock::store_report(
            &store,
            "sh-fixture-qa-1",
            &report(Some("task-1")).to_string()
        )
        .is_err()
    );
    assert_eq!(snapshot(&store), before);
}

#[test]
fn tokenless_report_is_valid_only_for_the_original_assignment() {
    for reused in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = assigned_store(dir.path(), reused);
        let before = snapshot(&store);
        let result = herdr_shepherd::flock::store_report(
            &store,
            "sh-fixture-qa-1",
            &report(None).to_string(),
        );
        if reused {
            assert!(result.is_err());
            assert_eq!(snapshot(&store), before);
        } else {
            assert!(result.is_ok());
        }
    }
}

#[test]
fn exact_duplicate_appends_history_and_ticket_evidence_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = assigned_store(dir.path(), false);
    let text = report(Some("task-1")).to_string();
    herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &text).unwrap();
    let before = snapshot(&store);
    assert_eq!(
        before["agents"][0]["completed_tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        before["tickets"][0]["findings"].as_array().unwrap().len(),
        1
    );
    herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &text).unwrap();
    assert_eq!(snapshot(&store), before);
}

#[test]
fn report_cli_accepts_optional_task_flag_and_rejects_stale_file() {
    use std::process::Command;
    let dir = tempfile::tempdir().unwrap();
    let store = assigned_store(dir.path(), true);
    let fake = dir.path().join("fake.json");
    std::fs::write(&fake, "{}").unwrap();
    let file = dir
        .path()
        .join(".shepherd-report-sh-fixture-qa-1-task-1.json");
    let run = |task_id: &str| {
        Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env_remove("HERDR_PANE_ID")
            .env("SHEPHERD_FAKE_HERDR", &fake)
            .args(["--json", "--state-dir"])
            .arg(dir.path().join("state"))
            .arg("--config-dir")
            .arg(dir.path().join("cfg"))
            .args(["agent", "--project-root"])
            .arg(dir.path())
            .args([
                "report",
                "--agent",
                "sh-fixture-qa-1",
                "--task-id",
                task_id,
                "--report-file",
            ])
            .arg(&file)
            .output()
            .unwrap()
    };
    std::fs::write(&file, report(Some("task-1")).to_string()).unwrap();
    let before = snapshot(&store);
    for flag in ["task-1", "task-2"] {
        assert!(!run(flag).status.success());
        assert_eq!(snapshot(&store), before);
    }
    // Flag-only reports remain valid for clients using the original JSON schema.
    std::fs::write(&file, report(None).to_string()).unwrap();
    let out = run("task-2");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let before = snapshot(&store);
    assert!(run("task-2").status.success());
    assert_eq!(snapshot(&store), before);
}

#[test]
fn tokenless_legacy_claim_can_still_complete_and_duplicate() {
    use herdr_shepherd::store::Store;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
    let mut legacy = legacy_agent("blocked");
    legacy["observed"] = json!(
        r#"{"prompt_delivery_uncertain":true,"prompt_claim":{"kind":"initial","text":"old prompt"}}"#
    );
    store
        .update(|data| {
            data.agents.push(serde_json::from_value(legacy).unwrap());
            Ok(())
        })
        .unwrap();
    let text = report(None).to_string();
    let accepted = herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &text).unwrap();
    assert!(accepted.prompt_claim().is_none());
    assert!(!accepted.prompt_delivery_uncertain());
    assert_eq!(accepted.completed_tasks.len(), 1);
    let before = snapshot(&store);
    herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &text).unwrap();
    assert_eq!(snapshot(&store), before);
}

#[test]
fn new_assignment_appends_history_without_rewriting_prior_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = assigned_store(dir.path(), true);
    let prior = snapshot(&store)["agents"][0]["completed_tasks"][0].clone();
    let accepted = herdr_shepherd::flock::store_report(
        &store,
        "sh-fixture-qa-1",
        &report(Some("task-2")).to_string(),
    )
    .unwrap();
    assert!(accepted.prompt_claim().is_none());
    assert!(!accepted.prompt_delivery_uncertain());
    let value = snapshot(&store);
    assert_eq!(value["agents"][0]["completed_tasks"][0], prior);
    assert_eq!(
        value["agents"][0]["completed_tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // A late exact duplicate of the archived payload is rejected even after
    // the new result arrives; neither result is appended a second time.
    assert!(
        herdr_shepherd::flock::store_report(
            &store,
            "sh-fixture-qa-1",
            &report(Some("task-1")).to_string()
        )
        .is_err()
    );
    assert_eq!(snapshot(&store), value);
}

#[test]
fn leftover_stored_report_cannot_be_relabelled_as_current_task() {
    use herdr_shepherd::{flock::store_report_for_task, store::AgentStatus};
    let dir = tempfile::tempdir().unwrap();
    let store = assigned_store(dir.path(), false);
    let text = report(None).to_string();
    store_report_for_task(&store, "sh-fixture-qa-1", Some("task-1"), &text).unwrap();
    store
        .update(|data| {
            let agent = &mut data.agents[0];
            agent.task_id = Some("task-2".into());
            agent.attempt_id = Some("attempt-2".into());
            agent.status = AgentStatus::Working;
            agent.prompt_pending = true;
            // Deliberately retain the old stored report: it must never act as a
            // duplicate of the same text submitted for the current assignment.
            Ok(())
        })
        .unwrap();
    let before = snapshot(&store);
    assert!(store_report_for_task(&store, "sh-fixture-qa-1", Some("task-2"), &text).is_err());
    assert_eq!(snapshot(&store), before);
}

#[test]
fn completed_history_keeps_duplicate_reports_idempotent_after_resume() {
    use herdr_shepherd::store::AgentStatus;
    let dir = tempfile::tempdir().unwrap();
    let store = assigned_store(dir.path(), false);
    let text = report(Some("task-1")).to_string();
    herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &text).unwrap();
    let before = snapshot(&store);
    store
        .update(|data| {
            // Legacy resume can clear the current projection; accepted evidence
            // remains authoritative for this same assignment.
            data.agents[0].report = None;
            data.agents[0].status = AgentStatus::Working;
            Ok(())
        })
        .unwrap();
    herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &text).unwrap();
    assert_eq!(snapshot(&store), before);
    store
        .update(|data| {
            data.agents[0].report = None;
            data.agents[0].status = AgentStatus::Working;
            Ok(())
        })
        .unwrap();
    let mut changed = report(Some("task-1"));
    changed["summary"] = json!("different result");
    let before = snapshot(&store);
    assert!(
        herdr_shepherd::flock::store_report(&store, "sh-fixture-qa-1", &changed.to_string())
            .is_err()
    );
    assert_eq!(snapshot(&store), before);
}
