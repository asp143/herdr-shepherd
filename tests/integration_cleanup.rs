//! Integration cleanup must survive each destructive effect without weakening
//! resource ownership or the proof that all child commits were integrated.
use herdr_shepherd::{
    error::ShepherdError,
    herdr::{AgentInfo, Herdr, Workspace, WorktreeInfo},
    store::{AgentRecord, AgentStatus, ChildWorktree, Store, Ticket},
    ticket::children::{cleanup_integrated_recorded, integrate},
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Remove,
    WorkspaceClose,
    TabClose,
    TabReject,
    TabStore,
    Store,
    Record,
    Reassign,
}
struct LocalHerdr {
    root: PathBuf,
    store: Store,
    workspaces: RefCell<Vec<Workspace>>,
    agents: RefCell<Vec<AgentInfo>>,
    panes: RefCell<HashMap<String, PathBuf>>,
    fault: Cell<Fault>,
    removals: RefCell<Vec<String>>,
    closed_tabs: RefCell<Vec<String>>,
}
impl Herdr for LocalHerdr {
    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        Ok(self.workspaces.borrow().clone())
    }
    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        Ok(self.agents.borrow().clone())
    }
    fn pane_exists(&self, pane: &str) -> Result<bool, ShepherdError> {
        Ok(self.panes.borrow().contains_key(pane)
            || self.agents.borrow().iter().any(|a| a.pane_id == pane))
    }
    fn pane_cwd(&self, pane: &str) -> Result<Option<PathBuf>, ShepherdError> {
        Ok(self.panes.borrow().get(pane).cloned())
    }
    fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError> {
        assert!(!force);
        let owned = self
            .workspaces
            .borrow()
            .iter()
            .find(|w| w.workspace_id == ws)
            .unwrap()
            .clone();
        let path = owned.worktree.unwrap().checkout_path.unwrap();
        let ticket = self.store.read()?.tickets.remove(0);
        assert!(
            ticket.child_cleanup.iter().any(|r| r.path == path),
            "receipt must be durable before removal"
        );
        git(&self.root, &["worktree", "remove", path.to_str().unwrap()]);
        self.removals.borrow_mut().push(ws.into());
        if self.fault.get() == Fault::Remove {
            self.fault.set(Fault::None);
            return Err(ShepherdError::Usage(
                "injected after checkout removal".into(),
            ));
        }
        if self.fault.get() == Fault::Reassign {
            self.fault.set(Fault::None);
            self.workspaces.borrow_mut()[0]
                .worktree
                .as_mut()
                .unwrap()
                .checkout_path = Some(path.parent().unwrap().join("child-2"));
        }
        if self.fault.get() == Fault::Record {
            self.fault.set(Fault::None);
            self.store.update(|data| {
                data.tickets[0].children[0].base_sha = "tampered".into();
                Ok(())
            })?;
        }
        if self.fault.get() == Fault::Store {
            fs::create_dir(self.store.root().join("state.json.tmp")).unwrap();
            self.fault.set(Fault::None);
        }
        Ok(())
    }
    fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError> {
        if self.fault.get() == Fault::WorkspaceClose {
            self.fault.set(Fault::None);
            return Err(ShepherdError::Usage(
                "injected workspace closure failure".into(),
            ));
        }
        self.workspaces
            .borrow_mut()
            .retain(|w| w.workspace_id != ws);
        Ok(())
    }
    fn close_tab(&self, tab: &str) -> Result<(), ShepherdError> {
        if self.fault.get() == Fault::TabReject {
            self.fault.set(Fault::None);
            return Err(ShepherdError::Usage("injected before tab closure".into()));
        }
        self.closed_tabs.borrow_mut().push(tab.into());
        let pane = tab.replace(":t", ":p");
        self.agents.borrow_mut().retain(|a| a.pane_id != pane);
        self.panes.borrow_mut().remove(&pane);
        if self.fault.get() == Fault::TabClose {
            self.fault.set(Fault::None);
            return Err(ShepherdError::Usage("injected after tab closure".into()));
        }
        if self.fault.get() == Fault::TabStore {
            self.fault.set(Fault::None);
            fs::create_dir(self.store.root().join("state.json.tmp")).unwrap();
        }
        Ok(())
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
}
struct Fixture {
    dir: tempfile::TempDir,
    ticket: PathBuf,
    store: Store,
    h: LocalHerdr,
}
impl Fixture {
    fn new(legacy: &[bool]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let ticket = dir.path().join("ticket");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["config", "user.name", "Fixture"]);
        git(&root, &["config", "user.email", "fixture@example.invalid"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        git(&root, &["commit", "--allow-empty", "-m", "base"]);
        git(&root, &["config", "extensions.worktreeConfig", "true"]);
        git(
            &root,
            &["worktree", "add", "-b", "feat/t", ticket.to_str().unwrap()],
        );
        let base = git(&ticket, &["rev-parse", "HEAD"]);
        let store = Store::open(&dir.path().join("state"), &root).unwrap();
        let h = LocalHerdr {
            root: root.clone(),
            store: store.clone(),
            workspaces: RefCell::default(),
            agents: RefCell::default(),
            panes: RefCell::default(),
            fault: Cell::new(Fault::None),
            removals: RefCell::default(),
            closed_tabs: RefCell::default(),
        };
        let mut children = Vec::new();
        for (index, legacy) in legacy.iter().enumerate() {
            let unit = index as u32 + 1;
            let path = dir.path().join(format!("child-{unit}"));
            let branch = format!("feat/t--{unit}");
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_str().unwrap(),
                    &base,
                ],
            );
            herdr_shepherd::gate::hook::install_configured(
                &path,
                &branch,
                &dir.path().join("state"),
                Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
                &dir.path().join("config"),
            )
            .unwrap();
            let file = format!("work-{unit}");
            fs::write(path.join(&file), format!("unit {unit}\n")).unwrap();
            git(&path, &["add", &file]);
            git(&path, &["commit", "-m", &file]);
            let workspace_id = legacy.then(|| format!("child-{unit}"));
            if let Some(id) = &workspace_id {
                h.workspaces.borrow_mut().push(Workspace {
                    workspace_id: id.clone(),
                    label: id.clone(),
                    worktree: Some(WorktreeInfo {
                        repo_key: "fixture".into(),
                        repo_root: Some(root.clone()),
                        checkout_path: Some(path.clone()),
                        is_linked_worktree: true,
                    }),
                });
            }
            children.push(ChildWorktree {
                unit,
                branch,
                path,
                workspace_id,
                base_sha: base.clone(),
                integrated: false,
            });
        }
        integrate(&ticket, &mut children).unwrap();
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    branch: "feat/t".into(),
                    worktree: Some(ticket.clone()),
                    workspace_id: Some("ticket".into()),
                    children,
                    ..Ticket::default()
                });
                Ok(())
            })
            .unwrap();
        Self {
            dir,
            ticket,
            store,
            h,
        }
    }
    fn snapshot(&self) -> Ticket {
        self.store.read().unwrap().tickets.remove(0)
    }
    fn run(&self) -> Result<Vec<u32>, ShepherdError> {
        cleanup_integrated_recorded(&self.h, &self.store, &self.snapshot())
    }
    fn child(&self, unit: u32) -> PathBuf {
        self.dir.path().join(format!("child-{unit}"))
    }
    fn lock(&self, unit: u32) -> PathBuf {
        let path = self
            .h
            .root
            .join(format!(".git/refs/heads/feat/t--{unit}.lock"));
        fs::write(&path, "locked").unwrap();
        path
    }
    fn assert_done(&self, count: u32) {
        let ticket = self.snapshot();
        assert!(ticket.children.is_empty());
        assert!(ticket.child_cleanup.is_empty());
        assert!(self.h.workspaces.borrow().is_empty());
        for unit in 1..=count {
            assert!(!self.child(unit).exists());
            assert!(
                git(
                    &self.ticket,
                    &["branch", "--list", &format!("feat/t--{unit}")]
                )
                .is_empty()
            );
            assert_eq!(
                fs::read_to_string(self.ticket.join(format!("work-{unit}"))).unwrap(),
                format!("unit {unit}\n")
            );
        }
    }
    fn reported_agent(&self, name: &str, pane: &str, worktree: &Path) {
        self.store
            .update(|data| {
                data.agents.push(AgentRecord {
                    name: name.into(),
                    ticket: "T-1".into(),
                    role: "coder".into(),
                    runner: "fake".into(),
                    status: AgentStatus::Reported,
                    pane_id: Some(pane.into()),
                    tab_id: Some(pane.replace(":p", ":t")),
                    worktree: worktree.into(),
                    task: "work".into(),
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
                });
                Ok(())
            })
            .unwrap();
        self.h.agents.borrow_mut().push(AgentInfo {
            name: Some(name.into()),
            agent: "fake".into(),
            agent_status: "idle".into(),
            cwd: Some(worktree.into()),
            pane_id: pane.into(),
            workspace_id: Some("ticket".into()),
        });
        self.h
            .panes
            .borrow_mut()
            .insert(pane.into(), worktree.into());
    }
}

