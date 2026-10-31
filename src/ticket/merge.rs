//! Terminal PR observations for resources owned by a ticket store.

use std::{
    path::Path,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::Config,
    error::ShepherdError,
    gate::transport::{PushTransport, TransportOptions},
    github::{PrBackend, PrState, PrStatus},
    herdr::{Herdr, Workspace},
    notify::Notifier,
    store::{AgentStatus, BlockedReason, Store, Ticket, TicketState},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Unchanged,
    Closed,
    CleanedUp,
    Escalated,
}

fn git(path: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let out = crate::proc::capture(
        Command::new("git")
            .current_dir(path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .args(["-c", "core.fsmonitor=false"])
            .args(args),
        Duration::from_secs(60),
        None,
    )?;
    if !out.status.success() {
        return Err(anyhow::anyhow!("git failed: {}", out.stderr.trim()).into());
    }
    Ok(out.stdout.trim().into())
}

fn same_path(left: &Path, right: &Path) -> bool {
    left.canonicalize()
        .ok()
        .zip(right.canonicalize().ok())
        .is_some_and(|(left, right)| left == right)
}

fn verify_owned(store: &Store, h: &dyn Herdr, ticket: &Ticket) -> Result<(), ShepherdError> {
    let denied = || {
        ShepherdError::PolicyDenied(
            "PR observation does not match the stored ticket resources".into(),
        )
    };
    let path = ticket.worktree.as_deref().ok_or_else(denied)?;
    let data = store.read()?;
    if !same_path(&crate::trusted::main_checkout(path)?, &data.project_root)
        || git(path, &["branch", "--show-current"])? != ticket.branch
    {
        return Err(denied());
    }
    let workspaces = h.workspaces()?;
    let matches = workspaces
        .iter()
        .filter(|workspace| {
            ticket.workspace_id.as_deref() == Some(workspace.workspace_id.as_str())
                && workspace.worktree.as_ref().is_some_and(|tree| {
                    tree.is_linked_worktree
                        && tree
                            .checkout_path
                            .as_ref()
                            .is_some_and(|checkout| same_path(checkout, path))
                })
        })
        .count();
    if matches != 1 {
        return Err(denied());
    }
    Ok(())
}

fn retire_flock(store: &Store, h: &dyn Herdr, id: &str) -> Result<(), ShepherdError> {
    for agent in store.read()?.agents.into_iter().filter(|agent| {
        agent.ticket == id
            && matches!(
                agent.status,
                AgentStatus::Starting
                    | AgentStatus::Working
                    | AgentStatus::Blocked
                    | AgentStatus::Reported
            )
    }) {
        crate::flock::cancel(h, store, &agent.name, false)?;
    }
    Ok(())
}

fn transition(
    store: &Store,
    id: &str,
    state: TicketState,
    reason: Option<BlockedReason>,
    now: &str,
) -> Result<(), ShepherdError> {
    store.update(|data| {
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        super::state::transition(ticket, state, reason, now)
    })
}

fn escalate(
    store: &Store,
    h: &dyn Herdr,
    cfg: &Config,
    id: &str,
    now: &str,
    message: &str,
) -> Result<MergeOutcome, ShepherdError> {
    if store
        .read()?
        .tickets
        .iter()
        .any(|ticket| ticket.id == id && ticket.state != TicketState::Blocked)
    {
        transition(
            store,
            id,
            TicketState::Blocked,
            Some(BlockedReason::Escalated),
            now,
        )?;
    }
    Notifier::new(h, cfg.upkeep.notifications).send(message, "");
    Ok(MergeOutcome::Escalated)
}

fn unpushed(
    ticket: &Ticket,
    status: &PrStatus,
    transport: &PushTransport,
) -> Result<bool, ShepherdError> {
    let path = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    if !git(path, &["status", "--porcelain"])?.is_empty() {
        return Ok(true);
    }
    let tip = transport.verified_ticket_head(&ticket.branch)?;
    let remote = transport.remote_tip(&ticket.branch)?;
    if remote.is_none() {
        // GitHub commonly deletes the head branch immediately after merge.
        // Cleanup still independently proves merged-head identity/containment.
        return Ok(status
            .head_oid
            .as_deref()
            .is_none_or(|merged| merged != tip));
    }
    Ok(remote.as_deref() != Some(tip.as_str()))
}

fn verify_children(ticket: &Ticket, h: &dyn Herdr) -> Result<(), ShepherdError> {
    let path = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    let trees = git(path, &["worktree", "list", "--porcelain", "-z"])?;
    let prefix = format!("branch refs/heads/{}--", ticket.branch);
    let mut tree_path = None;
    let workspaces = h.workspaces()?;
    for field in trees.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            tree_path = Some(Path::new(path));
        }
        if let Some(suffix) = field.strip_prefix(&prefix)
            && !suffix.is_empty()
            && suffix.bytes().all(|byte| byte.is_ascii_digit())
        {
            let branch = field.strip_prefix("branch refs/heads/").unwrap_or_default();
            let owned = ticket.children.iter().any(|child| {
                child.branch == branch
                    && tree_path.is_some_and(|path| same_path(path, &child.path))
                    && workspaces.iter().any(|workspace| {
                        workspace.workspace_id == child.workspace_id
                            && workspace.worktree.as_ref().is_some_and(|tree| {
                                tree.checkout_path
                                    .as_ref()
                                    .is_some_and(|path| same_path(path, &child.path))
                            })
                    })
            });
            if !owned {
                return Err(ShepherdError::PolicyDenied(
                    "cleanup child is not recorded in this ticket store".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Consume a terminal PR observation only for an exact stored PR and workspace.
/// Reserving the terminal/escalated state before retirement prevents later
/// revision dispatch from pushing work after a merge has been observed.
pub fn handle_pr_state(
    store: &Store,
    backend: &dyn PrBackend,
    h: &dyn Herdr,
    cfg: &Config,
    id: &str,
    status: &PrStatus,
    now: &str,
) -> Result<MergeOutcome, ShepherdError> {
    handle_with_context(
        MergeContext {
            store,
            backend,
            h,
            cfg,
            transport_options: TransportOptions::default(),
        },
        id,
        status,
        now,
    )
}

struct MergeContext<'a> {
    store: &'a Store,
    backend: &'a dyn PrBackend,
    h: &'a dyn Herdr,
    cfg: &'a Config,
    transport_options: TransportOptions,
}

fn handle_with_context(
    context: MergeContext<'_>,
    id: &str,
    status: &PrStatus,
    now: &str,
) -> Result<MergeOutcome, ShepherdError> {
    let MergeContext {
        store,
        backend,
        h,
        cfg,
        transport_options,
    } = context;
    if status.state == PrState::Open {
        return Ok(MergeOutcome::Unchanged);
    }
    // Wait for any revision launch to persist its pane before reserving a
    // terminal state. Otherwise retirement can miss an agent still starting.
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let data = store.read()?;
    let Some(ticket) = data.tickets.iter().find(|ticket| ticket.id == id) else {
        return Ok(MergeOutcome::Unchanged);
    };
    if matches!(ticket.state, TicketState::CleanedUp | TicketState::Failed)
        || ticket.pr_number != Some(status.number)
        || status.number == 0
        || ticket.branch != status.head
        || ticket.base != status.base
    {
        return Ok(MergeOutcome::Unchanged);
    }
    let state_dir = store
        .root()
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| ShepherdError::Usage("invalid project store path".into()))?;
    let selected = crate::supervise::config_for(cfg, &data.project_root, state_dir)?;
    let cfg = &selected;
    if ticket.state == TicketState::Blocked {
        if status.state == PrState::Merged {
            Notifier::new(h, cfg.upkeep.notifications).send(
                &format!("{id}: merged while blocked; resources kept"),
                &status.url,
            );
        }
        if ticket.blocked_reason == Some(BlockedReason::Escalated) {
            retire_flock(store, h, id)?;
        }
        return Ok(MergeOutcome::Unchanged);
    }
    verify_owned(store, h, ticket)?;
    if ticket.state == TicketState::Closed {
        retire_flock(store, h, id)?;
        return Ok(MergeOutcome::Unchanged);
    }
    let notifier = Notifier::new(h, cfg.upkeep.notifications);
    if status.state == PrState::Closed {
        if ticket.state == TicketState::Merged {
            return Ok(MergeOutcome::Unchanged);
        }
        transition(store, id, TicketState::Closed, None, now)?;
        notifier.send(&format!("{id}: PR closed; resources kept"), &status.url);
        crate::hooks::emit(
            cfg,
            "ticket.closed",
            &serde_json::json!({"ticket":id,"pr":status.number}),
        );
        retire_flock(store, h, id)?;
        return Ok(MergeOutcome::Closed);
    }
    if status.merged_at.as_deref().is_none_or(str::is_empty) {
        return Err(ShepherdError::Usage(
            "merged PR has no merge timestamp".into(),
        ));
    }
    let unsafe_work = if ticket.state == TicketState::Revising {
        true
    } else {
        let transport = PushTransport::open_with_options(
            state_dir,
            ticket.worktree.as_deref().unwrap(),
            transport_options,
        )?;
        match unpushed(ticket, status, &transport) {
            Ok(unsafe_work) => unsafe_work,
            Err(error) => {
                let outcome = escalate(
                    store,
                    h,
                    cfg,
                    id,
                    now,
                    &format!("{id}: cannot verify merged worktree: {error}"),
                )?;
                retire_flock(store, h, id)?;
                return Ok(outcome);
            }
        }
    };
    if unsafe_work {
        let message = format!(
            "{id}: merged with unpushed work in {}",
            ticket.worktree.as_deref().unwrap().display()
        );
        let outcome = escalate(store, h, cfg, id, now, &message)?;
        if let Err(error) = retire_flock(store, h, id) {
            notifier.send(
                &format!("{id}: flock retirement failed"),
                &error.to_string(),
            );
            return Err(error);
        }
        return Ok(outcome);
    }
    if ticket.state != TicketState::Merged {
        transition(store, id, TicketState::Merged, None, now)?;
        crate::hooks::emit(
            cfg,
            "ticket.merged",
            &serde_json::json!({"ticket":id,"pr":status.number}),
        );
    }
    let cleanup = (|| {
        retire_flock(store, h, id)?;
        verify_children(ticket, h)?;
        let preflight = crate::cleanup::preflight_with_transport(
            h,
            backend,
            cfg,
            ticket.worktree.as_deref().unwrap(),
            false,
            false,
            PushTransport::open_with_options(
                state_dir,
                ticket.worktree.as_deref().unwrap(),
                transport_options,
            )?,
        )?;
        crate::cleanup::run(preflight, false)
    })();
    match cleanup {
        Ok(_) => {
            transition(store, id, TicketState::CleanedUp, None, now)?;
            notifier.send(&format!("{id}: merged and cleaned up"), &status.url);
            Ok(MergeOutcome::CleanedUp)
        }
        Err(error) => escalate(
            store,
            h,
            cfg,
            id,
            now,
            &format!("{id}: cleanup blocked: {error}"),
        ),
    }
}

/// Watcher adapter: never initialize a store for an unrelated workspace.
pub fn handle_workspace_pr_state(
    h: &dyn Herdr,
    backend: &dyn PrBackend,
    cfg: &Config,
    workspace: &Workspace,
    status: &PrStatus,
    state_dir: &Path,
) -> Result<MergeOutcome, ShepherdError> {
    if status.state == PrState::Open {
        return Ok(MergeOutcome::Unchanged);
    }
    let Some(path) = workspace
        .worktree
        .as_ref()
        .and_then(|tree| tree.checkout_path.as_deref())
    else {
        return Ok(MergeOutcome::Unchanged);
    };
    let project = crate::trusted::main_checkout(path)?;
    let store = Store::open(state_dir, &project)?;
    if !store.root().join("state.json").is_file() {
        return Ok(MergeOutcome::Unchanged);
    }
    let data = store.read()?;
    let Some(ticket) = data.tickets.iter().find(|ticket| {
        ticket.workspace_id.as_deref() == Some(workspace.workspace_id.as_str())
            && ticket
                .worktree
                .as_ref()
                .is_some_and(|worktree| same_path(worktree, path))
    }) else {
        return Ok(MergeOutcome::Unchanged);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(anyhow::Error::from)?
        .as_secs()
        .to_string();
    handle_pr_state(&store, backend, h, cfg, &ticket.id, status, &now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{self, Config},
        error::ShepherdError,
        github::{Checks, PrState, PrStatus, ReviewDecision, fake::FakeBackend},
        herdr::{Herdr, Workspace, WorktreeInfo, fake::FakeHerdr},
        store::{AgentRecord, AgentStatus, BlockedReason, Store, Ticket, TicketState},
    };
    use std::{
        cell::Cell,
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    fn git(path: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    struct LocalHerdr {
        fake: FakeHerdr,
        main: PathBuf,
        fail_remove: bool,
        fail_retire: Cell<bool>,
    }
    impl Herdr for LocalHerdr {
        fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
            self.fake.workspaces()
        }
        fn agents(&self) -> Result<Vec<crate::herdr::AgentInfo>, ShepherdError> {
            self.fake.agents()
        }
        fn agent_get(&self, name: &str) -> Result<Option<crate::herdr::AgentInfo>, ShepherdError> {
            self.fake.agent_get(name)
        }
        fn send_keys(&self, name: &str, keys: &[String]) -> Result<(), ShepherdError> {
            if self.fail_retire.replace(false) {
                return Err(anyhow::anyhow!("fixture retirement failed").into());
            }
            self.fake.send_keys(name, keys)
        }
        fn close_tab(&self, tab: &str) -> Result<(), ShepherdError> {
            self.fake.close_tab(tab)
        }
        fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
            self.fake.rename_workspace(id, label)
        }
        fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError> {
            self.fake.move_block(ids, before)
        }
        fn report_workspace_tokens(
            &self,
            ws: &str,
            set: &[(String, String)],
            clear: &[String],
        ) -> Result<(), ShepherdError> {
            self.fake.report_workspace_tokens(ws, set, clear)
        }
        fn report_pane_tokens(
            &self,
            pane: &str,
            set: &[(String, String)],
            clear: &[String],
        ) -> Result<(), ShepherdError> {
            self.fake.report_pane_tokens(pane, set, clear)
        }
        fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError> {
            self.fake.notify(title, body)
        }
        fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError> {
            self.fake.close_workspace(ws)
        }
        fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError> {
            if self.fail_remove {
                return Err(anyhow::anyhow!("fixture removal failed").into());
            }
            let path = self
                .fake
                .workspaces
                .borrow()
                .iter()
                .find(|w| w.workspace_id == ws)
                .unwrap()
                .worktree
                .as_ref()
                .unwrap()
                .checkout_path
                .clone()
                .unwrap();
            assert!(!force);
            git(&self.main, &["worktree", "remove", path.to_str().unwrap()]);
            self.fake.remove_worktree(ws, force)
        }
    }
    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        h: LocalHerdr,
        cfg: Config,
        path: PathBuf,
        status: PrStatus,
    }
    impl Fixture {
        fn new(state: TicketState) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let main = dir.path().join("main");
            let path = dir.path().join("ticket");
            let remote = dir.path().join("remote.git");
            fs::create_dir(&main).unwrap();
            git(&main, &["init", "-b", "main"]);
            git(&main, &["config", "user.name", "Fixture"]);
            git(&main, &["config", "user.email", "fixture@example.invalid"]);
            git(&main, &["config", "commit.gpgsign", "false"]);
            let hooks = dir.path().join("hooks");
            fs::create_dir(&hooks).unwrap();
            git(
                &main,
                &["config", "core.hooksPath", hooks.to_str().unwrap()],
            );
            git(&main, &["commit", "--allow-empty", "-m", "base"]);
            git(dir.path(), &["init", "--bare", remote.to_str().unwrap()]);
            git(
                &main,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(&main, &["push", "origin", "main"]);
            git(
                &main,
                &[
                    "symbolic-ref",
                    "refs/remotes/origin/HEAD",
                    "refs/remotes/origin/main",
                ],
            );
            git(
                &main,
                &["worktree", "add", "-b", "feat/t", path.to_str().unwrap()],
            );
            git(&main, &["config", "extensions.worktreeConfig", "true"]);
            git(
                &path,
                &["config", "--worktree", "shepherd.ticketBranch", "feat/t"],
            );
            git(&path, &["commit", "--allow-empty", "-m", "ticket"]);
            git(&path, &["push", "origin", "feat/t"]);
            git(&main, &["merge", "--ff-only", "feat/t"]);
            git(&main, &["push", "origin", "main"]);
            let status = PrStatus {
                number: 1,
                url: "https://example.invalid/pull/1".into(),
                state: PrState::Merged,
                merged_at: Some("2026-11-01T00:00:00Z".into()),
                base: "main".into(),
                head: "feat/t".into(),
                head_oid: Some(git(&path, &["rev-parse", "HEAD"])),
                checks: Checks::Passing,
                review: ReviewDecision::None,
                comment_ids: vec![],
            };
            let ws = Workspace {
                workspace_id: "w1".into(),
                label: "ticket".into(),
                worktree: Some(WorktreeInfo {
                    repo_key: "fixture".into(),
                    repo_root: Some(main.clone()),
                    checkout_path: Some(path.clone()),
                    is_linked_worktree: true,
                }),
            };
            let h = LocalHerdr {
                fake: FakeHerdr::new(vec![ws]),
                main: main.clone(),
                fail_remove: false,
                fail_retire: Cell::new(false),
            };
            let store = Store::open(&dir.path().join("state"), &main).unwrap();
            crate::trusted::ensure_snapshot(&dir.path().join("state"), &main).unwrap();
            store
                .update(|d| {
                    d.tickets.push(Ticket {
                        id: "T1".into(),
                        branch: "feat/t".into(),
                        base: "main".into(),
                        state,
                        pr_number: Some(1),
                        worktree: Some(path.clone()),
                        workspace_id: Some("w1".into()),
                        ..Ticket::default()
                    });
                    Ok(())
                })
                .unwrap();
            let mut cfg = config::load(&dir.path().join("config"), None).unwrap();
            cfg.upkeep.regroup = false;
            Self {
                _dir: dir,
                store,
                h,
                cfg,
                path,
                status,
            }
        }
        fn run(&self) -> Result<MergeOutcome, ShepherdError> {
            let backend = FakeBackend::new(vec![Ok(Some(self.status.clone()))]);
            let result = handle_with_context(
                MergeContext {
                    store: &self.store,
                    backend: &backend,
                    h: &self.h,
                    cfg: &self.cfg,
                    transport_options: TransportOptions {
                        allow_local_for_tests: true,
                    },
                },
                "T1",
                &self.status,
                "now",
            );
            assert!(
                !backend
                    .calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("push "))
            );
            result
        }
        fn ticket(&self) -> Ticket {
            self.store.read().unwrap().tickets[0].clone()
        }
        fn agent(&self) {
            self.store
                .update(|d| {
                    d.agents.push(AgentRecord {
                        name: "coder".into(),
                        ticket: "T1".into(),
                        role: "coder".into(),
                        runner: "fake".into(),
                        status: AgentStatus::Working,
                        pane_id: Some("w1:p1".into()),
                        tab_id: Some("w1:t1".into()),
                        worktree: self.path.clone(),
                        task: "revise".into(),
                        report: None,
                        observed: None,
                        last_progress_at: "0".into(),
                        nudges: 0,
                        restarts: 0,
                        prompt_pending: false,
                    });
                    Ok(())
                })
                .unwrap();
            self.h
                .fake
                .agent_records
                .borrow_mut()
                .push(crate::herdr::AgentInfo {
                    name: Some("coder".into()),
                    agent: "fake".into(),
                    agent_status: "working".into(),
                    cwd: Some(self.path.clone()),
                    pane_id: "w1:p1".into(),
                    workspace_id: Some("w1".into()),
                });
        }
    }
    #[test]
    fn cleanup_uses_snapshot_despite_mutated_origin() {
        let fixture = Fixture::new(TicketState::PrOpen);
        git(
            &fixture.path,
            &["config", "remote.origin.url", "ext::invalid-agent-command"],
        );
        git(
            &fixture.path,
            &[
                "config",
                "remote.origin.uploadpack",
                "invalid-agent-command",
            ],
        );
        assert_eq!(fixture.run().unwrap(), MergeOutcome::CleanedUp);
        assert!(!fixture.path.exists());
    }

    #[test]
    fn merge_hooks_use_project_snapshot() {
        let mut fixture = Fixture::new(TicketState::PrOpen);
        let trusted = fixture._dir.path().join("trusted-hook");
        let watcher = fixture._dir.path().join("watcher-hook");
        let config = |marker: &Path| {
            format!(
                "[[hooks]]\non = 'ticket.merged'\ncommand = ['touch', '{}']\n",
                marker.display(),
            )
        };
        fixture
            .store
            .update(|data| {
                data.trust.as_mut().unwrap().config_text = Some(config(&trusted));
                Ok(())
            })
            .unwrap();
        fixture.cfg.raw = toml::from_str(&config(&watcher)).unwrap();
        assert_eq!(fixture.run().unwrap(), MergeOutcome::CleanedUp);
        assert!(trusted.exists());
        assert!(!watcher.exists());
    }

    #[test]
    fn missing_merged_head_keeps_deleted_remote_worktree() {
        let mut fixture = Fixture::new(TicketState::PrOpen);
        git(&fixture.h.main, &["push", "origin", "--delete", "feat/t"]);
        fixture.status.head_oid = None;
        let transport = PushTransport::open_with_options(
            &fixture._dir.path().join("state"),
            &fixture.path,
            TransportOptions {
                allow_local_for_tests: true,
            },
        )
        .unwrap();
        assert!(unpushed(&fixture.ticket(), &fixture.status, &transport).unwrap());
        drop(transport);
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(fixture.path.exists());
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
    }

    #[test]
    fn merged_while_blocked_notifies_and_keeps_worktree() {
        let fixture = Fixture::new(TicketState::Blocked);
        fixture
            .store
            .update(|data| {
                data.tickets[0].blocked_reason = Some(BlockedReason::Conflict);
                Ok(())
            })
            .unwrap();
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
        assert!(fixture.path.exists());
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("merged while blocked"))
        );
    }

    #[test]
    fn merged_clean_cleans_up() {
        let fixture = Fixture::new(TicketState::PrOpen);
        assert_eq!(fixture.run().unwrap(), MergeOutcome::CleanedUp);
        assert_eq!(fixture.ticket().state, TicketState::CleanedUp);
        assert!(!fixture.path.exists());
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("merged and cleaned up"))
        );
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
    }
    #[test]
    fn merged_during_revision_escalates_and_keeps() {
        let fixture = Fixture::new(TicketState::Revising);
        fixture.agent();
        fs::write(fixture.path.join("in-flight"), "unfinished").unwrap();
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
        assert_eq!(
            fixture.ticket().blocked_reason,
            Some(BlockedReason::Escalated)
        );
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            AgentStatus::Retired
        );
        assert!(fixture.path.join("in-flight").exists());
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("merged with unpushed work"))
        );
        assert!(
            !fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("remove_worktree"))
        );
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
    }
    #[test]
    fn closed_keeps_resources() {
        let mut fixture = Fixture::new(TicketState::CiWait);
        fixture.agent();
        fixture.status.state = PrState::Closed;
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Closed);
        assert_eq!(fixture.ticket().state, TicketState::Closed);
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            AgentStatus::Retired
        );
        assert!(fixture.path.exists());
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
    }
    #[test]
    fn cleanup_failure_escalates() {
        let mut fixture = Fixture::new(TicketState::PrOpen);
        fixture.h.fail_remove = true;
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
        assert_eq!(
            fixture.ticket().blocked_reason,
            Some(BlockedReason::Escalated)
        );
        assert!(fixture.path.exists());
    }
    #[test]
    fn merged_with_unpushed_commit_keeps_worktree() {
        let fixture = Fixture::new(TicketState::PrOpen);
        git(
            &fixture.path,
            &["commit", "--allow-empty", "-m", "local revision"],
        );
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(fixture.path.exists());
        assert_eq!(
            fixture.ticket().blocked_reason,
            Some(BlockedReason::Escalated)
        );
    }

    #[test]
    fn dirty_merged_worktree_is_kept() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fs::write(fixture.path.join("unfinished"), "keep this").unwrap();
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(fixture.path.join("unfinished").exists());
    }

    #[test]
    fn unrelated_pr_observation_does_not_retire_or_remove() {
        let mut fixture = Fixture::new(TicketState::Revising);
        fixture.agent();
        fixture.status.number = 99;
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
        assert_eq!(fixture.ticket().state, TicketState::Revising);
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            AgentStatus::Working
        );
        assert!(fixture.h.fake.calls.borrow().is_empty());
        assert!(fixture.path.exists());
    }

    #[test]
    fn merged_clean_removes_recorded_children_first() {
        let fixture = Fixture::new(TicketState::PrOpen);
        let child = fixture._dir.path().join("child");
        git(
            &fixture.h.main,
            &[
                "worktree",
                "add",
                "-b",
                "feat/t--1",
                child.to_str().unwrap(),
                "feat/t",
            ],
        );
        git(
            &child,
            &["config", "--worktree", "shepherd.ticketBranch", "feat/t--1"],
        );
        fixture.h.fake.workspaces.borrow_mut().push(Workspace {
            workspace_id: "child".into(),
            label: "child".into(),
            worktree: Some(WorktreeInfo {
                repo_key: "fixture".into(),
                repo_root: Some(fixture.h.main.clone()),
                checkout_path: Some(child.clone()),
                is_linked_worktree: true,
            }),
        });
        fixture
            .store
            .update(|data| {
                data.tickets[0].children.push(crate::store::ChildWorktree {
                    unit: 1,
                    branch: "feat/t--1".into(),
                    path: child.clone(),
                    workspace_id: "child".into(),
                    base_sha: fixture.status.head_oid.clone().unwrap(),
                    integrated: true,
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(fixture.run().unwrap(), MergeOutcome::CleanedUp);
        assert!(!child.exists());
        let calls = fixture.h.fake.calls.borrow();
        let removed = calls
            .iter()
            .filter(|call| call.starts_with("remove_worktree"))
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            removed,
            ["remove_worktree child false", "remove_worktree w1 false"]
        );
    }

    #[test]
    fn unrecorded_child_blocks_cleanup_before_removal() {
        let fixture = Fixture::new(TicketState::PrOpen);
        let child = fixture._dir.path().join("child");
        git(
            &fixture.h.main,
            &[
                "worktree",
                "add",
                "-b",
                "feat/t--1",
                child.to_str().unwrap(),
                "feat/t",
            ],
        );
        git(
            &child,
            &["config", "--worktree", "shepherd.ticketBranch", "feat/t--1"],
        );
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(fixture.path.exists());
        assert!(child.exists());
        assert!(
            !fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("remove_worktree"))
        );
    }

    #[test]
    fn watcher_never_initializes_an_unrelated_store() {
        let fixture = Fixture::new(TicketState::PrOpen);
        let state = fixture._dir.path().join("unrelated-state");
        let backend = FakeBackend::new(vec![]);
        let workspace = fixture.h.fake.workspaces.borrow()[0].clone();
        assert_eq!(
            handle_workspace_pr_state(
                &fixture.h,
                &backend,
                &fixture.cfg,
                &workspace,
                &fixture.status,
                &state
            )
            .unwrap(),
            MergeOutcome::Unchanged
        );
        assert!(!state.exists());
        assert!(fixture.path.exists());
    }
    #[test]
    fn merged_during_revision_keeps_even_a_clean_worktree() {
        let fixture = Fixture::new(TicketState::Revising);
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(fixture.path.exists());
        assert_eq!(
            fixture.ticket().blocked_reason,
            Some(BlockedReason::Escalated)
        );
    }

    #[test]
    fn merged_from_ci_wait_cleans_up() {
        let fixture = Fixture::new(TicketState::CiWait);
        assert_eq!(fixture.run().unwrap(), MergeOutcome::CleanedUp);
        assert_eq!(fixture.ticket().state, TicketState::CleanedUp);
    }
    #[test]
    fn failed_retirement_is_retried_without_cleanup() {
        for state in [TicketState::Revising, TicketState::CiWait] {
            let mut fixture = Fixture::new(state);
            fixture.agent();
            if state == TicketState::CiWait {
                fixture.status.state = PrState::Closed;
            }
            fixture.h.fail_retire.set(true);
            assert!(fixture.run().is_err());
            let expected = if state == TicketState::Revising {
                TicketState::Blocked
            } else {
                TicketState::Closed
            };
            assert_eq!(fixture.ticket().state, expected);
            assert_eq!(
                fixture.store.read().unwrap().agents[0].status,
                AgentStatus::Working
            );
            assert!(fixture.path.exists());
            assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
            assert_eq!(
                fixture.store.read().unwrap().agents[0].status,
                AgentStatus::Retired
            );
            assert_eq!(fixture.ticket().state, expected);
            assert!(
                !fixture
                    .h
                    .fake
                    .calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("remove_worktree"))
            );
        }
    }

    #[test]
    fn merged_from_building_cleans_up() {
        let fixture = Fixture::new(TicketState::Building);
        assert_eq!(fixture.run().unwrap(), MergeOutcome::CleanedUp);
        assert_eq!(fixture.ticket().state, TicketState::CleanedUp);
    }
    #[test]
    fn merged_waits_for_inflight_revision_launch_before_retiring() {
        let fixture = Fixture::new(TicketState::Revising);
        fixture.agent();
        let store = fixture.store.clone();
        let agent = store.update(|data| Ok(data.agents.remove(0))).unwrap();
        let dispatch = crate::ticket::revise::dispatch_lock(&store, "T1").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let outcome = fixture.run();
            finished_tx.send(()).unwrap();
            (fixture, outcome)
        });
        started_rx.recv().unwrap();
        assert!(
            matches!(
                finished_rx.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "merge retirement ran before the revision launch released its dispatch lock"
        );
        assert_eq!(
            store.read().unwrap().tickets[0].state,
            TicketState::Revising
        );
        // The launching dispatcher persists the live agent before releasing
        // its dispatch lock. Retirement must observe this completed record.
        store
            .update(|data| {
                data.agents.push(agent);
                Ok(())
            })
            .unwrap();
        drop(dispatch);
        let (fixture, outcome) = worker.join().unwrap();
        assert_eq!(outcome.unwrap(), MergeOutcome::Escalated);
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            AgentStatus::Retired
        );
        assert!(fixture.h.fake.agent_records.borrow().is_empty());
        assert!(fixture.path.exists());
    }
}
