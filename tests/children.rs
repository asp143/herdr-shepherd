use herdr_shepherd::{error::ShepherdError, store::ChildWorktree, ticket::children::integrate};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn git(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap()
}
fn ok(path: &Path, args: &[&str]) -> String {
    let output = git(path, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
struct Repo {
    dir: tempfile::TempDir,
    main: PathBuf,
    ticket: PathBuf,
}
impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let ticket = dir.path().join("ticket");
        fs::create_dir(&main).unwrap();
        ok(&main, &["init", "-b", "main"]);
        ok(&main, &["config", "user.name", "Fixture"]);
        ok(&main, &["config", "user.email", "fixture@example.invalid"]);
        ok(&main, &["config", "commit.gpgsign", "false"]);
        fs::write(main.join("shared.txt"), "base\n").unwrap();
        ok(&main, &["add", "shared.txt"]);
        ok(&main, &["commit", "-m", "base"]);
        ok(
            &main,
            &[
                "worktree",
                "add",
                "-b",
                "feat/tiny",
                ticket.to_str().unwrap(),
            ],
        );
        Self { dir, main, ticket }
    }
    fn child(&self, unit: u32) -> ChildWorktree {
        let path = self.dir.path().join(format!("child-{unit}"));
        let branch = format!("feat/tiny--{unit}");
        let base_sha = ok(&self.ticket, &["rev-parse", "HEAD"]);
        ok(
            &self.main,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                path.to_str().unwrap(),
                "feat/tiny",
            ],
        );
        ChildWorktree {
            unit,
            branch,
            path,
            workspace_id: format!("w{unit}"),
            base_sha,
            integrated: false,
        }
    }
    fn commit(child: &ChildWorktree, path: &str, contents: &str, message: &str) -> String {
        fs::write(child.path.join(path), contents).unwrap();
        ok(&child.path, &["add", path]);
        ok(&child.path, &["commit", "-m", message]);
        ok(&child.path, &["rev-parse", "HEAD"])
    }
}

#[test]
fn integrates_two_children_in_order() {
    let repo = Repo::new();
    let one = repo.child(1);
    let two = repo.child(2);
    Repo::commit(&one, "one.txt", "one\n", "unit one");
    Repo::commit(&two, "two.txt", "two\n", "unit two");
    let mut children = [two, one];
    let report = integrate(&repo.ticket, &mut children).unwrap();
    assert_eq!(report.units, [1, 2]);
    assert_eq!(
        ok(&repo.ticket, &["log", "-2", "--format=%s"]),
        "unit two\nunit one"
    );
    assert_eq!(
        fs::read_to_string(repo.ticket.join("one.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        fs::read_to_string(repo.ticket.join("two.txt")).unwrap(),
        "two\n"
    );
    assert!(children.iter().all(|child| child.integrated));
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
}

#[test]
fn conflict_aborts_and_keeps_branch_clean() {
    let repo = Repo::new();
    let one = repo.child(1);
    let two = repo.child(2);
    Repo::commit(&one, "shared.txt", "coder one\n", "unit one");
    Repo::commit(&two, "shared.txt", "coder two\n", "unit two");
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let mut children = [one, two];
    let ranges = herdr_shepherd::ticket::children::conflict_ranges(&children);
    assert_eq!(ranges.len(), 2);
    assert!(ranges[0].ends_with("..feat/tiny--1"));
    assert!(ranges[1].ends_with("..feat/tiny--2"));
    let error = integrate(&repo.ticket, &mut children).unwrap_err();
    assert_eq!(error.code(), "conflict");
    assert!(matches!(error, ShepherdError::Conflict { unit: 2, .. }));
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), before);
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
    assert!(
        children
            .iter()
            .all(|child| !child.integrated && child.path.exists())
    );
    assert_eq!(
        fs::read_to_string(repo.ticket.join("shared.txt")).unwrap(),
        "base\n"
    );
    assert!(
        !git(&repo.ticket, &["rev-parse", "--verify", "CHERRY_PICK_HEAD"])
            .status
            .success()
    );
}