#[test]
fn retry_before_checkout_removal() {
    let f = Fixture::new(&[false, true]);
    git(
        &f.h.root,
        &["worktree", "lock", f.child(1).to_str().unwrap()],
    );
    assert!(f.run().is_err());
    assert_eq!(f.snapshot().child_cleanup.len(), 2);
    assert!(f.child(1).exists());
    assert!(f.child(2).exists());
    git(
        &f.h.root,
        &["worktree", "unlock", f.child(1).to_str().unwrap()],
    );
    f.run().unwrap();
    f.assert_done(2);
}

#[test]
fn retry_later_child_ref_failure_in_native_legacy_and_mixed_sets() {
    for legacy in [[false, false], [true, true], [false, true], [true, false]] {
        let f = Fixture::new(&legacy);
        let lock = f.lock(2);
        assert!(f.run().is_err());
        assert!(!f.child(1).exists());
        assert!(!f.child(2).exists());
        assert!(git(&f.ticket, &["branch", "--list", "feat/t--1"]).is_empty());
        assert_eq!(f.snapshot().children.len(), 1);
        assert_eq!(f.snapshot().child_cleanup.len(), 1);
        fs::remove_file(lock).unwrap();
        f.run().unwrap();
        f.assert_done(2);
    }
}

#[test]
fn retry_legacy_removal_and_workspace_closure_failures() {
    for fault in [Fault::Remove, Fault::WorkspaceClose] {
        let f = Fixture::new(&[true, false]);
        f.h.fault.set(fault);
        assert!(f.run().is_err());
        assert!(!f.child(1).exists());
        assert!(f.child(2).exists());
        assert_eq!(f.h.workspaces.borrow().len(), 1);
        f.run().unwrap();
        f.assert_done(2);
    }
}

