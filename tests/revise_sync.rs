use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use herdr_shepherd::{
    gate::{BranchPolicy, Gate},
    store::{Store, Ticket, TicketState},
    ticket::revise::{prepare_revision, resume_revision},
};

fn local_transport_enabled(test: &str) -> bool {
    if std::env::var("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN").as_deref() == Ok("allow-local-in-tmp") {
        return true;
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "isolated local-transport test failed: {test}"
    );
    false
}

struct Repo {
    dir: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    user: PathBuf,
}

fn git(root: &Path, args: &[&str]) -> String {
    let result = Command::new("git")
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
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let remote = dir.path().join("remote.git");
        let user = dir.path().join("user");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--initial-branch", "fix/ticket"]);
        git(&root, &["commit", "--allow-empty", "-m", "initial"]);
        git(&root, &["init", "--bare", remote.to_str().unwrap()]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&root, &["push", "--set-upstream", "origin", "fix/ticket"]);
        git(
            &root,
            &[
                "clone",
                "--branch",
                "fix/ticket",
                remote.to_str().unwrap(),
                user.to_str().unwrap(),
            ],
        );
        let repo = Self {
            dir,
            root,
            remote,
            user,
        };
        let store = Store::open(&repo.gate().state_dir, &repo.root).unwrap();
        store
            .update(|data| {
                data.trust = Some(herdr_shepherd::store::ProjectTrust {
                    project_root: repo.root.clone(),
                    common_git_dir: repo.root.join(".git"),
                    origin_url: repo.remote.to_string_lossy().into(),
                    push_url: repo.remote.to_string_lossy().into(),
                    default_branch: "main".into(),
                    config_commit: None,
                    config_blob: None,
                    config_text: None,
                    trusted_at: "fixture".into(),
                    global_config: Some("/dev/null".into()),
                    object_format: "sha1".into(),
                });
                data.tickets.push(repo.ticket());
                Ok(())
            })
            .unwrap();
        repo
    }

    fn gate(&self) -> Gate {
        Gate {
            policy: BranchPolicy::new(&[], Some("main"), Some("main")).unwrap(),
            ticket_branch: "fix/ticket".into(),
            worktree: self.root.clone(),
            state_dir: self.dir.path().join("state"),
            config_dir: self.dir.path().join("config"),
        }
    }

    fn ticket(&self) -> Ticket {
        Ticket {
            id: "T-1".into(),
            branch: "fix/ticket".into(),
            base: "main".into(),
            state: TicketState::Revising,
            worktree: Some(self.root.clone()),
            workspace_id: Some("w1".into()),
            pr_number: Some(7),
            ..Ticket::default()
        }
    }

    fn user_commit(&self) -> String {
        fs::write(self.user.join("user.txt"), "human change\n").unwrap();
        git(&self.user, &["add", "user.txt"]);
        git(&self.user, &["commit", "-m", "human change"]);
        git(&self.user, &["push", "origin", "fix/ticket"]);
        git(&self.user, &["rev-parse", "HEAD"])
    }
}

#[test]
fn user_commit_fast_forwarded() {
    if !local_transport_enabled("user_commit_fast_forwarded") {
        return;
    }
    let repo = Repo::new();
    let expected = repo.user_commit();
    prepare_revision(&repo.ticket(), &repo.gate()).unwrap();
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), expected);
    assert_eq!(
        fs::read_to_string(repo.root.join("user.txt")).unwrap(),
        "human change\n"
    );
}

