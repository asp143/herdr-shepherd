use std::{fs, process::Command};

#[test]
fn stage_change_hook_can_cancel_the_same_ticket() {
    cancellation_hook_case("ticket.stage_changed", false);
}

#[test]
fn spawned_hook_can_cancel_the_same_ticket() {
    cancellation_hook_case("agent.spawned", true);
}

fn cancellation_hook_case(event: &str, spawn: bool) {
    use herdr_shepherd::store::{Store, Ticket, TicketState};

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let config = dir.path().join("config");
    fs::create_dir(&config).unwrap();
    let store = Store::open(&state, dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                state: TicketState::Building,
                branch: "fix/ticket".into(),
                worktree: Some(dir.path().into()),
                workspace_id: Some("w1".into()),
                ..Ticket::default()
            });
            Ok(())
        })
        .unwrap();
    let args = [
        env!("CARGO_BIN_EXE_herdr-shepherd"),
        "--json",
        "--state-dir",
        state.to_str().unwrap(),
        "--config-dir",
        config.to_str().unwrap(),
        "ticket",
        "--project-root",
        dir.path().to_str().unwrap(),
    ];
    let command: toml::Value = args
        .iter()
        .copied()
        .chain(["cancel", "T-1"])
        .collect::<Vec<_>>()
        .into();
    fs::write(
        config.join("shepherd.toml"),
        format!("[[hooks]]\non = '{event}'\ncommand = {command}\ntimeout_seconds = 1\n"),
    )
    .unwrap();
    let fake = dir.path().join("fake.json");
    fs::write(&fake, "{}").unwrap();
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..6])
        .arg(if spawn { "agent" } else { "ticket" })
        .args(&args[7..])
        .env("SHEPHERD_FAKE_HERDR", fake);
    if spawn {
        let task = dir.path().join("task");
        fs::write(&task, "Implement one task").unwrap();
        cmd.args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
            .arg(task);
    } else {
        cmd.args(["transition", "T-1", "--to", "reviewing"]);
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stderr.is_empty(), "{out:?}");
    let data = store.read().unwrap();
    let ticket = &data.tickets[0];
    assert_eq!(ticket.state, TicketState::Closed);
    if spawn {
        assert_eq!(data.agents.len(), 1);
        assert_eq!(
            data.agents[0].status,
            herdr_shepherd::store::AgentStatus::Retired
        );
        let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["data"]["status"], "retired");
    } else {
        assert_eq!(ticket.history.len(), 2);
        assert_eq!(ticket.history[0].to, TicketState::Reviewing);
        assert_eq!(ticket.history[1].from, TicketState::Reviewing);
        assert_eq!(ticket.history[1].to, TicketState::Closed);
    }
}

#[test]
fn emit_keeps_json_envelope_and_passes_payload() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("hook-output");
    let payload = dir.path().join("payload.json");
    fs::write(&payload, r#"{"pr":12}"#).unwrap();
    let text = format!(
        "[[hooks]]\non = 'ticket.cleaned_up'\ncommand = ['sh', '-c', 'cat > \"$1\"; echo hook-stdout', 'hook', '{}']\n",
        out.display()
    );
    fs::write(dir.path().join("shepherd.toml"), text).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir.path())
        .args(["--json", "--config-dir"])
        .arg(dir.path())
        .args(["hooks", "emit", "ticket.cleaned_up", "--json-file"])
        .arg(payload)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["event"], "ticket.cleaned_up");
    assert_eq!(fs::read_to_string(out).unwrap(), r#"{"pr":12}"#);
}

#[test]
fn direct_escalation_hook_can_cancel_after_durable_update() {
    supervision_cancellation_hook_case(false, "direct");
}

#[test]
fn direct_unblocked_hook_can_cancel_after_durable_update() {
    supervision_cancellation_hook_case(true, "direct");
}

#[test]
fn project_escalation_hook_can_cancel_after_outer_ownership() {
    supervision_cancellation_hook_case(false, "project");
}

#[test]
fn project_unblocked_hook_can_cancel_after_outer_ownership() {
    supervision_cancellation_hook_case(true, "project");
}

