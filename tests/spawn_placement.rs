use herdr_shepherd::store::{ChildWorktree, Store, Ticket, TicketState};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::mpsc,
    time::Duration,
};
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
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn concurrent_initial_coders_use_distinct_worktrees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-m", "base"]);
    let origin = dir.path().join("origin.git");
    git(&root, &["init", "--bare", origin.to_str().unwrap()]);
    git(
        &root,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&root, &["push", "origin", "main"]);
    let ticket = dir.path().join("ticket");
    git(
        &root,
        &["worktree", "add", "-b", "feat/t", ticket.to_str().unwrap()],
    );
    let base = git(&ticket, &["rev-parse", "HEAD"]);
    let state = dir.path().join("state");
    let store = Store::open(&state, &root).unwrap();
    let mut children = Vec::new();
    for unit in [1, 2] {
        let path = dir.path().join(format!("child-{unit}"));
        let branch = format!("feat/t--{unit}");
        git(
            &root,
            &["worktree", "add", "-b", &branch, path.to_str().unwrap()],
        );
        children.push(ChildWorktree {
            unit,
            branch,
            path,
            workspace_id: Some(format!("child-{unit}")),
            base_sha: base.clone(),
            integrated: false,
        });
    }
    store
        .update(|d| {
            d.tickets.push(Ticket {
                id: "T-1".into(),
                slug: "t".into(),
                branch: "feat/t".into(),
                base: "main".into(),
                state: TicketState::Building,
                worktree: Some(ticket.clone()),
                workspace_id: Some("parent".into()),
                children,
                ..Ticket::default()
            });
            Ok(())
        })
        .unwrap();
    let fake = dir.path().join("fake.json");
    fs::write(&fake, "{}").unwrap();
    let mut pipes = Vec::new();
    let mut processes = Vec::new();
    for unit in [1, 2] {
        let path = dir.path().join(format!("task-{unit}"));
        assert!(
            Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let child = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("SHEPHERD_FAKE_HERDR", &fake)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["--json", "--state-dir"])
            .arg(&state)
            .args(["--config-dir"])
            .arg(dir.path().join("cfg"))
            .args(["agent", "--project-root"])
            .arg(&root)
            .args([
                "spawn",
                "--ticket",
                "T-1",
                "--role",
                "coder",
                "--unit",
                &unit.to_string(),
                "--task-file",
            ])
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        processes.push(OwnedChild(child));
        pipes.push(path);
    }
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut releases = Vec::new();
    let mut writers = Vec::new();
    for (index, path) in pipes.into_iter().enumerate() {
        let ready = ready_tx.clone();
        let (tx, rx) = mpsc::channel();
        releases.push(tx);
        writers.push(std::thread::spawn(move || {
            let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
            ready.send(index).unwrap();
            rx.recv().unwrap();
            file.write_all(b"Implement one independent unit\n").unwrap();
        }));
    }
    let first = ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first caller must reach task read");
    // Old placement admits both readers before either launch is reserved.
    // Correct admission holds the second caller until the first launch lands.
    let second = ready_rx.recv_timeout(Duration::from_millis(100)).ok();
    releases[first].send(()).unwrap();
    let second = second.unwrap_or_else(|| {
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("second caller must proceed after reservation")
    });
    releases[second].send(()).unwrap();
    for writer in writers {
        writer.join().unwrap();
    }
    for child in &mut processes {
        let status = child.0.wait().unwrap();
        if !status.success() {
            let mut text = String::new();
            use std::io::Read;
            child
                .0
                .stdout
                .as_mut()
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            panic!("spawn failed: {text}");
        }
    }
    let data = store.read().unwrap();
    assert_eq!(data.agents.len(), 2);
    for agent in &data.agents {
        assert!(
            agent.pane_id.as_deref().unwrap().starts_with("parent:"),
            "every coder must open in the ticket workspace: {agent:?}"
        );
        assert!(
            data.tickets[0]
                .children
                .iter()
                .any(|c| c.path == agent.worktree),
            "an explicit unit must use its child even for the first coder"
        );
    }
    let paths: std::collections::BTreeSet<PathBuf> =
        data.agents.iter().map(|a| a.worktree.clone()).collect();
    assert_eq!(
        paths.len(),
        2,
        "parallel coders must never share a writable worktree"
    );
}