#[test]
fn revision_sync_ignores_agent_git_config() {
    if !local_transport_enabled("revision_sync_ignores_agent_git_config") {
        return;
    }
    let repo = Repo::new();
    let marker = repo.dir.path().join("agent-config-executed");
    let evil = repo.dir.path().join("agent-helper");
    fs::write(
        &evil,
        format!("#!/bin/sh\nprintf executed > '{}'\ncat\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&evil, fs::Permissions::from_mode(0o700)).unwrap();
    let hooks = repo.dir.path().join("agent-hooks");
    fs::create_dir(&hooks).unwrap();
    fs::copy(&evil, hooks.join("post-merge")).unwrap();
    fs::write(repo.user.join(".gitattributes"), "*.txt filter=agent\n").unwrap();
    fs::write(repo.user.join("user.txt"), "human change\n").unwrap();
    git(&repo.user, &["add", ".gitattributes", "user.txt"]);
    git(
        &repo.user,
        &["commit", "-m", "human change with attributes"],
    );
    git(&repo.user, &["push", "origin", "fix/ticket"]);
    let expected = git(&repo.user, &["rev-parse", "HEAD"]);
    for (key, value) in [
        ("remote.origin.uploadpack", evil.to_str().unwrap()),
        ("core.hooksPath", hooks.to_str().unwrap()),
        ("core.fsmonitor", evil.to_str().unwrap()),
        ("filter.agent.smudge", evil.to_str().unwrap()),
        ("filter.agent.clean", evil.to_str().unwrap()),
        ("remote.origin.url", "ssh://agent.invalid/redirect"),
    ] {
        git(&repo.root, &["config", key, value]);
    }
    git(
        &repo.root,
        &[
            "config",
            "url.ssh://agent.invalid/rewrite.insteadOf",
            repo.remote.to_str().unwrap(),
        ],
    );
    let invalid = repo.dir.path().join("untrusted.gitconfig");
    fs::write(&invalid, "not valid git config\n").unwrap();
    git(
        &repo.root,
        &["config", "include.path", invalid.to_str().unwrap()],
    );
    prepare_revision(&repo.ticket(), &repo.gate()).unwrap();
    assert!(
        !marker.exists(),
        "agent-controlled Git configuration executed"
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(".git/refs/heads/fix/ticket"))
            .unwrap()
            .trim(),
        expected
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("user.txt")).unwrap(),
        "human change\n"
    );
}

#[test]
fn revision_sync_preserves_linked_and_split_indexes() {
    if !local_transport_enabled("revision_sync_preserves_linked_and_split_indexes") {
        return;
    }
    let repo = Repo::new();
    let before = git(&repo.root, &["rev-parse", "HEAD"]);
    git(&repo.root, &["update-ref", "refs/heads/main", &before]);
    git(&repo.root, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let linked = repo.dir.path().join("ticket-worktree");
    git(
        &repo.root,
        &["worktree", "add", linked.to_str().unwrap(), "fix/ticket"],
    );
    git(&linked, &["update-index", "--split-index"]);
    git(&repo.root, &["pack-refs", "--all"]);
    let mut ticket = repo.ticket();
    ticket.worktree = Some(linked.clone());
    Store::open(&repo.gate().state_dir, &repo.root)
        .unwrap()
        .update(|data| {
            data.tickets[0] = ticket.clone();
            Ok(())
        })
        .unwrap();
    let mut gate = repo.gate();
    gate.worktree = linked.clone();
    let expected = repo.user_commit();
    prepare_revision(&ticket, &gate).unwrap();
    assert_eq!(git(&linked, &["rev-parse", "HEAD"]), expected);
    assert!(git(&linked, &["status", "--porcelain"]).is_empty());
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), before);
    assert_eq!(
        fs::read_to_string(linked.join("user.txt")).unwrap(),
        "human change\n"
    );
    let transport =
        herdr_shepherd::gate::transport::PushTransport::open(&gate.state_dir, &linked).unwrap();
    assert_eq!(
        transport.verified_ticket_head("fix/ticket").unwrap(),
        expected
    );
    assert!(transport.worktree_status("fix/ticket").unwrap().is_empty());
}

#[test]
fn diverged_remote_blocks_conflict() {
    if !local_transport_enabled("diverged_remote_blocks_conflict") {
        return;
    }
    let repo = Repo::new();
    repo.user_commit();
    git(
        &repo.root,
        &["commit", "--allow-empty", "-m", "local change"],
    );
    let before = git(&repo.root, &["rev-parse", "HEAD"]);
    let store = Store::open(&repo.gate().state_dir, &repo.root).unwrap();
    store
        .update(|data| {
            let mut ticket = repo.ticket();
            ticket.revision_task = Some("Fix divergence".into());
            data.tickets[0] = ticket;
            Ok(())
        })
        .unwrap();
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    let error = resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap_err();
    assert_eq!(error.code(), "conflict");
    let ticket = store.read().unwrap().tickets.remove(0);
    assert_eq!(ticket.state, TicketState::Blocked);
    assert_eq!(
        ticket.blocked_reason,
        Some(herdr_shepherd::store::BlockedReason::Conflict)
    );
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), before);
    assert!(h.calls.borrow().is_empty());
    assert_eq!(gh.pushes.get(), 0);
}

