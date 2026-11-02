use herdr_shepherd::{
    config, flock,
    herdr::fake::FakeHerdr,
    launch::{self, LaunchRequest},
    roles,
    store::{AgentStatus, Store, Ticket, TicketState},
};
use std::{fs, path::Path};

const REPORT: &str = r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#;

fn fixture() -> (tempfile::TempDir, Store, FakeHerdr, config::Config) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/retirement-hooks");
    fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                state: TicketState::Building,
                worktree: Some(dir.path().into()),
                workspace_id: Some("w1".into()),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let mut cfg = config::load(&dir.path().join("config"), None).unwrap();
    let command: toml::Value = vec![
        "sh",
        "-c",
        "printf '%s\\n' \"$SHEPHERD_EVENT\" >> \"$1\"",
        "hook",
        dir.path().join("events").to_str().unwrap(),
    ]
    .into();
    cfg.raw = format!("[[hooks]]\non='agent.reported'\ncommand={command}\n[[hooks]]\non='agent.retired'\ncommand={command}\n").parse().unwrap();
    (dir, store, FakeHerdr::new(vec![]), cfg)
}

fn request(root: &Path, cfg: &config::Config) -> LaunchRequest {
    let resolved = roles::resolve(cfg).unwrap();
    LaunchRequest {
        ticket: "T-1".into(),
        role: resolved
            .roles
            .iter()
            .find(|r| r.name == "coder")
            .unwrap()
            .clone(),
        runners: vec![resolved.runners["codex-sol-high"].clone()],
        worktree: root.into(),
        workspace_id: "w1".into(),
        tab_label: "coder".into(),
        prompt: "task {{task_id}}".into(),
        state_dir: root.join("state"),
    }
}

#[test]
fn reports_emit_reported_while_owned_idle_process_stays_alive() {
    let (dir, store, h, cfg) = fixture();
    assert!(herdr_shepherd::hooks::known_event("agent.reported"));
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    let reported =
        flock::report_for_task(&h, &store, &cfg, &rec.name, rec.task_id.as_deref(), REPORT)
            .unwrap();
    assert_eq!(reported.status, AgentStatus::Reported);
    assert_eq!(h.agent_records.borrow().len(), 1);
    assert!(
        !h.calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("close_tab"))
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "agent.reported\n"
    );
    flock::report_for_task(&h, &store, &cfg, &rec.name, rec.task_id.as_deref(), REPORT).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "agent.reported\n"
    );
}

#[test]
fn cli_cancel_emits_retired_once_after_real_closure() {
    let (dir, store, h, cfg) = fixture();
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    flock::store_report_for_task(&store, &rec.name, rec.task_id.as_deref(), REPORT).unwrap();
    let config_dir = dir.path().join("config");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("shepherd.toml"),
        toml::to_string(&cfg.raw).unwrap(),
    )
    .unwrap();
    let fake = dir.path().join("fake.json");
    fs::write(&fake, "{}").unwrap();
    for _ in 0..2 {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("SHEPHERD_FAKE_HERDR", &fake)
            .env_remove("HERDR_PANE_ID")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["--json", "--state-dir"])
            .arg(dir.path().join("state"))
            .arg("--config-dir")
            .arg(&config_dir)
            .args(["agent", "--project-root"])
            .arg(dir.path())
            .args(["cancel", &rec.name])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
    }
    assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Retired);
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap_or_default(),
        "agent.retired\n"
    );
}

#[test]
fn confirmed_busy_predecessor_shutdown_emits_real_retirement() {
    let (dir, store, h, cfg) = fixture();
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    let pane = rec.pane_id.as_ref().unwrap();
    h.pane_sessions
        .borrow_mut()
        .insert(pane.clone(), "owned-session".into());
    store
        .update(|data| {
            let mut observed: serde_json::Value =
                serde_json::from_str(data.agents[0].observed.as_deref().unwrap()).unwrap();
            observed["session_ref"] = "owned-session".into();
            data.agents[0].observed = Some(observed.to_string());
            Ok(())
        })
        .unwrap();
    flock::store_report_for_task(&store, &rec.name, rec.task_id.as_deref(), REPORT).unwrap();
    let idle = h.agent_records.borrow()[0].clone();
    h.agent_records.borrow_mut()[0].agent_status = "working".into();
    h.agent_get_results
        .borrow_mut()
        .extend([Some(idle.clone()), Some(idle)]);
    let next = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    assert_ne!(next.name, rec.name);
    assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Retired);
    assert!(
        !h.agent_records
            .borrow()
            .iter()
            .any(|a| a.name.as_ref() == Some(&rec.name))
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap_or_default(),
        "agent.retired\n"
    );
}