struct PlacementFixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    config: PathBuf,
    fake: PathBuf,
    task: PathBuf,
    ticket_path: PathBuf,
    store: Store,
}
impl PlacementFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["commit", "--allow-empty", "-m", "base"]);
        let ticket_path = dir.path().join("ticket");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "feat/t",
                ticket_path.to_str().unwrap(),
            ],
        );
        let state = dir.path().join("state");
        let store = Store::open(&state, &root).unwrap();
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    slug: "t".into(),
                    branch: "feat/t".into(),
                    base: "main".into(),
                    state: TicketState::Building,
                    worktree: Some(ticket_path.clone()),
                    workspace_id: Some("ticket-workspace".into()),
                    ..Ticket::default()
                });
                Ok(())
            })
            .unwrap();
        let config = dir.path().join("config");
        let fake = dir.path().join("fake.json");
        let task = dir.path().join("task.txt");
        fs::write(&fake, "{}").unwrap();
        fs::write(&task, "Implement the assigned task").unwrap();
        Self {
            _dir: dir,
            root,
            state,
            config,
            fake,
            task,
            ticket_path,
            store,
        }
    }
    fn spawn(&self, role: &str, unit: Option<u32>) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
        command
            .env("SHEPHERD_FAKE_HERDR", &self.fake)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["--json", "--state-dir"])
            .arg(&self.state)
            .arg("--config-dir")
            .arg(&self.config)
            .args(["agent", "--project-root"])
            .arg(&self.root)
            .args(["spawn", "--ticket", "T-1", "--role", role, "--task-file"])
            .arg(&self.task);
        if let Some(unit) = unit {
            command.arg("--unit").arg(unit.to_string());
        }
        command.output().unwrap()
    }
}

#[test]
fn first_explicit_unit_allocates_an_isolated_checkout_in_ticket_tab() {
    let f = PlacementFixture::new();
    let tip = git(&f.ticket_path, &["rev-parse", "HEAD"]);
    let out = f.spawn("coder", Some(1));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let data = f.store.read().unwrap();
    let child = &data.tickets[0].children[0];
    assert_eq!(child.unit, 1);
    assert_eq!(child.workspace_id, None);
    assert_eq!(child.branch, "feat/t--1");
    assert_eq!(child.base_sha, tip);
    assert_eq!(git(&child.path, &["rev-parse", "HEAD"]), tip);
    assert_eq!(git(&child.path, &["branch", "--show-current"]), "feat/t--1");
    assert_eq!(data.agents[0].worktree, child.path);
    assert!(
        data.agents[0]
            .pane_id
            .as_deref()
            .unwrap()
            .starts_with("ticket-workspace:")
    );
    let hook_path = git(
        &child.path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "hooks/pre-push",
        ],
    );
    assert!(fs::read_to_string(hook_path).unwrap().contains("shepherd"));
    assert!(data.agents[0].task.contains("feat/t--1"));
    assert_eq!(data.tickets[0].branch, "feat/t");
    assert_eq!(data.tickets[0].worktree.as_ref(), Some(&f.ticket_path));
}

#[test]
fn active_unit_cannot_be_reused_and_parallel_coder_requires_unit() {
    let f = PlacementFixture::new();
    let first = f.spawn("coder", Some(2));
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    for (unit, message) in [
        (Some(2), "child unit already has an active agent"),
        (None, "parallel coders require --unit"),
    ] {
        let out = f.spawn("coder", unit);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains(message));
    }
    let data = f.store.read().unwrap();
    assert_eq!(data.agents.len(), 1);
    assert_eq!(data.tickets[0].children.len(), 1);
}

#[test]
fn every_ticket_role_opens_in_the_ticket_workspace() {
    let f = PlacementFixture::new();
    for role in ["planner", "researcher", "coder", "reviewer", "qa"] {
        let out = f.spawn(role, None);
        assert!(
            out.status.success(),
            "{role}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let data = f.store.read().unwrap();
    assert_eq!(data.agents.len(), 5);
    assert!(data.tickets[0].children.is_empty());
    for agent in data.agents {
        assert_eq!(agent.worktree, f.ticket_path);
        assert!(
            agent
                .pane_id
                .as_deref()
                .unwrap()
                .starts_with("ticket-workspace:")
        );
    }
}

#[test]
fn revision_roles_open_ticket_tabs_without_creating_workspaces() {
    use herdr_shepherd::{
        config,
        gate::{BranchPolicy, Gate},
        github::fake::FakeBackend,
        herdr::fake::FakeHerdr,
        store::ProjectTrust,
        ticket::revise::{RevisionOutcome, RevisionRun, RevisionStage, resume_revision},
    };
    for (stage, role) in [
        (RevisionStage::Coder, "coder"),
        (RevisionStage::Reviewer, "reviewer"),
        (RevisionStage::Qa, "qa"),
    ] {
        let f = PlacementFixture::new();
        let head = git(&f.ticket_path, &["rev-parse", "HEAD"]);
        f.store
            .update(|data| {
                // The sync already completed. This fixture exercises local stage
                // dispatch without entering any network or publication path.
                data.trust = Some(ProjectTrust {
                    project_root: f.root.clone(),
                    common_git_dir: f.root.join(".git"),
                    origin_url: "https://example.invalid/repo.git".into(),
                    push_url: "https://example.invalid/repo.git".into(),
                    default_branch: "main".into(),
                    config_commit: None,
                    config_blob: None,
                    config_text: None,
                    trusted_at: "fixture".into(),
                    global_config: Some("/dev/null".into()),
                    object_format: "sha1".into(),
                });
                data.tickets[0].state = TicketState::Revising;
                data.tickets[0].revision_task = Some("Address the review".into());
                data.tickets[0].revision_run = Some(RevisionRun {
                    base_sha: head.clone(),
                    synced_sha: Some(head.clone()),
                    stage,
                    agent: None,
                    tested_sha: None,
                });
                Ok(())
            })
            .unwrap();
        let h = FakeHerdr::new(vec![]);
        let gh = FakeBackend::new(vec![]);
        let cfg = config::load(&f.config, None).unwrap();
        let gate = Gate {
            policy: BranchPolicy::new(&[], Some("main"), Some("main")).unwrap(),
            ticket_branch: "feat/t".into(),
            worktree: f.ticket_path.clone(),
            state_dir: f.state.clone(),
            config_dir: f.config.clone(),
        };
        let outcome = resume_revision(&h, &gh, &f.store, &cfg, &gate, "T-1", "1").unwrap();
        assert!(matches!(outcome, RevisionOutcome::Started { role: actual, .. } if actual == role));
        assert!(h.calls.borrow().contains(&format!(
            "create_tab ticket-workspace revision {role} {}",
            f.ticket_path.display()
        )));
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("create_worktree"))
        );
        assert!(gh.calls.borrow().is_empty());
        assert_eq!(f.store.read().unwrap().agents[0].worktree, f.ticket_path);
    }
}