#[test]
fn retry_store_failure_after_ref_deletion() {
    let f = Fixture::new(&[true, false]);
    f.h.fault.set(Fault::Store);
    assert!(f.run().is_err());
    assert!(!f.child(1).exists());
    assert!(git(&f.ticket, &["branch", "--list", "feat/t--1"]).is_empty());
    assert_eq!(f.snapshot().children.len(), 2);
    assert_eq!(f.snapshot().child_cleanup.len(), 2);
    fs::remove_dir(f.store.root().join("state.json.tmp")).unwrap();
    f.run().unwrap();
    f.assert_done(2);
}

#[test]
fn retry_tab_close_and_retirement_store_failures_keep_sibling() {
    for fault in [Fault::TabClose, Fault::TabStore, Fault::TabReject] {
        let f = Fixture::new(&[false]);
        f.reported_agent("child", "ticket:p1", &f.child(1));
        f.reported_agent("sibling", "ticket:p2", &f.ticket);
        f.h.fault.set(fault);
        assert!(f.run().is_err());
        assert!(f.child(1).exists());
        assert_eq!(
            f.h.closed_tabs.borrow().len(),
            usize::from(fault != Fault::TabReject)
        );
        if fault == Fault::TabStore {
            fs::remove_dir(f.store.root().join("state.json.tmp")).unwrap();
        }
        f.run().unwrap();
        f.assert_done(1);
        let agents = f.store.read().unwrap().agents;
        assert_eq!(agents[0].status, AgentStatus::Retired);
        assert_eq!(agents[1].status, AgentStatus::Reported);
        assert_eq!(f.h.closed_tabs.borrow().as_slice(), ["ticket:t1"]);
        assert_eq!(f.h.agents.borrow()[0].name.as_deref(), Some("sibling"));
    }
}

