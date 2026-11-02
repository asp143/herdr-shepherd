use std::{fs, path::Path, process::Command};
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
#[test]
fn cli_hooks_use_frozen_text_after_shared_local_ref_poisoning() {
    use herdr_shepherd::store::{AgentRecord, AgentStatus, Store};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "trunk"]);
    let safe = dir.path().join("safe-marker");
    let evil = dir.path().join("evil-marker");
    let table = |marker: &Path| {
        format!(
            "[shepherd]\nprotected_branches=['protected-extra']\n[[hooks]]\non='ticket.queued'\ncommand=['/bin/sh','-c','printf fixture > \"$1\"','fixture','{}']\n",
            marker.display()
        )
    };
    fs::write(root.join(".shepherd.toml"), table(&safe)).unwrap();
    git(&root, &["add", ".shepherd.toml"]);
    git(&root, &["commit", "-m", "trusted config"]);
    let origin = dir.path().join("origin.git");
    git(&root, &["init", "--bare", origin.to_str().unwrap()]);
    git(
        &root,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&root, &["push", "origin", "trunk"]);
    let state = dir.path().join("state");
    let cfgdir = dir.path().join("cfg");
    let run = |cwd: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
            .env_remove("HERDR_SHEPHERD_PROJECT_ROOT")
            .args(["--json", "--state-dir"])
            .arg(&state)
            .args(["--config-dir"])
            .arg(&cfgdir)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(
        &root,
        &["config", "trust", "--project-root", root.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let ticket = dir.path().join("ticket");
    git(
        &root,
        &["worktree", "add", "-b", "feat/x", ticket.to_str().unwrap()],
    );
    fs::write(
        ticket.join(".shepherd.toml"),
        table(&evil).replace("protected-extra", "dropped-extra"),
    )
    .unwrap();
    git(&ticket, &["add", ".shepherd.toml"]);
    git(&ticket, &["commit", "-m", "untrusted config"]);
    let poisoned = git(&ticket, &["rev-parse", "HEAD"]);
    git(&ticket, &["update-ref", "refs/heads/trunk", &poisoned]);
    let payload = dir.path().join("event.json");
    fs::write(&payload, "{}").unwrap();
    let out = run(
        &ticket,
        &[
            "hooks",
            "emit",
            "ticket.queued",
            "--json-file",
            payload.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(safe.exists());
    assert!(
        !evil.exists(),
        "mutable local refs must not inject privileged hook commands"
    );
    let config = herdr_shepherd::trusted::committed_config(&cfgdir, &ticket, None, &state).unwrap();
    assert_eq!(
        config.raw["shepherd"]["protected_branches"][0].as_str(),
        Some("protected-extra")
    );
    let store = Store::open(&state, &root).unwrap();
    store
        .update(|d| {
            d.agents.push(AgentRecord {
                name: "sh-managed-lead".into(),
                ticket: String::new(),
                role: "lead".into(),
                runner: "codex-sol-high".into(),
                status: AgentStatus::Working,
                pane_id: Some("managed-pane".into()),
                tab_id: None,
                worktree: root.clone(),
                task: String::new(),
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
    let before = store.read().unwrap().trust.unwrap().config_blob;
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("HERDR_PANE_ID", "managed-pane")
        .args(["--json", "--state-dir"])
        .arg(&state)
        .args(["config", "trust", "--project-root"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(before, store.read().unwrap().trust.unwrap().config_blob);
}