fn stored_revision(repo: &Repo) -> Store {
    let store = Store::open(&repo.gate().state_dir, &repo.root).unwrap();
    let mut ticket = repo.ticket();
    ticket.revision_task = Some("Fix the reviewed edge case".into());
    store
        .update(|data| {
            data.tickets[0] = ticket;
            Ok(())
        })
        .unwrap();
    store
}

#[test]
fn revision_coder_prompt_keeps_writing_rules() {
    if !local_transport_enabled("revision_coder_prompt_keeps_writing_rules") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    let herdr_shepherd::ticket::revise::RevisionOutcome::Started { agent, role } =
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap()
    else {
        panic!("expected revision coder");
    };
    assert_eq!(role, "coder");
    let data = store.read().unwrap();
    let task = &data
        .agents
        .iter()
        .find(|record| record.name == agent)
        .unwrap()
        .task;
    for rule in [
        "test-first",
        "Use conventional commits",
        "type(scope)",
        "imperative subject under 50 characters",
        "body explaining why",
        "Do not add AI attribution or Co-Authored-By trailers",
        "report concrete evidence",
        "delegate code fixes back through the shepherd",
        "Never commit report files",
    ] {
        assert!(task.contains(rule), "missing revision writing rule: {rule}");
    }
    assert!(task.contains("Fix the reviewed edge case"));
    assert!(task.contains("New diff:"));
    assert!(task.contains("Revision "));
}

#[test]
fn revision_runs_coder_reviewer_then_qa() {
    if !local_transport_enabled("revision_runs_coder_reviewer_then_qa") {
        return;
    }
    use herdr_shepherd::ticket::revise::RevisionOutcome;
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    for role in ["coder", "reviewer", "qa"] {
        let result = resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap();
        let RevisionOutcome::Started {
            agent,
            role: actual,
        } = result
        else {
            panic!("expected {role}, got {result:?}")
        };
        assert_eq!(actual, role);
        assert_eq!(
            resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap(),
            RevisionOutcome::Waiting
        );
        let task = store
            .read()
            .unwrap()
            .agents
            .into_iter()
            .find(|record| record.name == agent)
            .unwrap()
            .task;
        if role == "coder" {
            assert!(task.contains("Fix the reviewed edge case"));
        }
        let sha = git(&repo.root, &["rev-parse", "HEAD"]);
        herdr_shepherd::flock::store_report(&store, &agent, &serde_json::json!({"status":"completed", "summary":"done", "tests":["fixture checks"], "changed_paths":[], "blockers":[], "findings":[], "commit_sha":sha}).to_string()).unwrap();
        assert_eq!(
            store.read().unwrap().tickets[0].state,
            TicketState::Revising
        );
    }
    assert_eq!(gh.pushes.get(), 0);
}

fn complete_revision_agents(
    repo: &Repo,
    store: &Store,
    h: &herdr_shepherd::herdr::fake::FakeHerdr,
    gh: &PushBackend,
) {
    use herdr_shepherd::ticket::revise::RevisionOutcome;
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    for stage in 0..3 {
        let RevisionOutcome::Started { agent, .. } =
            herdr_shepherd::ticket::scheduler::resume_revising(
                h,
                gh,
                store,
                &cfg,
                &repo.gate(),
                "T-1",
                "now",
            )
            .unwrap()
        else {
            panic!("expected agent")
        };
        if stage == 0 {
            git(
                &repo.root,
                &["commit", "--allow-empty", "-m", "revision fix"],
            );
        }
        let sha = git(&repo.root, &["rev-parse", "HEAD"]);
        herdr_shepherd::flock::store_report(store, &agent, &serde_json::json!({"status":"completed", "summary":"done", "tests":["fixture checks"], "changed_paths":[], "blockers":[], "findings":[], "commit_sha":sha}).to_string()).unwrap();
    }
}

