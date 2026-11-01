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
    pr_open_case(false);
}

#[test]
fn pr_open_hook_can_cancel_after_reviewer_bookkeeping() {
    pr_open_case(true);
}

fn pr_open_case(cancel_hook: bool) {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().join("repo");
    fs::create_dir(&root).unwrap();
    git(&root, &["init", "-b", "main"]);
    git(&root, &["commit", "--allow-empty", "-m", "base"]);
    if cancel_hook {
        fs::write(root.join("CODEOWNERS"), "* @fixture-reviewer\n").unwrap();
        git(&root, &["add", "CODEOWNERS"]);
        git(&root, &["commit", "-m", "owners"]);
    }
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
    if cancel_hook {
        fs::write(root.join("change.txt"), "change\n").unwrap();
        git(&root, &["add", "change.txt"]);
    }
    git(&root, &["commit", "--allow-empty", "-m", "change"]);
    let head = git(&root, &["rev-parse", "HEAD"]);
    let store = Store::open(&state, &root).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "S1".into(),
                title: "Fix boundary".into(),
                state: TicketState::ReadyForPr,
                qa_commit_sha: Some(head.clone()),
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
    let config = dir.path().join("cfg");
    let before = dir.path().join("before.json");
    let fake_herdr = dir.path().join("herdr.json");
    fs::write(&fake_herdr, "{}").unwrap();
    if cancel_hook {
        fs::create_dir(&config).unwrap();
        let command: toml::Value = vec![
            "sh".to_owned(),
            "-c".into(),
            r#"before=$1; shift; "$@" show S1 > "$before" && "$@" cancel S1"#.into(),
            "hook".into(),
            before.display().to_string(),
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
        fs::write(
            config.join("shepherd.toml"),
            format!(
                "[[hooks]]\non = 'ticket.pr_opened'\ncommand = {command}\ntimeout_seconds = 1\n"
            ),
        )
        .unwrap();
    }
    let summary = dir.path().join("summary");
    let changes = dir.path().join("changes");
    let testing = dir.path().join("testing");
    fs::write(&summary, "Check the boundary.").unwrap();
    fs::write(&changes, "Validate input").unwrap();
    fs::write(&testing, "cargo test").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("SHEPHERD_FAKE_GITHUB", &fake)
        .env("SHEPHERD_FAKE_HERDR", &fake_herdr)
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
    assert!(out.stderr.is_empty(), "{out:?}");
    assert_eq!(
        ticket.state,
        if cancel_hook {
            TicketState::Closed
        } else {
            TicketState::CiWait
        }
    );
    if cancel_hook {
        let observed: serde_json::Value =
            serde_json::from_slice(&fs::read(before).unwrap()).unwrap();
        assert_eq!(observed["data"]["state"], "ci_wait");
        assert_eq!(observed["data"]["ci_head_sha"], head);
        assert_eq!(
            observed["data"]["pr_reviewers_pending"],
            serde_json::json!([])
        );
        let returned: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(returned["data"]["state"], "closed");
    }
    assert_eq!(ticket.pr_number, Some(1));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/feat/smoke"]), head);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fake.join("github.json")).unwrap()).unwrap();
    assert_eq!(state["status"]["head_oid"], head);
    assert_eq!(state["status"]["checks"], "pending");
    if cancel_hook {
        let calls = state["calls"].as_array().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == Some("reviewers 1 [\"fixture-reviewer\"]"))
                .count(),
            1,
            "reviewer request must finish exactly once before cancellation"
        );
    }
    assert!(fake.join("github.lock").exists());
}