#[test]
fn refuses_dirty_ticket_worktree() {
    let repo = Repo::new();
    let one = repo.child(1);
    Repo::commit(&one, "one.txt", "one\n", "unit one");
    fs::write(repo.ticket.join("shared.txt"), "local edits\n").unwrap();
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let error = integrate(&repo.ticket, &mut [one]).unwrap_err();
    assert_eq!(error.code(), "usage");
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), before);
    assert_eq!(
        fs::read_to_string(repo.ticket.join("shared.txt")).unwrap(),
        "local edits\n"
    );
}

#[derive(Default)]
struct LocalHerdr {
    workspaces: std::cell::RefCell<Vec<herdr_shepherd::herdr::Workspace>>,
    remove_error: std::cell::Cell<bool>,
}
impl herdr_shepherd::herdr::Herdr for LocalHerdr {
    fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        label: &str,
        base: &str,
        path: &Path,
    ) -> Result<herdr_shepherd::herdr::Workspace, ShepherdError> {
        if ok(repo, &["rev-parse", "--absolute-git-dir"])
            != ok(
                repo,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
        {
            return Err(ShepherdError::HerdrUnavailable(
                "linked_worktree_source".into(),
            ));
        }
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        ok(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().unwrap(),
                base,
            ],
        );
        let workspace = herdr_shepherd::herdr::Workspace {
            workspace_id: "owned-child".into(),
            label: label.into(),
            worktree: Some(herdr_shepherd::herdr::WorktreeInfo {
                repo_key: "test".into(),
                repo_root: Some(repo.into()),
                checkout_path: Some(path.into()),
                is_linked_worktree: true,
            }),
        };
        self.workspaces.borrow_mut().push(workspace.clone());
        Ok(workspace)
    }
    fn workspaces(&self) -> Result<Vec<herdr_shepherd::herdr::Workspace>, ShepherdError> {
        Ok(self.workspaces.borrow().clone())
    }
    fn agents(&self) -> Result<Vec<herdr_shepherd::herdr::AgentInfo>, ShepherdError> {
        Ok(Vec::new())
    }
    fn rename_workspace(&self, _: &str, _: &str) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn move_block(&self, _: &[String], _: Option<&str>) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn report_workspace_tokens(
        &self,
        _: &str,
        _: &[(String, String)],
        _: &[String],
    ) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn report_pane_tokens(
        &self,
        _: &str,
        _: &[(String, String)],
        _: &[String],
    ) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn notify(&self, _: &str, _: &str) -> Result<(), ShepherdError> {
        Ok(())
    }
    fn remove_worktree(&self, id: &str, force: bool) -> Result<(), ShepherdError> {
        assert!(!force);
        if self.remove_error.get() {
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "temporary removal failure"
            )));
        }
        let workspace = self
            .workspaces
            .borrow()
            .iter()
            .find(|ws| ws.workspace_id == id)
            .unwrap()
            .clone();
        let tree = workspace.worktree.unwrap();
        ok(
            tree.repo_root.as_deref().unwrap(),
            &[
                "worktree",
                "remove",
                tree.checkout_path.as_ref().unwrap().to_str().unwrap(),
            ],
        );
        self.workspaces
            .borrow_mut()
            .retain(|ws| ws.workspace_id != id);
        Ok(())
    }
    fn close_workspace(&self, _: &str) -> Result<(), ShepherdError> {
        Ok(())
    }
}