#[test]
fn revision_pushes_and_returns_to_ci_wait() {
    if !local_transport_enabled("revision_pushes_and_returns_to_ci_wait") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    let expected = git(&repo.root, &["rev-parse", "HEAD"]);
    let main_before = git(
        &repo.root,
        &[
            "--git-dir",
            repo.remote.to_str().unwrap(),
            "rev-parse",
            "main",
        ],
    );
    git(
        &repo.root,
        &[
            "config",
            "remote.origin.push",
            "+refs/heads/fix/ticket:refs/heads/main",
        ],
    );
    git(&repo.root, &["config", "remote.origin.mirror", "true"]);
    let result = resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap();
    assert_eq!(
        result,
        herdr_shepherd::ticket::revise::RevisionOutcome::Pushed {
            sha: expected.clone()
        }
    );
    assert_eq!(store.read().unwrap().tickets[0].state, TicketState::CiWait);
    assert_eq!(
        git(
            &repo.root,
            &[
                "--git-dir",
                repo.remote.to_str().unwrap(),
                "rev-parse",
                "fix/ticket"
            ]
        ),
        expected
    );
    assert_eq!(gh.pushes.get(), 1);
    assert_eq!(
        git(
            &repo.root,
            &[
                "--git-dir",
                repo.remote.to_str().unwrap(),
                "rev-parse",
                "main"
            ]
        ),
        main_before
    );
}

#[test]
fn revision_syncs_before_coder_starts() {
    if !local_transport_enabled("revision_syncs_before_coder_starts") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let gh = PushBackend::new(&repo);
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    let expected = repo.user_commit();
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    let outcome = resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap();
    assert!(matches!(
        outcome,
        herdr_shepherd::ticket::revise::RevisionOutcome::Started { .. }
    ));
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), expected);
    assert_eq!(
        store.read().unwrap().tickets[0]
            .revision_run
            .as_ref()
            .unwrap()
            .base_sha,
        base
    );
}

#[test]
fn revision_base_includes_pulled_user_commits() {
    if !local_transport_enabled("revision_base_includes_pulled_user_commits") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let gh = PushBackend::new(&repo);
    let base = git(&repo.root, &["rev-parse", "HEAD"]);
    let synced = repo.user_commit();
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    for expected_role in ["coder", "reviewer", "qa"] {
        let herdr_shepherd::ticket::revise::RevisionOutcome::Started { agent, role } =
            resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap()
        else {
            panic!("expected revision agent");
        };
        assert_eq!(role, expected_role);
        let data = store.read().unwrap();
        assert_eq!(
            data.tickets[0].revision_run.as_ref().unwrap().base_sha,
            base
        );
        let task = &data
            .agents
            .iter()
            .find(|record| record.name == agent)
            .unwrap()
            .task;
        assert!(task.contains(&format!("New diff: {base}..HEAD")));
        assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), synced);
        herdr_shepherd::flock::store_report(
            &store,
            &agent,
            &serde_json::json!({
                "status":"completed", "summary":"reviewed human change", "tests":[],
                "changed_paths":["user.txt"], "blockers":[], "findings":[], "commit_sha":synced
            })
            .to_string(),
        )
        .unwrap();
    }
}

#[test]
fn changes_after_qa_block_push() {
    if !local_transport_enabled("changes_after_qa_block_push") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    git(
        &repo.root,
        &["commit", "--allow-empty", "-m", "post QA change"],
    );
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap(),
        herdr_shepherd::ticket::revise::RevisionOutcome::Blocked
    );
    assert_eq!(gh.pushes.get(), 0);
    assert_eq!(
        store.read().unwrap().tickets[0].blocked_reason,
        Some(herdr_shepherd::store::BlockedReason::Conflict)
    );
}

#[test]
fn remote_move_after_sync_blocks_and_notifies() {
    if !local_transport_enabled("remote_move_after_sync_blocks_and_notifies") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    let remote = repo.user_commit();
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap(),
        herdr_shepherd::ticket::revise::RevisionOutcome::Blocked
    );
    let data = store.read().unwrap();
    assert_eq!(
        data.tickets[0].blocked_reason,
        Some(herdr_shepherd::store::BlockedReason::Conflict)
    );
    assert!(
        data.pending_pings
            .iter()
            .any(|ping| ping.contains("T-1") && ping.contains("conflict"))
    );
    assert!(
        h.calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("notify ") && call.contains("T-1"))
    );
    assert_eq!(gh.pushes.get(), 0);
    assert_eq!(
        git(&repo.remote, &["rev-parse", "refs/heads/fix/ticket"]),
        remote
    );
}