#[test]
fn missing_checkout_requires_persisted_intent() {
    let f = Fixture::new(&[false, true]);
    git(
        &f.h.root,
        &["worktree", "remove", f.child(1).to_str().unwrap()],
    );
    assert!(f.run().is_err());
    assert!(f.child(2).exists());
    assert!(f.snapshot().child_cleanup.is_empty());
    assert!(!git(&f.ticket, &["branch", "--list", "feat/t--1"]).is_empty());
}

#[test]
fn retry_refuses_changed_resources_and_unintegrated_commits() {
    for change in [
        "moved-ref",
        "reassigned-checkout",
        "reused-branch",
        "directory",
        "symlink",
        "record-path",
        "record-base",
        "record-workspace",
        "record-unit",
        "record-integrated",
        "receipt-tip",
        "receipt-path",
        "legacy-workspace",
        "ticket-rewind",
    ] {
        let f = Fixture::new(&[true, false]);
        let original = f.snapshot();
        let lock = f.lock(1);
        assert!(f.run().is_err());
        assert!(!f.child(1).exists());
        fs::remove_file(lock).unwrap();
        match change {
            "moved-ref" => {
                git(
                    &f.ticket,
                    &[
                        "update-ref",
                        "refs/heads/feat/t--1",
                        &original.children[0].base_sha,
                    ],
                );
            }
            "reassigned-checkout" => {
                git(
                    &f.h.root,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "other",
                        f.child(1).to_str().unwrap(),
                    ],
                );
            }
            "reused-branch" => {
                git(
                    &f.h.root,
                    &[
                        "worktree",
                        "add",
                        f.dir.path().join("replacement").to_str().unwrap(),
                        "feat/t--1",
                    ],
                );
            }
            "directory" => fs::create_dir(f.child(1)).unwrap(),
            "symlink" => std::os::unix::fs::symlink(f.child(2), f.child(1)).unwrap(),
            "legacy-workspace" => {
                f.h.workspaces.borrow_mut().push(Workspace {
                    workspace_id: "child-1".into(),
                    label: "reassigned".into(),
                    worktree: Some(WorktreeInfo {
                        repo_key: "fixture".into(),
                        repo_root: Some(f.h.root.clone()),
                        checkout_path: Some(f.child(2)),
                        is_linked_worktree: true,
                    }),
                });
            }
            "ticket-rewind" => {
                git(
                    &f.ticket,
                    &["reset", "--hard", &original.children[0].base_sha],
                );
            }
            _ => f
                .store
                .update(|data| {
                    let t = &mut data.tickets[0];
                    match change {
                        "record-path" => t.children[0].path = f.dir.path().join("tampered"),
                        "record-base" => t.children[0].base_sha = "a".repeat(40),
                        "record-workspace" => t.children[0].workspace_id = None,
                        "record-unit" => t.children[0].unit = 99,
                        "record-integrated" => t.children[0].integrated = false,
                        "receipt-tip" => {
                            t.child_cleanup[0].tip = original.children[0].base_sha.clone()
                        }
                        "receipt-path" => t.child_cleanup[0].path = f.dir.path().join("tampered"),
                        _ => unreachable!(),
                    }
                    Ok(())
                })
                .unwrap(),
        }
        let error = f.run().unwrap_err().to_string();
        match change {
            "directory" => {
                assert!(
                    error.starts_with("git branch --show-current failed:"),
                    "{change}: {error}"
                );
                assert!(error.contains("not a git repository"), "{change}: {error}");
            }
            "reassigned-checkout" | "symlink" => {
                assert_eq!(error, "child worktree ownership changed", "{change}");
            }
            "ticket-rewind" => {
                assert_eq!(
                    error,
                    "child commits are not integrated; child tip is not an ancestor of ticket HEAD. Review and merge feat/t--1 into the ticket branch, then retry cleanup",
                    "{change}"
                );
            }
            _ => assert_eq!(error, "integration cleanup ownership changed", "{change}"),
        }
        assert!(f.child(2).exists(), "removed sibling for {change}");
        assert!(
            !git(&f.ticket, &["branch", "--list", "feat/t--1"]).is_empty(),
            "deleted ref for {change}"
        );
        assert_eq!(f.snapshot().children.len(), 2);
    }
}