#[test]
fn automatic_retirement_emits_after_owned_process_closes() {
    let (dir, store, h, cfg) = fixture();
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    let rec =
        flock::store_report_for_task(&store, &rec.name, rec.task_id.as_deref(), REPORT).unwrap();
    let mut uncertain = rec.clone();
    uncertain.set_prompt_delivery_uncertain(true);
    store
        .update(|data| {
            data.agents[0] = uncertain.clone();
            Ok(())
        })
        .unwrap();
    assert!(!flock::retire_automatically_with_config(&h, &store, &uncertain, &cfg).unwrap());
    assert!(!dir.path().join("events").exists());
    store
        .update(|data| {
            data.agents[0] = rec.clone();
            Ok(())
        })
        .unwrap();
    assert!(flock::retire_automatically_with_config(&h, &store, &rec, &cfg).unwrap());
    let retired = store.read().unwrap().agents[0].clone();
    assert_eq!(retired.status, AgentStatus::Retired);
    assert!(h.agent_records.borrow().is_empty());
    assert!(
        !h.pane_cwds
            .borrow()
            .contains_key(rec.pane_id.as_ref().unwrap())
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "agent.retired\n"
    );
    assert!(flock::retire_automatically_with_config(&h, &store, &retired, &cfg).unwrap());
    assert_eq!(
        fs::read_to_string(dir.path().join("events")).unwrap(),
        "agent.retired\n"
    );
}

#[test]
fn real_retirement_requires_the_owned_terminal_to_disappear() {
    for missing_tab in [true, false] {
        let (dir, store, h, cfg) = fixture();
        let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
        let mut rec =
            flock::store_report_for_task(&store, &rec.name, rec.task_id.as_deref(), REPORT)
                .unwrap();
        if missing_tab {
            rec.tab_id = None;
            store
                .update(|data| {
                    data.agents[0] = rec.clone();
                    Ok(())
                })
                .unwrap();
        }
        h.close_tab_effects.borrow_mut().push_back(false);
        assert!(
            !flock::retire_automatically_with_config(&h, &store, &rec, &cfg).unwrap(),
            "missing_tab={missing_tab}"
        );
        assert_eq!(
            store.read().unwrap().agents[0].status,
            AgentStatus::Reported
        );
        assert_eq!(h.agent_records.borrow().len(), 1);
        assert!(!dir.path().join("events").exists());
    }
}

#[test]
fn cancellation_keeps_resources_when_close_ack_has_no_effect() {
    let (dir, store, h, cfg) = fixture();
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    let rec =
        flock::store_report_for_task(&store, &rec.name, rec.task_id.as_deref(), REPORT).unwrap();
    h.close_tab_effects.borrow_mut().push_back(false);
    assert!(flock::cancel_with_config(&h, &store, &rec.name, false, &cfg).is_err());
    assert_eq!(
        store.read().unwrap().agents[0].status,
        AgentStatus::Reported
    );
    assert_eq!(h.agent_records.borrow().len(), 1);
    assert!(!dir.path().join("events").exists());
}

#[test]
fn policy_denial_does_not_emit_retirement_for_a_live_terminal() {
    let (dir, store, h, cfg) = fixture();
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    h.close_tab_effects.borrow_mut().push_back(false);
    herdr_shepherd::supervise::apply(
        &herdr_shepherd::supervise::ApplyContext {
            h: &h,
            store: &store,
            cfg: &cfg,
            state_dir: &dir.path().join("state"),
            now_s: 5,
        },
        &rec,
        &herdr_shepherd::supervise::Decision::Deny {
            reason: "policy denied".into(),
        },
    )
    .unwrap();
    assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Blocked);
    assert_eq!(h.agent_records.borrow().len(), 1);
    assert!(!dir.path().join("events").exists());
}

#[test]
fn explicit_cancellation_owns_the_agent_before_recipient_reads() {
    let (dir, store, h, cfg) = fixture();
    let rec = launch::launch_configured(&h, &store, request(dir.path(), &cfg), &cfg).unwrap();
    let callback_store = store.clone();
    *h.on_get.borrow_mut() = Some(Box::new(move |name| {
        assert!(
            flock::try_agent_operation(&callback_store, name)
                .unwrap()
                .is_none(),
            "cancellation must drain reuse and recovery before reading its recipient"
        );
    }));
    flock::cancel_with_config(&h, &store, &rec.name, false, &cfg).unwrap();
    assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Retired);
    assert!(
        flock::try_agent_operation(&store, &rec.name)
            .unwrap()
            .is_some()
    );
}