#[test]
fn remote_move_during_push_blocks_and_notifies() {
    if !local_transport_enabled("remote_move_during_push_blocks_and_notifies") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    let local = git(&repo.root, &["rev-parse", "HEAD"]);
    gh.move_remote_on_push.set(true);
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap(),
        herdr_shepherd::ticket::revise::RevisionOutcome::Blocked
    );
    let data = store.read().unwrap();
    assert_eq!(
        data.tickets[0].blocked_reason,
        Some(herdr_shepherd::store::BlockedReason::Conflict)
    );
    assert!(
        h.calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("notify ") && call.contains("conflict"))
    );
    assert_eq!(git(&repo.root, &["rev-parse", "HEAD"]), local);
    assert_eq!(
        git(&repo.remote, &["rev-parse", "refs/heads/fix/ticket"]),
        git(&repo.user, &["rev-parse", "HEAD"])
    );
    assert_eq!(gh.pushes.get(), 0);
}

#[test]
fn non_fast_forward_rejection_blocks_without_retry() {
    if !local_transport_enabled("non_fast_forward_rejection_blocks_without_retry") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    *gh.push_error.borrow_mut() =
        Some("[rejected] fix/ticket -> fix/ticket (non-fast-forward)".into());
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap(),
        herdr_shepherd::ticket::revise::RevisionOutcome::Blocked
    );
    assert_eq!(
        store.read().unwrap().tickets[0].blocked_reason,
        Some(herdr_shepherd::store::BlockedReason::Conflict)
    );
    assert!(
        h.calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("notify ") && call.contains("conflict"))
    );
    assert_eq!(gh.pushes.get(), 0);
}

#[test]
fn revision_push_outage_keeps_run_retryable() {
    if !local_transport_enabled("revision_push_outage_keeps_run_retryable") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    *gh.push_error.borrow_mut() = Some("temporary connection timeout".into());
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later")
            .unwrap_err()
            .code(),
        "runtime"
    );
    let data = store.read().unwrap();
    assert_eq!(data.tickets[0].state, TicketState::Revising);
    assert_eq!(data.tickets[0].revision_round, 0);
    assert!(data.pending_pings.is_empty());
    *gh.push_error.borrow_mut() = None;
    assert!(matches!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "retry").unwrap(),
        herdr_shepherd::ticket::revise::RevisionOutcome::Pushed { .. }
    ));
}

