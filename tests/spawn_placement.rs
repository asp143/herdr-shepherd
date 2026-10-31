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
            workspace_id: format!("child-{unit}"),
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
    let paths: std::collections::BTreeSet<PathBuf> =
        data.agents.iter().map(|a| a.worktree.clone()).collect();
    assert_eq!(
        paths.len(),
        2,
        "parallel coders must never share a writable worktree"
    );
}