#[test]
fn sweep_escalation_hook_can_cancel_after_outer_ownership() {
    supervision_cancellation_hook_case(false, "sweep");
}

#[test]
fn sweep_unblocked_hook_can_cancel_after_outer_ownership() {
    supervision_cancellation_hook_case(true, "sweep");
}

#[test]
fn pane_escalation_hook_can_cancel_after_outer_ownership() {
    supervision_cancellation_hook_case(false, "pane");
}

#[test]
fn pane_unblocked_hook_can_cancel_after_outer_ownership() {
    supervision_cancellation_hook_case(true, "pane");
}

// Executed by the real shell hook, in a separate process, after cancellation.
#[test]
fn supervision_hook_ownership_is_released() {
    let Some(root) = std::env::var_os("SHEPHERD_TEST_HOOK_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let store = herdr_shepherd::store::Store::open(&root.join("state"), &root).unwrap();
    let _agent = herdr_shepherd::flock::try_agent_operation(&store, "sh-owned-coder-1")
        .unwrap()
        .expect("hook must run after the outer agent operation releases");
    let project_lock = store.root().join("supervise.lock");
    if project_lock.exists() {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(project_lock)
            .unwrap();
        file.try_lock()
            .expect("hook must run after the outer project operation releases");
    }
    fs::write(root.join("ownership-released"), "released").unwrap();
}

fn supervision_cancellation_hook_case(unblock: bool, entry: &str) {
    use herdr_shepherd::{
        config,
        herdr::{AgentInfo, fake::FakeHerdr},
        store::{AgentRecord, AgentStatus, Store, Ticket, TicketState},
        supervise::{self, ApplyContext, Decision},
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for args in [
        vec!["init", "--quiet", "--initial-branch", "main"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .current_dir(root)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    let state = root.join("state");
    let config = root.join("config");
    fs::create_dir(&config).unwrap();
    let store = Store::open(&state, root).unwrap();
    let rec = AgentRecord {
        name: "sh-owned-coder-1".into(),
        ticket: "T-1".into(),
        role: "coder".into(),
        runner: "codex-sol-high".into(),
        status: AgentStatus::Blocked,
        pane_id: Some("w1:p1".into()),
        tab_id: Some("w1:t1".into()),
        worktree: root.into(),
        task: "do the task".into(),
        report: None,
        observed: None,
        last_progress_at: herdr_shepherd::launch::now(),
        nudges: 0,
        restarts: 0,
        prompt_pending: true,
    };
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                state: TicketState::Building,
                branch: "fix/ticket".into(),
                worktree: Some(root.into()),
                workspace_id: Some("w1".into()),
                ..Default::default()
            });
            data.agents.push(rec.clone());
            Ok(())
        })
        .unwrap();
    let fake = root.join("fake.json");
    fs::write(&fake, "{}").unwrap();
    let payload = root.join("payload");
    let before = root.join("before.json");
    let cancelled = root.join("cancelled.json");
    let command: toml::Value = vec![
        "env".to_owned(),
        format!("SHEPHERD_FAKE_HERDR={}", fake.display()),
        format!("SHEPHERD_TEST_HOOK_ROOT={}", root.display()),
        "sh".into(),
        "-c".into(),
        r#"cat > "$1"
before=$2
cancelled=$3
probe=$4
shift 4
"$@" show T-1 > "$before" || exit 1
"$@" cancel T-1 > "$cancelled" || exit 1
"$probe" --exact supervision_hook_ownership_is_released --nocapture > /dev/null
"#
        .into(),
        "hook".into(),
        payload.display().to_string(),
        before.display().to_string(),
        cancelled.display().to_string(),
        std::env::current_exe().unwrap().display().to_string(),
        env!("CARGO_BIN_EXE_herdr-shepherd").into(),
        "--json".into(),
        "--state-dir".into(),
        state.display().to_string(),
        "--config-dir".into(),
        config.display().to_string(),
        "ticket".into(),
        "--project-root".into(),
        root.display().to_string(),
    ]
    .into();
    let event = if unblock {
        "agent.unblocked"
    } else {
        "agent.escalated"
    };
    fs::write(
        config.join("shepherd.toml"),
        format!("[supervision]\nescalate_minutes = {}\n[[hooks]]\non = '{event}'\ncommand = {command}\ntimeout_seconds = 1\n", if unblock { 15 } else { 0 }),
    )
    .unwrap();
    let cfg = config::load(&config, None).unwrap();
    let h = FakeHerdr::new(vec![]);
    h.agent_records.borrow_mut().push(AgentInfo {
        name: Some(rec.name.clone()),
        agent: "codex".into(),
        agent_status: "blocked".into(),
        cwd: Some(root.into()),
        pane_id: "w1:p1".into(),
        workspace_id: Some("w1".into()),
    });
    let screen = "Trust this folder? Trust and continue";
    h.visible_screens.borrow_mut().push_back(screen.into());
    let decision = if unblock {
        Decision::SendKeys {
            rule: "codex-trust".into(),
            keys: vec!["enter".into()],
        }
    } else {
        Decision::Escalate {
            text: "needs attention".into(),
        }
    };
    match entry {
        "direct" => supervise::apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &state,
                now_s: 1,
            },
            &rec,
            &decision,
        )
        .unwrap(),
        "project" | "sweep" => {
            let report = if entry == "project" {
                supervise::supervise_project(&h, &store, &cfg, &state, 1).unwrap()
            } else {
                supervise::sweep_projects(&h, &state, &cfg, 1)
            };
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(report.acted, 1);
        }
        "pane" => assert_eq!(
            supervise::handle_pane_event(
                &h,
                &state,
                &cfg,
                "pane.agent_status_changed",
                &serde_json::json!({"pane_id":"w1:p1"}),
            )
            .unwrap(),
            1
        ),
        _ => panic!("unknown entrypoint"),
    }

    let data = store.read().unwrap();
    assert_eq!(
        data.tickets[0].state,
        TicketState::Closed,
        "{entry} {event} hook must complete public cancellation"
    );
    assert_eq!(data.agents[0].status, AgentStatus::Retired);
    assert!(root.join("ownership-released").exists());
    let before: serde_json::Value = serde_json::from_slice(&fs::read(before).unwrap()).unwrap();
    assert_eq!(
        before["data"]["state"],
        if unblock { "building" } else { "blocked" }
    );
    assert!(
        !before["data"]["history"].as_array().unwrap().is_empty(),
        "action must be durable before the hook"
    );
    let cancelled: serde_json::Value =
        serde_json::from_slice(&fs::read(cancelled).unwrap()).unwrap();
    assert_eq!(cancelled["ok"], true);
    let payload: serde_json::Value = serde_json::from_slice(&fs::read(payload).unwrap()).unwrap();
    assert_eq!(payload["ticket"], "T-1");
    assert_eq!(payload["agent"], rec.name);
    if unblock {
        assert_eq!(payload["rule"], "codex-trust");
        assert!(
            data.agents[0]
                .observed
                .as_ref()
                .unwrap()
                .contains("answered_hash")
        );
    } else if entry == "direct" {
        assert_eq!(payload["text"], "needs attention");
    } else {
        assert_eq!(
            payload["text"],
            format!("T-1: {} still blocked: {screen}", rec.name)
        );
    }
    let inputs = || {
        h.calls
            .borrow()
            .iter()
            .filter(|call| {
                call.starts_with("send_keys")
                    || call.starts_with("prompt_agent")
                    || call.starts_with("start_agent")
            })
            .count()
    };
    let sent = inputs();
    assert_eq!(sent, usize::from(unblock));
    supervise::apply(
        &ApplyContext {
            h: &h,
            store: &store,
            cfg: &cfg,
            state_dir: &state,
            now_s: 2,
        },
        &rec,
        &decision,
    )
    .unwrap();
    supervise::supervise_project(&h, &store, &cfg, &state, 2).unwrap();
    assert_eq!(inputs(), sent, "no input may follow completed cancellation");
}
