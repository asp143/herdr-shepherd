//! Public CLI interleavings for ticket cancellation and launch admission.
use herdr_shepherd::store::{AgentStatus, Store, Ticket, TicketState};
use std::{
    fs,
    io::Write,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct OwnedProcess(Child);
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    root: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
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
        fs::write(dir.path().join("fake.json"), "{}").unwrap();
        let root = dir.path().to_owned();
        Self { dir, store, root }
    }
    fn git_repo() -> Self {
        let mut f = Self::new();
        f.root = f.dir.path().join("repo");
        fs::create_dir(&f.root).unwrap();
        git(&f.root, &["init", "-b", "fix/ticket"]);
        git(&f.root, &["config", "user.name", "Fixture"]);
        git(
            &f.root,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(&f.root, &["config", "commit.gpgsign", "false"]);
        git(&f.root, &["commit", "--allow-empty", "-m", "base"]);
        f.store = Store::open(&f.dir.path().join("state"), &f.root).unwrap();
        f.store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    state: TicketState::Building,
                    branch: "fix/ticket".into(),
                    worktree: Some(f.root.clone()),
                    workspace_id: Some("w1".into()),
                    ..Ticket::default()
                });
                Ok(())
            })
            .unwrap();
        f
    }
    fn command(&self, kind: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
        cmd.env("SHEPHERD_FAKE_HERDR", self.dir.path().join("fake.json"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["--json", "--state-dir"])
            .arg(self.dir.path().join("state"))
            .arg("--config-dir")
            .arg(self.dir.path().join("config"))
            .args([kind, "--project-root"])
            .arg(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }
}

#[test]
fn cancellation_drains_admitted_cli_launch() {
    let f = Fixture::new();
    let task = f.dir.path().join("task");
    assert!(
        Command::new("mkfifo")
            .arg(&task)
            .status()
            .unwrap()
            .success()
    );
    let mut launch = OwnedProcess(
        f.command("agent")
            .args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
            .arg(&task)
            .spawn()
            .unwrap(),
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let mut file = fs::OpenOptions::new().write(true).open(task).unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        file.write_all(b"Implement one task\n").unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The launcher has admitted the active ticket and is paused before reserve.
    let mut cancel = OwnedProcess(f.command("ticket").args(["cancel", "T-1"]).spawn().unwrap());
    let (done_tx, done_rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let status = cancel.0.wait().unwrap();
        done_tx.send(status).unwrap();
    });
    let early = done_rx.recv_timeout(Duration::from_millis(200)).ok();
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    let launched = launch.0.wait().unwrap();
    let cancelled = early.unwrap_or_else(|| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    waiter.join().unwrap();
    assert!(
        early.is_none(),
        "cancellation returned while an admitted launch was still pending"
    );
    assert!(launched.success());
    assert!(cancelled.success());
    let data = f.store.read().unwrap();
    assert_eq!(data.tickets[0].state, TicketState::Closed);
    assert_eq!(data.agents.len(), 1);
    assert_eq!(data.agents[0].status, AgentStatus::Retired);
}

#[test]
fn cancelled_ticket_refuses_later_cli_launch() {
    let f = Fixture::new();
    let out = f
        .command("ticket")
        .args(["cancel", "T-1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let task = f.dir.path().join("task");
    fs::write(&task, "Implement one task").unwrap();
    let out = f
        .command("agent")
        .args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
        .arg(task)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(f.store.read().unwrap().agents.is_empty());
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
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
fn integration_preserves_child_registered_before_persistence() {
    integration_registration_case(false);
}

#[test]
fn integration_cleanup_failure_preserves_new_child_and_recovery() {
    integration_registration_case(true);
}

fn integration_registration_case(existing_child: bool) {
    use herdr_shepherd::store::ChildWorktree;
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::git_repo();
    let existing_path = f.dir.path().join("original-child");
    if existing_child {
        let base = git(&f.root, &["rev-parse", "HEAD"]);
        git(
            &f.root,
            &[
                "worktree",
                "add",
                "-b",
                "fix/ticket--1",
                existing_path.to_str().unwrap(),
            ],
        );
        fs::write(existing_path.join("original.txt"), "completed child\n").unwrap();
        git(&existing_path, &["add", "original.txt"]);
        git(&existing_path, &["commit", "-m", "original child"]);
        f.store
            .update(|data| {
                data.tickets[0].children.push(ChildWorktree {
                    unit: 1,
                    branch: "fix/ticket--1".into(),
                    path: existing_path.clone(),
                    workspace_id: Some("child1".into()),
                    base_sha: base,
                    integrated: false,
                });
                Ok(())
            })
            .unwrap();
    }

    let child_path = f.dir.path().join("child");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-b",
            "fix/ticket--2",
            child_path.to_str().unwrap(),
        ],
    );
    let child = ChildWorktree {
        unit: 2,
        branch: "fix/ticket--2".into(),
        path: child_path.clone(),
        workspace_id: Some("child2".into()),
        base_sha: git(&f.root, &["rev-parse", "HEAD"]),
        integrated: false,
    };
    fs::write(child_path.join("new.txt"), "new child work\n").unwrap();
    git(&child_path, &["add", "new.txt"]);
    git(&child_path, &["commit", "-m", "new child"]);
    let bin = f.dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let ready = f.dir.path().join("ready");
    let release = f.dir.path().join("release");
    for path in [&ready, &release] {
        assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    }
    // The second HEAD read follows Git integration and immediately precedes
    // persistence. Only this subprocess sees the wrapper; all Git is local.
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
if [ "$*" = 'rev-parse --absolute-git-dir' ]; then
    /usr/bin/git "$@"
    touch "$BARRIER_ARMED"
    exit
fi
if [ "$*" = 'rev-parse HEAD' ] && [ -f "$BARRIER_ARMED" ] && [ ! -f "$BARRIER_DONE" ]; then
    if [ -f "$BARRIER_FIRST_HEAD" ]; then
        head=$(/usr/bin/git "$@") || exit
        touch "$BARRIER_DONE"
        printf ready > "$BARRIER_READY"
        read -r release < "$BARRIER_RELEASE"
        printf '%s\n' "$head"
        exit
    fi
    touch "$BARRIER_FIRST_HEAD"
fi
exec /usr/bin/git "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap();
    let mut integrate = OwnedProcess(
        f.command("ticket")
            .env("PATH", path)
            .env("BARRIER_ARMED", f.dir.path().join("armed"))
            .env("BARRIER_FIRST_HEAD", f.dir.path().join("first-head"))
            .env("BARRIER_DONE", f.dir.path().join("done"))
            .env("BARRIER_READY", &ready)
            .env("BARRIER_RELEASE", &release)
            .args(["integrate", "T-1"])
            .spawn()
            .unwrap(),
    );
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut value = String::new();
        use std::io::Read;
        fs::File::open(ready)
            .unwrap()
            .read_to_string(&mut value)
            .unwrap();
        tx.send(value).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("integration must reach persistence barrier");
    // Exercise a concurrent durable registration, independently of admission
    // serialization, so persistence must merge identities rather than replace.
    f.store
        .update(|data| {
            data.tickets[0].children.push(child.clone());
            Ok(())
        })
        .unwrap();
    let (launch_tx, launch_rx) = mpsc::channel();
    let launcher = if !existing_child {
        let task = f.dir.path().join("new-task");
        fs::write(&task, "Implement the next task\n").unwrap();
        let mut launch = OwnedProcess(
            f.command("agent")
                .args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
                .arg(task)
                .spawn()
                .unwrap(),
        );
        Some(std::thread::spawn(move || {
            launch_tx.send(launch.0.wait().unwrap()).unwrap()
        }))
    } else {
        None
    };
    let early_launch = if launcher.is_some() {
        launch_rx.recv_timeout(Duration::from_millis(200)).ok()
    } else {
        None
    };
    fs::OpenOptions::new()
        .write(true)
        .open(release)
        .unwrap()
        .write_all(b"continue\n")
        .unwrap();
    reader.join().unwrap();
    let status = integrate.0.wait().unwrap();
    // Fake Herdr retains the original child's directory, so cleanup must fail
    // safely after its commits land; the store keeps the recovery identity.
    assert_eq!(status.success(), !existing_child);
    if let Some(launcher) = launcher {
        let status =
            early_launch.unwrap_or_else(|| launch_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        launcher.join().unwrap();
        assert!(
            early_launch.is_none(),
            "launch started before integration finished persistence and cleanup"
        );
        assert!(status.success());
    }
    let data = f.store.read().unwrap();
    assert_eq!(
        data.tickets[0].children.len(),
        1 + usize::from(existing_child),
        "concurrent child registration was lost"
    );
    let new = data.tickets[0]
        .children
        .iter()
        .find(|child| child.unit == 2)
        .unwrap();
    assert!(!new.integrated);
    assert!(child_path.exists());
    assert!(
        !f.root.join("new.txt").exists(),
        "new work must not enter the previous integration"
    );
    if existing_child {
        assert!(
            data.tickets[0]
                .children
                .iter()
                .find(|child| child.unit == 1)
                .unwrap()
                .integrated
        );
        assert!(existing_path.exists());
        assert_eq!(
            fs::read_to_string(f.root.join("original.txt")).unwrap(),
            "completed child\n"
        );
    }
}

#[test]
fn integration_waits_for_admitted_spawn_and_keeps_live_work() {
    let f = Fixture::git_repo();
    let task = f.dir.path().join("task");
    assert!(
        Command::new("mkfifo")
            .arg(&task)
            .status()
            .unwrap()
            .success()
    );
    let mut launch = OwnedProcess(
        f.command("agent")
            .args(["spawn", "--ticket", "T-1", "--role", "coder", "--task-file"])
            .arg(&task)
            .spawn()
            .unwrap(),
    );
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let mut file = fs::OpenOptions::new().write(true).open(task).unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        file.write_all(b"Implement one task\n").unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut integrate = OwnedProcess(
        f.command("ticket")
            .args(["integrate", "T-1"])
            .spawn()
            .unwrap(),
    );
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || tx.send(integrate.0.wait().unwrap()).unwrap());
    let early = rx.recv_timeout(Duration::from_millis(200)).ok();
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert!(launch.0.wait().unwrap().success());
    let result = early.unwrap_or_else(|| rx.recv_timeout(Duration::from_secs(5)).unwrap());
    waiter.join().unwrap();
    assert!(
        early.is_none(),
        "integration used an admission snapshot before a pending launch"
    );
    assert!(!result.success(), "integration must refuse a live coder");
    assert_eq!(
        f.store.read().unwrap().agents[0].status,
        AgentStatus::Working
    );
    assert!(f.root.exists());
}

#[test]
fn integration_retires_only_child_tab_with_canonical_cwd() {
    let f = Fixture::git_repo();
    let base = git(&f.root, &["rev-parse", "HEAD"]);
    let child = f.dir.path().join("native-child");
    git(&f.root, &["config", "extensions.worktreeConfig", "true"]);
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-b",
            "fix/ticket--1",
            child.to_str().unwrap(),
        ],
    );
    git(
        &child,
        &[
            "config",
            "--worktree",
            "shepherd.ticketBranch",
            "fix/ticket--1",
        ],
    );
    fs::write(child.join("work.txt"), "child work\n").unwrap();
    git(&child, &["add", "work.txt"]);
    git(&child, &["commit", "-m", "child work"]);
    let alias = f.dir.path().join("child-alias");
    std::os::unix::fs::symlink(&child, &alias).unwrap();
    f.store
        .update(|data| {
            data.tickets[0]
                .children
                .push(herdr_shepherd::store::ChildWorktree {
                    unit: 1,
                    branch: "fix/ticket--1".into(),
                    path: child.clone(),
                    workspace_id: None,
                    base_sha: base.clone(),
                    integrated: false,
                });
            for (name, pane, worktree) in
                [("child", "w1:p1", &alias), ("sibling", "w1:p2", &f.root)]
            {
                data.agents.push(herdr_shepherd::store::AgentRecord {
                    name: name.into(),
                    ticket: "T-1".into(),
                    role: "coder".into(),
                    runner: "fake".into(),
                    status: AgentStatus::Reported,
                    pane_id: Some(pane.into()),
                    tab_id: Some(pane.replace(":p", ":t")),
                    worktree: worktree.clone(),
                    task: "work".into(),
                    report: None,
                    observed: None,
                    last_progress_at: "0".into(),
                    nudges: 0,
                    restarts: 0,
                    prompt_pending: false,
                });
            }
            Ok(())
        })
        .unwrap();
    fs::write(f.dir.path().join("fake.json"), serde_json::to_vec(&serde_json::json!({"agents": [
        {"name":"child", "agent":"fake", "agent_status":"idle", "cwd":child, "pane_id":"w1:p1", "workspace_id":"w1"},
        {"name":"sibling", "agent":"fake", "agent_status":"idle", "cwd":f.root, "pane_id":"w1:p2", "workspace_id":"w1"}
    ]})).unwrap()).unwrap();
    let out = f
        .command("ticket")
        .args(["integrate", "T-1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!child.exists());
    assert_eq!(
        fs::read_to_string(f.root.join("work.txt")).unwrap(),
        "child work\n"
    );
    let data = f.store.read().unwrap();
    assert!(data.tickets[0].children.is_empty());
    assert_eq!(data.agents[0].status, AgentStatus::Retired);
    assert_eq!(data.agents[1].status, AgentStatus::Reported);
    assert!(!git(&f.root, &["branch", "--list", "fix/ticket--1"]).contains("fix/ticket--1"));
}

#[test]
fn integration_retries_after_checkout_removal_and_ref_lock() {
    integration_cleanup_retry_case("ref-lock");
}

#[test]
fn integration_retries_after_native_remove_reports_failure() {
    integration_cleanup_retry_case("remove-error");
}

#[test]
fn integration_retries_after_ref_delete_reports_failure() {
    integration_cleanup_retry_case("ref-error");
}

#[test]
fn integration_keeps_receipt_if_ref_is_recreated_after_delete() {
    integration_cleanup_retry_case("recreate-ref");
}

fn integration_cleanup_retry_case(fault: &str) {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::git_repo();
    let child = f.dir.path().join("child");
    let base = git(&f.root, &["rev-parse", "HEAD"]);
    git(&f.root, &["config", "extensions.worktreeConfig", "true"]);
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-b",
            "fix/ticket--1",
            child.to_str().unwrap(),
        ],
    );
    git(
        &child,
        &[
            "config",
            "--worktree",
            "shepherd.ticketBranch",
            "fix/ticket--1",
        ],
    );
    fs::write(child.join("work"), "integrated work\n").unwrap();
    git(&child, &["add", "work"]);
    git(&child, &["commit", "-m", "unit work"]);
    f.store
        .update(|data| {
            data.tickets[0]
                .children
                .push(herdr_shepherd::store::ChildWorktree {
                    unit: 1,
                    branch: "fix/ticket--1".into(),
                    path: child.clone(),
                    workspace_id: None,
                    base_sha: base,
                    integrated: false,
                });
            Ok(())
        })
        .unwrap();
    let lock = f.root.join(".git/refs/heads/fix/ticket--1.lock");
    let mut command = f.command("ticket");
    if fault == "ref-lock" {
        fs::write(&lock, "locked").unwrap();
    } else {
        let bin = f.dir.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let wrapper = bin.join("git");
        fs::write(
            &wrapper,
            r#"#!/bin/sh
case "$1 $2" in
  'worktree remove')
    if [ "$CLEANUP_FAULT" = remove-error ]; then
      /usr/bin/git "$@" || exit
      exit 1
    fi
    ;;
  'update-ref -d')
    /usr/bin/git "$@" || exit
    if [ "$CLEANUP_FAULT" = recreate-ref ]; then
      /usr/bin/git update-ref "$3" "$4" || exit
      exit 0
    fi
    exit 1
    ;;
esac
exec /usr/bin/git "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();
        command
            .env(
                "PATH",
                std::env::join_paths([bin, "/usr/bin".into(), "/bin".into()]).unwrap(),
            )
            .env("CLEANUP_FAULT", fault);
    }
    let first = command.args(["integrate", "T-1"]).output().unwrap();
    assert!(!first.status.success(), "{fault}");
    assert!(!child.exists(), "reproduce failure after checkout removal");
    assert_eq!(f.store.read().unwrap().tickets[0].child_cleanup.len(), 1);
    if fault == "ref-lock" {
        fs::remove_file(lock).unwrap();
    }
    let retry = f
        .command("ticket")
        .args(["integrate", "T-1"])
        .output()
        .unwrap();
    assert!(
        retry.status.success(),
        "retry: {} {}",
        String::from_utf8_lossy(&retry.stdout),
        String::from_utf8_lossy(&retry.stderr)
    );
    let ticket = f.store.read().unwrap().tickets.remove(0);
    assert!(ticket.children.is_empty());
    assert!(ticket.child_cleanup.is_empty());
    assert!(git(&f.root, &["branch", "--list", "fix/ticket--1"]).is_empty());
    assert_eq!(
        fs::read_to_string(f.root.join("work")).unwrap(),
        "integrated work\n"
    );
}
