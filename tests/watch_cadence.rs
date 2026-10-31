use herdr_shepherd::store::{AgentRecord, AgentStatus, Store, Ticket, TicketState};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
struct OwnedWatch {
    child: Child,
    pids: PathBuf,
}
impl Drop for OwnedWatch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(text) = fs::read_to_string(&self.pids) {
            for (index, pid) in text.split_whitespace().enumerate() {
                if let Ok(pid) = pid.parse::<i32>() {
                    if index == 0 {
                        let _ = nix::sys::signal::killpg(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    } else {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(pid),
                            nix::sys::signal::Signal::SIGKILL,
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn blocked_pr_poll_does_not_delay_supervision() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "--quiet", "--initial-branch", "main"]);
    git(
        &root,
        &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
    );
    let remote = dir.path().join("origin.git");
    git(&root, &["init", "--bare", remote.to_str().unwrap()]);
    git(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let worktree = root.join("ticket");
    fs::create_dir(&worktree).unwrap();
    let state = dir.path().join("state");
    let config = dir.path().join("config");
    fs::create_dir(&config).unwrap();
    fs::write(config.join("shepherd.toml"),"[upkeep]\nnotifications=false\nmodel_badge=false\nregroup=false\n[supervision]\nsweep_seconds=1\n").unwrap();
    let store = Store::open(&state, &root).unwrap();
    let now = herdr_shepherd::launch::now();
    let rec = AgentRecord {
        name: "sh-cadence-coder-1".into(),
        ticket: "T-1".into(),
        role: "coder".into(),
        runner: "codex-sol-high".into(),
        status: AgentStatus::Working,
        pane_id: Some("w1:p1".into()),
        tab_id: Some("w1:t1".into()),
        worktree: worktree.clone(),
        task: "Implement tiny fixture".into(),
        report: None,
        observed: None,
        last_progress_at: now.clone(),
        nudges: 0,
        restarts: 0,
        prompt_pending: false,
    };
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                branch: "feat/test".into(),
                base: "main".into(),
                state: TicketState::Building,
                worktree: Some(worktree.clone()),
                workspace_id: Some("w1".into()),
                ..Default::default()
            });
            data.agents.push(rec);
            Ok(())
        })
        .unwrap();
    let info = serde_json::json!({"name":"sh-cadence-coder-1","agent":"codex","agent_status":"blocked","cwd":worktree,"foreground_cwd":worktree,"pane_id":"w1:p1","workspace_id":"w1"});
    let fake = dir.path().join("herdr-fake");
    let keys = dir.path().join("keys");
    let pids = dir.path().join("pids");
    let script = format!(
        "#!/bin/sh\ncase \"$1 $2\" in\n'api snapshot') printf '%s %s\\n' \"$$\" \"$PPID\" > \"$FIXTURE_PR_PIDS\"; sleep 8; printf '%s\\n' '{{\"result\":{{\"snapshot\":{{\"workspaces\":[]}}}}}}';;\n'agent get') printf '%s\\n' '{}' ;;\n'agent read') printf '%s\\n' '{{\"result\":{{\"text\":\"Trust this folder? Trust and continue\"}}}}';;\n'pane get') printf '%s\\n' '{}' ;;\n'agent send-keys') : > \"$FIXTURE_KEYS\"; printf '%s\\n' '{{\"result\":{{}}}}';;\n'agent list') printf '%s\\n' '{}' ;;\n*) printf '%s\\n' '{{\"result\":{{}}}}' ;;\nesac\n",
        serde_json::json!({"result":{"agent":info}}),
        serde_json::json!({"result":{"pane":{"cwd":worktree,"foreground_cwd":worktree,"agent_session":null}}}),
        serde_json::json!({"result":{"agents":[info]}})
    );
    fs::write(&fake, script).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("herdr.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let log = dir.path().join("watch.log");
    let output = fs::File::create(&log).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir.path())
        .env("HERDR_BIN_PATH", &fake)
        .env("HERDR_SOCKET_PATH", socket)
        .env("FIXTURE_PR_PIDS", &pids)
        .env("FIXTURE_KEYS", &keys)
        .env_remove("SHEPHERD_WATCH_PR_ONLY")
        .env_remove("SHEPHERD_WATCH_DETACHED")
        .args(["--state-dir"])
        .arg(&state)
        .args(["--config-dir"])
        .arg(&config)
        .args(["watch", "--interval", "180"])
        .stdout(Stdio::null())
        .stderr(output)
        .spawn()
        .unwrap();
    let mut owned = OwnedWatch { child, pids };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && (!owned.pids.exists() || !keys.exists()) {
        assert!(
            owned.child.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&log).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        owned.pids.exists(),
        "PR worker did not start: {}",
        fs::read_to_string(&log).unwrap()
    );
    assert!(
        keys.exists(),
        "blocked PR poll delayed owned-agent trust handling: {}",
        fs::read_to_string(&log).unwrap()
    );
}