#[test]
fn child_hook_blocks_ticket_branch_push() {
    let repo = Repo::new();
    let state = repo.dir.path().join("state");
    let remote = repo.dir.path().join("origin.git");
    ok(
        repo.dir.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    ok(
        &repo.main,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let ticket = herdr_shepherd::store::Ticket {
        id: "TINY".into(),
        slug: "tiny".into(),
        branch: "feat/tiny".into(),
        worktree: Some(repo.ticket.clone()),
        ..Default::default()
    };
    let child = herdr_shepherd::ticket::children::create_child(
        &LocalHerdr::default(),
        &ticket,
        2,
        &state,
        Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
    )
    .unwrap();
    assert!(child.path.starts_with(&state));
    assert_eq!(child.branch, "feat/tiny--2");
    assert_eq!(
        ok(
            &child.path,
            &["config", "--worktree", "--get", "shepherd.ticketBranch"]
        ),
        "feat/tiny--2"
    );
    let output = git(
        &child.path,
        &["push", "origin", "HEAD:refs/heads/feat/tiny"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("push blocked"));
    assert!(ok(&repo.main, &["ls-remote", "origin"]).is_empty());
}

#[test]
fn cleanup_removes_only_integrated_owned_children() {
    let repo = Repo::new();
    let state = repo.dir.path().join("state");
    let h = LocalHerdr::default();
    let ticket = herdr_shepherd::store::Ticket {
        id: "TINY".into(),
        slug: "tiny".into(),
        branch: "feat/tiny".into(),
        worktree: Some(repo.ticket.clone()),
        ..Default::default()
    };
    let child = herdr_shepherd::ticket::children::create_child(
        &h,
        &ticket,
        1,
        &state,
        Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
    )
    .unwrap();
    Repo::commit(&child, "one.txt", "one\n", "unit one");
    let mut children = [child];
    assert!(
        herdr_shepherd::ticket::children::cleanup_integrated(&h, &repo.ticket, &children).is_err()
    );
    assert!(children[0].path.exists());
    integrate(&repo.ticket, &mut children).unwrap();
    assert_eq!(
        herdr_shepherd::ticket::children::cleanup_integrated(&h, &repo.ticket, &children).unwrap(),
        [1]
    );
    assert!(!children[0].path.exists());
    assert!(
        !git(
            &repo.ticket,
            &["show-ref", "--verify", "refs/heads/feat/tiny--1"]
        )
        .status
        .success()
    );
    assert_eq!(
        fs::read_to_string(repo.ticket.join("one.txt")).unwrap(),
        "one\n"
    );
}

#[test]
fn refuses_child_from_another_repository() {
    let repo = Repo::new();
    let mut child = repo.child(1);
    Repo::commit(&child, "one.txt", "one\n", "unit one");
    let unrelated = Repo::new();
    child.path = unrelated.ticket.clone();
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let error = integrate(&repo.ticket, &mut [child]).unwrap_err();
    assert_eq!(error.code(), "policy_denied");
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), before);
}

#[test]
fn integrates_empty_child_without_changing_head() {
    let repo = Repo::new();
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let mut children = [repo.child(1)];
    let report = integrate(&repo.ticket, &mut children).unwrap();
    assert_eq!(report.units, [1]);
    assert_eq!(report.head, before);
    assert!(children[0].integrated);
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
}

#[test]
fn hook_install_failure_removes_owned_empty_child() {
    let repo = Repo::new();
    let state = repo.dir.path().join("state");
    let h = LocalHerdr::default();
    let ticket = herdr_shepherd::store::Ticket {
        id: "TINY".into(),
        slug: "tiny".into(),
        branch: "feat/tiny".into(),
        worktree: Some(repo.ticket.clone()),
        ..Default::default()
    };
    assert!(
        herdr_shepherd::ticket::children::create_child(
            &h,
            &ticket,
            1,
            &state,
            &repo.dir.path().join("missing-shepherd")
        )
        .is_err()
    );
    assert!(h.workspaces.borrow().is_empty());
    assert!(
        !git(
            &repo.ticket,
            &["show-ref", "--verify", "refs/heads/feat/tiny--1"]
        )
        .status
        .success()
    );
    assert!(
        !state
            .join("children")
            .join(herdr_shepherd::store::project_hash(&repo.ticket))
            .join("1")
            .exists()
    );
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
}

#[test]
fn hook_failure_retains_recovery_when_cleanup_fails() {
    let repo = Repo::new();
    let state = repo.dir.path().join("state");
    let h = LocalHerdr::default();
    h.remove_error.set(true);
    let ticket = herdr_shepherd::store::Ticket {
        id: "TINY".into(),
        slug: "tiny".into(),
        branch: "feat/tiny".into(),
        worktree: Some(repo.ticket.clone()),
        ..Default::default()
    };
    let error = herdr_shepherd::ticket::children::create_child(
        &h,
        &ticket,
        1,
        &state,
        &repo.dir.path().join("missing-shepherd"),
    )
    .unwrap_err();
    let recovery_path = state
        .join("children")
        .join(herdr_shepherd::store::project_hash(&repo.ticket))
        .join("1.pending.json");
    let child: ChildWorktree = serde_json::from_slice(&fs::read(&recovery_path).unwrap()).unwrap();
    assert_eq!(child.workspace_id, "owned-child");
    assert_eq!(child.branch, "feat/tiny--1");
    assert!(child.path.exists());
    assert!(!child.integrated);
    assert!(error.to_string().contains(recovery_path.to_str().unwrap()));
    assert_eq!(h.workspaces.borrow().len(), 1);
}

#[test]
fn refuses_existing_cherry_pick_without_aborting_it() {
    let repo = Repo::new();
    let child = repo.child(1);
    ok(
        &child.path,
        &["commit", "--allow-empty", "-m", "empty change"],
    );
    let tip = ok(&child.path, &["rev-parse", "HEAD"]);
    assert!(!git(&repo.ticket, &["cherry-pick", &tip]).status.success());
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
    assert_eq!(ok(&repo.ticket, &["rev-parse", "CHERRY_PICK_HEAD"]), tip);
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let error = integrate(&repo.ticket, &mut [child]).unwrap_err();
    assert_eq!(error.code(), "usage");
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), before);
    assert_eq!(ok(&repo.ticket, &["rev-parse", "CHERRY_PICK_HEAD"]), tip);
}