fn assert_cleanup_receipt_blocks_admission(direct: bool) {
    use herdr_shepherd::{
        config,
        herdr::fake::FakeHerdr,
        launch::{LaunchRequest, launch_configured},
        roles,
        ticket::children::{cleanup_integrated_recorded, create_child_configured, integrate},
    };
    let f = PlacementFixture::new();
    git(&f.root, &["config", "user.name", "Fixture"]);
    git(
        &f.root,
        &["config", "user.email", "fixture@example.invalid"],
    );
    let h = FakeHerdr::new(vec![]);
    let mut ticket = f.store.read().unwrap().tickets.remove(0);
    let child = create_child_configured(
        &h,
        &ticket,
        1,
        &f.state,
        Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
        &f.config,
    )
    .unwrap();
    fs::write(child.path.join("work"), "child work\n").unwrap();
    git(&child.path, &["add", "work"]);
    git(&child.path, &["commit", "-m", "child work"]);
    ticket.children.push(child.clone());
    integrate(&f.ticket_path, &mut ticket.children).unwrap();
    f.store
        .update(|data| {
            data.tickets[0] = ticket.clone();
            Ok(())
        })
        .unwrap();
    git(&f.root, &["worktree", "lock", child.path.to_str().unwrap()]);
    assert!(cleanup_integrated_recorded(&h, &f.store, &ticket).is_err());
    assert_eq!(f.store.read().unwrap().tickets[0].child_cleanup.len(), 1);
    assert!(child.path.exists());
    h.calls.borrow_mut().clear();
    let child_tip = git(&child.path, &["rev-parse", "HEAD"]);
    let ticket_tip = git(&f.ticket_path, &["rev-parse", "HEAD"]);
    let message = if direct {
        let cfg = config::load(&f.config, None).unwrap();
        let resolved = roles::resolve(&cfg).unwrap();
        let role = resolved
            .roles
            .iter()
            .find(|r| r.name == "coder")
            .unwrap()
            .clone();
        let runners = role
            .runners
            .iter()
            .map(|name| resolved.runners[name].clone())
            .collect();
        launch_configured(
            &h,
            &f.store,
            LaunchRequest {
                ticket: ticket.id.clone(),
                role,
                runners,
                worktree: child.path.clone(),
                workspace_id: "ticket-workspace".into(),
                tab_label: "coder-1".into(),
                prompt: "work".into(),
                state_dir: f.state.clone(),
            },
            &cfg,
        )
        .unwrap_err()
        .to_string()
    } else {
        let out = f.spawn("coder", Some(1));
        assert!(
            !out.status.success(),
            "receipt-owned child was reused: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(message.contains("finish cleanup"), "{message}");
    assert!(f.store.read().unwrap().agents.is_empty());
    assert!(!h.calls.borrow().iter().any(|c| c.starts_with("create_tab")));
    assert_eq!(git(&child.path, &["rev-parse", "HEAD"]), child_tip);
    assert_eq!(git(&f.ticket_path, &["rev-parse", "HEAD"]), ticket_tip);
    git(
        &f.root,
        &["worktree", "unlock", child.path.to_str().unwrap()],
    );
    cleanup_integrated_recorded(&h, &f.store, &f.store.read().unwrap().tickets.remove(0)).unwrap();
    assert!(!child.path.exists());
}

#[test]
fn cleanup_receipt_blocks_cli_unit_admission() {
    assert_cleanup_receipt_blocks_admission(false);
}

#[test]
fn cleanup_receipt_blocks_direct_launch_admission() {
    assert_cleanup_receipt_blocks_admission(true);
}