#[test]
fn missing_sync_snapshot_blocks_push() {
    if !local_transport_enabled("missing_sync_snapshot_blocks_push") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    store
        .update(|data| {
            data.tickets[0].revision_run.as_mut().unwrap().synced_sha = None;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap(),
        herdr_shepherd::ticket::revise::RevisionOutcome::Blocked
    );
    assert_eq!(gh.pushes.get(), 0);
    assert_eq!(
        store.read().unwrap().tickets[0].blocked_reason,
        Some(herdr_shepherd::store::BlockedReason::Conflict)
    );
}

#[test]
fn merged_pr_cannot_receive_revision_push() {
    if !local_transport_enabled("merged_pr_cannot_receive_revision_push") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    gh.pr_state.set(herdr_shepherd::github::PrState::Merged);
    assert_eq!(
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later")
            .unwrap_err()
            .code(),
        "policy_denied"
    );
    assert_eq!(gh.pushes.get(), 0);
    assert_eq!(
        store.read().unwrap().tickets[0].state,
        TicketState::Revising
    );
}

#[test]
fn reviewer_feedback_reaches_next_coder() {
    if !local_transport_enabled("reviewer_feedback_reaches_next_coder") {
        return;
    }
    use herdr_shepherd::ticket::revise::RevisionOutcome;
    let repo = Repo::new();
    let store = stored_revision(&repo);
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    for role in ["coder", "reviewer"] {
        let RevisionOutcome::Started { agent, .. } =
            resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap()
        else {
            panic!("expected agent")
        };
        let findings = if role == "reviewer" {
            serde_json::json!([{ "severity":"high", "text":"missing empty input guard", "location":"parser.rs:12" }])
        } else {
            serde_json::json!([])
        };
        herdr_shepherd::flock::store_report(&store, &agent, &serde_json::json!({"status":"completed", "summary":"reviewed", "tests":[], "changed_paths":[], "blockers":[], "findings":findings, "commit_sha":git(&repo.root, &["rev-parse", "HEAD"])}).to_string()).unwrap();
    }
    let RevisionOutcome::Started { agent, role } =
        resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap()
    else {
        panic!("expected correction coder")
    };
    assert_eq!(role, "coder");
    let data = store.read().unwrap();
    assert_eq!(data.tickets[0].revision_round, 1);
    assert!(
        data.agents
            .iter()
            .find(|record| record.name == agent)
            .unwrap()
            .task
            .contains("missing empty input guard")
    );
    assert_eq!(gh.pushes.get(), 0);
}

#[test]
fn revision_loop_cap_including_overflow_blocks() {
    if !local_transport_enabled("revision_loop_cap_including_overflow_blocks") {
        return;
    }
    use herdr_shepherd::ticket::revise::RevisionOutcome;
    for limit in [1, u32::MAX] {
        let repo = Repo::new();
        let store = stored_revision(&repo);
        let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
        let gh = PushBackend::new(&repo);
        let mut cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
        cfg.raw = format!("[shepherd]\nmax_revision_rounds={limit}")
            .parse()
            .unwrap();
        store
            .update(|data| {
                data.tickets[0].revision_round = limit;
                Ok(())
            })
            .unwrap();
        for role in ["coder", "reviewer"] {
            let RevisionOutcome::Started { agent, .. } =
                resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap()
            else {
                panic!("expected agent")
            };
            let blockers = if role == "reviewer" {
                vec!["input is unchecked"]
            } else {
                vec![]
            };
            herdr_shepherd::flock::store_report(&store, &agent, &serde_json::json!({"status":"completed", "summary":"reviewed", "tests":[], "changed_paths":[], "blockers":blockers, "findings":[], "commit_sha":git(&repo.root, &["rev-parse", "HEAD"])}).to_string()).unwrap();
        }
        assert_eq!(
            resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "now").unwrap(),
            RevisionOutcome::Blocked
        );
        assert_eq!(
            store.read().unwrap().tickets[0].blocked_reason,
            Some(herdr_shepherd::store::BlockedReason::LoopCap)
        );
        assert_eq!(gh.pushes.get(), 0);
    }
}

#[test]
fn ack_prompts_queued_for_each_comment() {
    if !local_transport_enabled("ack_prompts_queued_for_each_comment") {
        return;
    }
    let repo = Repo::new();
    let store = stored_revision(&repo);
    store
        .update(|data| {
            for id in [41, 42] {
                data.tickets[0]
                    .comments
                    .push(herdr_shepherd::store::CommentRecord {
                        id,
                        body: format!("Please fix {id}"),
                        author: "reviewer".into(),
                        triaged: true,
                        verdict: Some("accepted".into()),
                        ..Default::default()
                    });
            }
            data.tickets[0]
                .comments
                .push(herdr_shepherd::store::CommentRecord {
                    id: 43,
                    body: "Already addressed".into(),
                    author: "reviewer".into(),
                    triaged: true,
                    verdict: Some("accepted".into()),
                    acknowledged: true,
                    ack_sha: Some("old".into()),
                    ..Default::default()
                });
            Ok(())
        })
        .unwrap();
    let h = herdr_shepherd::herdr::fake::FakeHerdr::new(vec![]);
    let gh = PushBackend::new(&repo);
    let cfg = herdr_shepherd::config::load(&repo.gate().config_dir, None).unwrap();
    complete_revision_agents(&repo, &store, &h, &gh);
    let expected = git(&repo.root, &["rev-parse", "HEAD"]);
    resume_revision(&h, &gh, &store, &cfg, &repo.gate(), "T-1", "later").unwrap();
    let data = store.read().unwrap();
    assert_eq!(data.pending_pings.len(), 2);
    for id in [41, 42] {
        let prompt = data
            .pending_pings
            .iter()
            .find(|prompt| prompt.contains(&format!("--comment {id}")))
            .unwrap();
        assert!(prompt.contains("ticket ack T-1"));
        assert!(prompt.contains(&expected));
        assert_eq!(
            data.tickets[0]
                .comments
                .iter()
                .find(|comment| comment.id == id)
                .unwrap()
                .ack_sha
                .as_deref(),
            Some(expected.as_str())
        );
    }
    assert_eq!(data.tickets[0].comments[2].ack_sha.as_deref(), Some("old"));
}