#[test]
fn cli_conflict_persists_both_ranges_and_blocks_ticket() {
    cli_conflict_case(false);
}

#[test]
fn cli_conflict_hook_can_cancel_the_same_ticket() {
    cli_conflict_case(true);
}

fn cli_conflict_case(cancel_hook: bool) {
    use herdr_shepherd::store::{BlockedReason, Store, Ticket, TicketState};
    let repo = Repo::new();
    let one = repo.child(1);
    let two = repo.child(2);
    Repo::commit(&one, "shared.txt", "one\n", "one");
    Repo::commit(&two, "shared.txt", "two\n", "two");
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let children = vec![one, two];
    let state = repo.dir.path().join("state");
    let store = Store::open(&state, &repo.main).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                branch: "feat/tiny".into(),
                state: TicketState::Building,
                worktree: Some(repo.ticket.clone()),
                children: children.clone(),
                ..Ticket::default()
            });
            Ok(())
        })
        .unwrap();
    let fake = repo.dir.path().join("fake.json");
    fs::write(&fake, "{}").unwrap();
    let config = repo.dir.path().join("cfg");
    if cancel_hook {
        fs::create_dir(&config).unwrap();
        let command = toml::Value::try_from(vec![
            env!("CARGO_BIN_EXE_herdr-shepherd"),
            "--state-dir",
            state.to_str().unwrap(),
            "--config-dir",
            config.to_str().unwrap(),
            "ticket",
            "--project-root",
            repo.main.to_str().unwrap(),
            "cancel",
            "T-1",
        ])
        .unwrap();
        fs::write(
            config.join("shepherd.toml"),
            format!("[[hooks]]\non = 'ticket.blocked'\ncommand = {command}\ntimeout_seconds = 1\n"),
        )
        .unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("SHEPHERD_FAKE_HERDR", fake)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["--json", "--state-dir"])
        .arg(&state)
        .args(["--config-dir"])
        .arg(config)
        .args(["ticket", "--project-root"])
        .arg(&repo.main)
        .args(["integrate", "T-1"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let data = store.read().unwrap();
    assert!(out.stderr.is_empty(), "{out:?}");
    if cancel_hook {
        assert_eq!(data.tickets[0].state, TicketState::Closed);
        assert_eq!(data.tickets[0].history[0].to, TicketState::Blocked);
        assert_eq!(
            data.tickets[0].history[0].reason,
            Some(BlockedReason::Conflict)
        );
        assert_eq!(data.tickets[0].history[1].from, TicketState::Blocked);
    } else {
        assert_eq!(data.tickets[0].state, TicketState::Blocked);
        assert_eq!(
            data.tickets[0].blocked_reason,
            Some(BlockedReason::Conflict)
        );
    }
    let serialized = serde_json::to_value(&data.tickets[0]).unwrap();
    let ranges = serialized["integration_ranges"].as_array().unwrap();
    assert_eq!(ranges.len(), 2);
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), before);
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
}

