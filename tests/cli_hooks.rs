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