struct PushBackend {
    pushes: std::cell::Cell<usize>,
    pr_state: std::cell::Cell<herdr_shepherd::github::PrState>,
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    bin: PathBuf,
    user: PathBuf,
    move_remote_on_push: std::cell::Cell<bool>,
    push_error: std::cell::RefCell<Option<String>>,
}
impl PushBackend {
    fn new(repo: &Repo) -> Self {
        let bin = repo.dir.path().join("fake-bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(
            bin.join("gh"),
            "#!/bin/sh\necho 'no pull requests found for branch fix/ticket' >&2\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(bin.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
        git(&repo.root, &["push", "origin", "HEAD:main"]);
        Self {
            pushes: std::cell::Cell::new(0),
            pr_state: std::cell::Cell::new(herdr_shepherd::github::PrState::Open),
            root: repo.root.clone(),
            config: repo.gate().config_dir,
            state: repo.gate().state_dir,
            bin,
            user: repo.user.clone(),
            move_remote_on_push: std::cell::Cell::new(false),
            push_error: std::cell::RefCell::new(None),
        }
    }
}
impl herdr_shepherd::github::PrBackend for PushBackend {
    fn view(
        &self,
        _worktree: &Path,
        _branch: &str,
    ) -> Result<Option<herdr_shepherd::github::PrStatus>, herdr_shepherd::error::ShepherdError>
    {
        Ok(Some(herdr_shepherd::github::PrStatus {
            number: 7,
            url: "https://example.invalid/pr/7".into(),
            state: self.pr_state.get(),
            merged_at: None,
            base: "main".into(),
            head: "fix/ticket".into(),
            head_oid: None,
            checks: herdr_shepherd::github::Checks::Passing,
            review: herdr_shepherd::github::ReviewDecision::None,
            comment_ids: vec![],
        }))
    }
    fn push(&self, gate: &Gate) -> Result<(), herdr_shepherd::error::ShepherdError> {
        if let Some(error) = self.push_error.borrow().as_ref() {
            return Err(anyhow::anyhow!("{error}").into());
        }
        if self.move_remote_on_push.replace(false) {
            git(
                &self.user,
                &["commit", "--allow-empty", "-m", "racing user commit"],
            );
            git(&self.user, &["push", "origin", "fix/ticket"]);
        }
        let path =
            std::env::join_paths([self.bin.clone(), "/usr/bin".into(), "/bin".into()]).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("PATH", path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
            .current_dir(&self.root)
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--config-dir")
            .arg(&self.config)
            .args(["pr", "push", "--branch", &gate.ticket_branch, "--worktree"])
            .arg(&gate.worktree)
            .output()
            .unwrap();
        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "gated push: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        self.pushes.set(self.pushes.get() + 1);
        Ok(())
    }
    fn create(
        &self,
        _gate: &Gate,
        _base: &str,
        _title: &str,
        _body: &str,
    ) -> Result<herdr_shepherd::github::PrStatus, herdr_shepherd::error::ShepherdError> {
        panic!("revision must not create PR")
    }
    fn add_reviewers(
        &self,
        _gate: &Gate,
        _number: u64,
        _reviewers: &[String],
    ) -> Result<(), herdr_shepherd::error::ShepherdError> {
        panic!("unexpected reviewers")
    }
    fn reply(
        &self,
        _gate: &Gate,
        _number: u64,
        _comment_id: Option<u64>,
        _body: &str,
    ) -> Result<(), herdr_shepherd::error::ShepherdError> {
        panic!("lead handles ack")
    }
}