#[test]
fn integrates_already_contained_child_without_replaying_commits() {
    let repo = Repo::new();
    let child = repo.child(1);
    let tip = Repo::commit(&child, "one.txt", "one\n", "unit one");
    ok(&repo.ticket, &["merge", "--ff-only", &tip]);
    let before = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    let mut children = [child];
    let report = integrate(&repo.ticket, &mut children).unwrap();
    assert_eq!(report.units, [1]);
    assert_eq!(report.ranges.len(), 1);
    assert!(report.ranges[0].ends_with("..feat/tiny--1"));
    assert_eq!(report.head, before);
    assert_eq!(ok(&repo.ticket, &["rev-parse", "HEAD"]), before);
    assert!(children[0].integrated);
    assert_eq!(
        fs::read_to_string(repo.ticket.join("one.txt")).unwrap(),
        "one\n"
    );
    assert!(ok(&repo.ticket, &["status", "--porcelain"]).is_empty());
}

#[test]
fn child_hook_uses_explicit_config_on_first_install() {
    let repo = Repo::new();
    let state = repo.dir.path().join("state");
    let config_dir = repo.dir.path().join("custom global config");
    fs::create_dir(&config_dir).unwrap();
    fs::write(
        config_dir.join("shepherd.toml"),
        "[shepherd]\nprotected_branches = ['feat/tiny--2']\n",
    )
    .unwrap();
    let remote = repo.dir.path().join("origin.git");
    ok(
        repo.dir.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    ok(
        &repo.main,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let ticket = herdr_shepherd::store::Ticket {
        id: "TINY".into(),
        slug: "tiny".into(),
        branch: "feat/tiny".into(),
        worktree: Some(repo.ticket.clone()),
        ..Default::default()
    };
    let child = herdr_shepherd::ticket::children::create_child_configured(
        &LocalHerdr::default(),
        &ticket,
        2,
        &state,
        Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
        &config_dir,
    )
    .unwrap();
    let hook = fs::read_to_string(state.join("hooks/pre-push")).unwrap();
    assert!(hook.contains(&format!(
        "--config-dir '{}' hook pre-push",
        config_dir.display()
    )));
    let output = git(
        &child.path,
        &["push", "origin", "HEAD:refs/heads/feat/tiny--2"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("push blocked"));
    assert!(ok(&repo.main, &["ls-remote", "origin"]).is_empty());
}

#[test]
fn child_uses_primary_source_and_ticket_tip() {
    let repo = Repo::new();
    ok(
        &repo.ticket,
        &["commit", "--allow-empty", "-m", "ticket progress"],
    );
    let tip = ok(&repo.ticket, &["rev-parse", "HEAD"]);
    assert_ne!(tip, ok(&repo.main, &["rev-parse", "HEAD"]));
    let state = repo.dir.path().join("state");
    let config = repo.dir.path().join("selected-config");
    let h = LocalHerdr::default();
    let ticket = herdr_shepherd::store::Ticket {
        id: "TINY".into(),
        slug: "tiny".into(),
        branch: "feat/tiny".into(),
        worktree: Some(repo.ticket.clone()),
        ..Default::default()
    };
    let child = herdr_shepherd::ticket::children::create_child_configured(
        &h,
        &ticket,
        2,
        &state,
        Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
        &config,
    )
    .unwrap();
    assert_eq!(child.base_sha, tip);
    assert_eq!(ok(&child.path, &["rev-parse", "HEAD"]), tip);
    assert_eq!(child.branch, "feat/tiny--2");
    assert_eq!(
        h.workspaces.borrow()[0]
            .worktree
            .as_ref()
            .unwrap()
            .repo_root
            .as_ref(),
        Some(&repo.main)
    );
    assert!(
        child.path.starts_with(
            state
                .join("children")
                .join(herdr_shepherd::store::project_hash(&repo.ticket))
        )
    );
    let hooks = ok(
        &child.path,
        &["config", "--worktree", "--get", "core.hooksPath"],
    );
    let hook = fs::read_to_string(Path::new(&hooks).join("pre-push")).unwrap();
    assert!(hook.contains(config.to_str().unwrap()));
    assert!(hook.contains(state.to_str().unwrap()));
}