#[test]
fn whole_set_preflight_retains_earlier_children() {
    for change in ["dirty", "unintegrated", "unguarded", "live"] {
        let f = Fixture::new(&[false, true]);
        match change {
            "dirty" => fs::write(f.child(2).join("keep"), "local").unwrap(),
            "unintegrated" => {
                fs::write(f.child(2).join("keep"), "new commit").unwrap();
                git(&f.child(2), &["add", "keep"]);
                git(&f.child(2), &["commit", "-m", "not integrated"]);
            }
            "live" => {
                f.h.agents.borrow_mut().push(AgentInfo {
                    name: Some("unknown".into()),
                    agent: "fake".into(),
                    agent_status: "working".into(),
                    cwd: Some(f.child(2)),
                    pane_id: "child-2:p1".into(),
                    workspace_id: Some("child-2".into()),
                });
            }
            "unguarded" => {
                git(
                    &f.child(2),
                    &["config", "--worktree", "shepherd.ticketBranch", "other"],
                );
            }
            _ => unreachable!(),
        }
        let error = f.run().unwrap_err().to_string();
        let expected = match change {
            "dirty" => "child worktree must be clean before cleanup",
            "unintegrated" => {
                "child commits are not integrated; child tip is not an ancestor of ticket HEAD. Review and merge feat/t--2 into the ticket branch, then retry cleanup"
            }
            "unguarded" => "child worktree ownership changed",
            "live" => "child still has an unowned or active agent",
            _ => unreachable!(),
        };
        assert_eq!(error, expected, "{change}");
        assert!(f.child(1).exists());
        assert!(f.child(2).exists());
        assert!(f.snapshot().child_cleanup.is_empty());
    }
}

#[test]
fn retirement_retry_refuses_replacement_pane_or_changed_record() {
    for change in ["pane", "agent-record", "session"] {
        let f = Fixture::new(&[false]);
        f.reported_agent("child", "ticket:p1", &f.child(1));
        f.h.fault.set(Fault::TabClose);
        assert!(f.run().is_err());
        match change {
            "pane" => {
                f.h.panes
                    .borrow_mut()
                    .insert("ticket:p1".into(), f.ticket.clone());
            }
            "agent-record" => {
                f.store
                    .update(|data| {
                        data.agents[0].worktree = f.ticket.clone();
                        Ok(())
                    })
                    .unwrap();
            }
            "session" => {
                f.h.agents.borrow_mut().push(AgentInfo {
                    name: Some("replacement".into()),
                    agent: "fake".into(),
                    agent_status: "working".into(),
                    cwd: Some(f.ticket.clone()),
                    pane_id: "ticket:p1".into(),
                    workspace_id: Some("ticket".into()),
                });
            }
            _ => unreachable!(),
        }
        let error = f.run().unwrap_err().to_string();
        let expected = match change {
            "pane" | "session" => "child: automatic retirement deferred; child resources kept",
            "agent-record" => "integration cleanup ownership changed",
            _ => unreachable!(),
        };
        assert_eq!(error, expected, "{change}");
        assert!(f.child(1).exists());
        assert_eq!(f.h.closed_tabs.borrow().len(), 1);
        assert_eq!(f.snapshot().children.len(), 1);
    }
}

