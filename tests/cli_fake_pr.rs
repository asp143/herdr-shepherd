#![cfg(debug_assertions)]

use herdr_shepherd::store::{Store, Ticket, TicketState};
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
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
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[test]
fn fake_backend_pushes_local_branch_and_opens_pr() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-m", "base"]);
    let remote = dir.path().join("remote.git");
    git(
        &root,
        &[
            "clone",
            "--bare",
            root.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(
        &root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let state = dir.path().join("state");
    herdr_shepherd::trusted::ensure_snapshot(&state, &root).unwrap();
    git(&root, &["branch", "feat/smoke"]);
    git(&root, &["symbolic-ref", "HEAD", "refs/heads/feat/smoke"]);
    git(&root, &["commit", "--allow-empty", "-m", "change"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let store = Store::open(&state, &root).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "S1".into(),
                title: "Fix boundary".into(),
                state: TicketState::ReadyForPr,
                branch: "feat/smoke".into(),
                base: "main".into(),
                worktree: Some(root.clone()),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let fake = dir.path().join("fake");
    fs::create_dir(&fake).unwrap();
    fs::write(
        fake.join("github.json"),
        r#"{"status":null,"author":"smoke-user"}"#,
    )
    .unwrap();
    let summary = dir.path().join("summary");
    let changes = dir.path().join("changes");
    let testing = dir.path().join("testing");
    fs::write(&summary, "Check the boundary.").unwrap();
    fs::write(&changes, "Validate input").unwrap();
    fs::write(&testing, "cargo test").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("SHEPHERD_FAKE_GITHUB", &fake)
        .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
        .args(["--json", "--state-dir"])
        .arg(&state)
        .args(["--config-dir"])
        .arg(dir.path().join("cfg"))
        .args(["ticket", "--project-root"])
        .arg(&root)
        .args(["pr", "S1", "--summary-file"])
        .arg(summary)
        .arg("--changes-file")
        .arg(changes)
        .arg("--testing-file")
        .arg(testing)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let ticket = &store.read().unwrap().tickets[0];
    assert_eq!(ticket.state, TicketState::CiWait);
    assert_eq!(ticket.pr_number, Some(1));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/feat/smoke"]), head);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fake.join("github.json")).unwrap()).unwrap();
    assert_eq!(state["status"]["head_oid"], head);
    assert_eq!(state["status"]["checks"], "pending");
    assert!(fake.join("github.lock").exists());
}