#[test]
fn old_cleanup_receipts_without_retiring_agents_still_load() {
    let f = Fixture::new(&[false]);
    let lock = f.lock(1);
    assert!(f.run().is_err());
    fs::remove_file(lock).unwrap();
    let receipt = f.snapshot().child_cleanup.remove(0);
    let mut value = serde_json::to_value(&receipt).unwrap();
    value.as_object_mut().unwrap().remove("retiring_agents");
    let loaded: herdr_shepherd::store::ChildCleanup = serde_json::from_value(value).unwrap();
    assert_eq!(loaded, receipt);
    f.run().unwrap();
    f.assert_done(1);
}

#[test]
fn initial_intent_store_failure_has_no_resource_effects() {
    let f = Fixture::new(&[true, false]);
    fs::create_dir(f.store.root().join("state.json.tmp")).unwrap();
    assert!(f.run().is_err());
    assert!(f.snapshot().child_cleanup.is_empty());
    assert!(f.child(1).exists());
    assert!(f.child(2).exists());
    assert!(f.h.removals.borrow().is_empty());
    fs::remove_dir(f.store.root().join("state.json.tmp")).unwrap();
    f.run().unwrap();
    f.assert_done(2);
}

#[test]
fn changed_record_after_checkout_removal_preserves_ref() {
    let f = Fixture::new(&[true, false]);
    f.h.fault.set(Fault::Record);
    assert_eq!(
        f.run().unwrap_err().to_string(),
        "integration cleanup ownership changed"
    );
    assert!(!f.child(1).exists());
    assert!(f.child(2).exists());
    assert!(!git(&f.ticket, &["branch", "--list", "feat/t--1"]).is_empty());
    assert_eq!(f.snapshot().children.len(), 2);
}

#[test]
fn retires_multiple_reported_tabs_on_one_child() {
    let f = Fixture::new(&[false]);
    f.reported_agent("original", "ticket:p1", &f.child(1));
    f.reported_agent("revision", "ticket:p2", &f.child(1));
    f.run().unwrap();
    f.assert_done(1);
    assert_eq!(f.h.closed_tabs.borrow().len(), 2);
    assert!(
        f.store
            .read()
            .unwrap()
            .agents
            .iter()
            .all(|a| a.status == AgentStatus::Retired)
    );
}

#[test]
fn reassigned_legacy_workspace_is_not_closed_after_removal() {
    let f = Fixture::new(&[true, false]);
    f.h.fault.set(Fault::Reassign);
    assert_eq!(
        f.run().unwrap_err().to_string(),
        "integration cleanup ownership changed"
    );
    assert!(!f.child(1).exists());
    assert!(f.child(2).exists());
    assert_eq!(f.h.workspaces.borrow().len(), 1);
    assert!(!git(&f.ticket, &["branch", "--list", "feat/t--1"]).is_empty());
    assert_eq!(f.snapshot().children.len(), 2);
}

#[test]
fn ancestry_receipt_survives_later_edits_to_landed_lines() {
    let f = Fixture::new(&[false, false]);
    git(
        &f.h.root,
        &["worktree", "lock", f.child(1).to_str().unwrap()],
    );
    assert!(f.run().is_err());
    assert_eq!(f.snapshot().child_cleanup.len(), 2);
    fs::write(f.ticket.join("work-2"), "later ticket edit\n").unwrap();
    git(&f.ticket, &["add", "work-2"]);
    git(&f.ticket, &["commit", "-m", "edit integrated work"]);
    git(
        &f.h.root,
        &["worktree", "unlock", f.child(1).to_str().unwrap()],
    );
    f.run().unwrap();
    assert!(f.snapshot().children.is_empty());
    assert!(f.snapshot().child_cleanup.is_empty());
    for unit in [1, 2] {
        assert!(!f.child(unit).exists());
        assert!(git(&f.ticket, &["branch", "--list", &format!("feat/t--{unit}")]).is_empty());
    }
    assert_eq!(
        fs::read_to_string(f.ticket.join("work-2")).unwrap(),
        "later ticket edit\n"
    );
}
