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
    let state_dir = store
        .root()
        .parent()
        .and_then(Path::parent)
        .ok_or_else(denied)?;
    let transport = PushTransport::open(state_dir, path)?;
    if transport.trust.project_root != data.project_root || !transport.matches_repository(path)? {
        return Err(denied());
    }
    transport.verified_ticket_head(&ticket.branch)?;
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

#[cfg(test)]
fn retire_flock(store: &Store, h: &dyn Herdr, id: &str) -> Result<(), ShepherdError> {
    retire_flock_inner(store, h, id, None)
}

pub(crate) fn retire_flock_with_config(
    store: &Store,
    h: &dyn Herdr,
    id: &str,
    cfg: &Config,
) -> Result<(), ShepherdError> {
    retire_flock_inner(store, h, id, Some(cfg))
}

fn retire_flock_inner(
    store: &Store,
    h: &dyn Herdr,
    id: &str,
    cfg: Option<&Config>,
) -> Result<(), ShepherdError> {
    let _hooks = crate::hooks::defer();
    // A pending close belongs to its exact assignment, even if the process has
    // vanished. Never silently skip a changed recipient on a handoff retry.
    let data = store.read()?;
    for expected in data
        .cleanup_retiring_agents
        .iter()
        .filter(|a| a.ticket == id)
    {
        let mut retired = expected.clone();
        retired.status = AgentStatus::Retired;
        if !data.agents.contains(expected) && !data.agents.contains(&retired) {
            return Err(ShepherdError::PolicyDenied(
                "handoff retirement intent changed; resources kept".into(),
            ));
        }
    }
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
        let retired = crate::flock::automatic::retire_for_ticket_handoff(h, store, &agent, cfg)?;
        if !retired {
            return Err(ShepherdError::PolicyDenied(format!(
                "{}: automatic retirement deferred; resources kept",
                agent.name
            )));
        }
    }
    // Successful handoff releases the admission fence so a later CI or PR
    // revision can start its fresh cohort. Failed closes retain their intent.
    store.update(|data| {
        for expected in data
            .cleanup_retiring_agents
            .iter()
            .filter(|a| a.ticket == id)
        {
            let mut retired = expected.clone();
            retired.status = AgentStatus::Retired;
            if !data.agents.contains(&retired) {
                return Err(ShepherdError::PolicyDenied(
                    "handoff retirement incomplete; resources kept".into(),
                ));
            }
        }
        data.cleanup_retiring_agents.retain(|a| a.ticket != id);
        Ok(())
    })?;
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
    store.update(|data| {
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        if ticket.state != TicketState::Blocked {
            super::state::transition(
                ticket,
                TicketState::Blocked,
                Some(BlockedReason::Escalated),
                now,
            )?;
        }
        // This escalation already tells the user about the merged ticket.
        // Later blocked observations must not produce a second merge notice.
        ticket.merged_while_blocked_notified = true;
        Ok(())
    })?;
    Notifier::new(h, cfg.upkeep.notifications).send(message, "");
    Ok(MergeOutcome::Escalated)
}

fn unpushed(
    ticket: &Ticket,
    status: &PrStatus,
    transport: &PushTransport,
) -> Result<bool, ShepherdError> {
    if status.head_oid.is_none() {
        return Ok(true);
    }
    if !transport.worktree_status(&ticket.branch)?.is_empty() {
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
                    && match &child.workspace_id {
                        Some(id) => workspaces.iter().any(|workspace| {
                            workspace.workspace_id == *id
                                && workspace.worktree.as_ref().is_some_and(|tree| {
                                    tree.is_linked_worktree
                                        && tree
                                            .checkout_path
                                            .as_ref()
                                            .is_some_and(|path| same_path(path, &child.path))
                                })
                        }),
                        None => !workspaces.iter().any(|workspace| {
                            workspace.worktree.as_ref().is_some_and(|tree| {
                                tree.checkout_path
                                    .as_ref()
                                    .is_some_and(|path| same_path(path, &child.path))
                            })
                        }),
                    }
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
    // Terminal and cleanup hooks can reenter the same ticket's ownership.
    let _hooks = crate::hooks::defer();
    // Wait for any revision launch to persist its pane before reserving a
    // terminal state. Otherwise retirement can miss an agent still starting.
    let _dispatch = super::revise::dispatch_lock(store, id)?;
    let _operation = super::operation::exclusive(store, id)?;
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
            let notify = store.update(|data| {
                let Some(current) = data.tickets.iter_mut().find(|current| current.id == id) else {
                    return Ok(false);
                };
                if current.merged_while_blocked_notified {
                    return Ok(false);
                }
                current.merged_while_blocked_notified = true;
                Ok(true)
            })?;
            if notify {
                Notifier::new(h, cfg.upkeep.notifications).send(
                    &format!("{id}: merged while blocked; resources kept"),
                    &status.url,
                );
            }
        }
        if ticket.blocked_reason == Some(BlockedReason::Escalated) {
            retire_flock_with_config(store, h, id, cfg)?;
        }
        return Ok(MergeOutcome::Unchanged);
    }
    if let Err(error) = verify_owned(store, h, ticket) {
        if status.state == PrState::Merged {
            return escalate(
                store,
                h,
                cfg,
                id,
                now,
                &format!("{id}: merged but resource verification failed; resources kept: {error}"),
            );
        }
        return Err(error);
    }
    if ticket.state == TicketState::Closed {
        retire_flock_with_config(store, h, id, cfg)?;
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
        retire_flock_with_config(store, h, id, cfg)?;
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
        let check = (|| {
            let transport = PushTransport::open_with_options(
                state_dir,
                ticket.worktree.as_deref().unwrap(),
                transport_options,
            )?;
            unpushed(ticket, status, &transport)
        })();
        match check {
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
                retire_flock_with_config(store, h, id, cfg)?;
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
        if let Err(error) = retire_flock_with_config(store, h, id, cfg) {
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
        retire_flock_with_config(store, h, id, cfg)?;
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
        fn pane_exists(&self, pane: &str) -> Result<bool, ShepherdError> {
            self.fake.pane_exists(pane)
        }
        fn pane_cwd(&self, pane: &str) -> Result<Option<PathBuf>, ShepherdError> {
            self.fake.pane_cwd(pane)
        }
        fn pane_session_ref(&self, pane: &str) -> Result<Option<String>, ShepherdError> {
            self.fake.pane_session_ref(pane)
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
                        task_id: None,
                        attempt_id: None,
                        unit: None,
                        launch_profile: None,
                        completed_tasks: vec![],
                        dispatch_reason: None,
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
        fn reported_flock(&self) {
            self.agent();
            let template = self.store.read().unwrap().agents[0].clone();
            self.store
                .update(|data| {
                    data.agents.clear();
                    for (index, role) in ["coder", "reviewer", "qa"].into_iter().enumerate() {
                        let mut rec = template.clone();
                        rec.name = role.into();
                        rec.role = role.into();
                        rec.status = AgentStatus::Reported;
                        rec.pane_id = Some(format!("w1:p{}", index + 1));
                        rec.tab_id = Some(format!("w1:t{}", index + 1));
                        rec.task_id = Some(format!("task-{role}"));
                        rec.observed = Some(
                            serde_json::json!({"session_ref":format!("session-{role}")})
                                .to_string(),
                        );
                        data.agents.push(rec);
                    }
                    Ok(())
                })
                .unwrap();
            self.h.fake.agent_records.borrow_mut().clear();
            for rec in self.store.read().unwrap().agents {
                let pane = rec.pane_id.unwrap();
                self.h
                    .fake
                    .pane_sessions
                    .borrow_mut()
                    .insert(pane.clone(), format!("session-{}", rec.role));
                self.h
                    .fake
                    .agent_records
                    .borrow_mut()
                    .push(crate::herdr::AgentInfo {
                        name: Some(rec.name),
                        agent: "fake".into(),
                        agent_status: "idle".into(),
                        cwd: Some(self.path.clone()),
                        pane_id: pane,
                        workspace_id: Some("w1".into()),
                    });
            }
        }
        fn retirement_events(&mut self) -> PathBuf {
            let events = self._dir.path().join("retirement-events.jsonl");
            let command = vec![
                "sh".to_owned(),
                "-c".into(),
                "cat >> \"$1\"; printf '\\n' >> \"$1\"".into(),
                "hook".into(),
                events.display().to_string(),
            ];
            let text = format!(
                "[[hooks]]\non = 'agent.retired'\ncommand = {}\n",
                toml::Value::from(command)
            );
            self.cfg.raw = toml::from_str(&text).unwrap();
            self.store
                .update(|data| {
                    data.trust.as_mut().unwrap().config_text = Some(text);
                    Ok(())
                })
                .unwrap();
            events
        }
    }

    #[test]
    fn ticket_handoff_retries_lost_close_acknowledgement() {
        for entry in ["direct", "merge"] {
            for failed_close in 1..=3 {
                let mut fixture = Fixture::new(TicketState::PrOpen);
                fixture.reported_flock();
                let events = fixture.retirement_events();
                for _ in 1..failed_close {
                    fixture
                        .h
                        .fake
                        .close_tab_results
                        .borrow_mut()
                        .push_back(Ok(()));
                }
                fixture
                    .h
                    .fake
                    .close_tab_results
                    .borrow_mut()
                    .push_back(Err(ShepherdError::Usage(
                        "lost close acknowledgement".into(),
                    )));
                let retire =
                    || retire_flock_with_config(&fixture.store, &fixture.h, "T1", &fixture.cfg);
                if entry == "direct" {
                    assert!(
                        retire()
                            .unwrap_err()
                            .to_string()
                            .contains("lost close acknowledgement")
                    );
                } else {
                    assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
                }
                assert_eq!(
                    fixture.h.fake.agent_records.borrow().len(),
                    3 - failed_close
                );
                assert_eq!(
                    fixture.store.read().unwrap().agents[failed_close - 1].status,
                    AgentStatus::Reported
                );
                assert!(
                    !fixture
                        .h
                        .pane_exists(&format!("w1:p{failed_close}"))
                        .unwrap()
                );
                assert_eq!(
                    fs::read_to_string(&events)
                        .unwrap_or_default()
                        .lines()
                        .count(),
                    failed_close - 1
                );

                for _ in 0..2 {
                    if entry == "direct" {
                        retire().unwrap();
                    } else {
                        assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
                        assert_eq!(fixture.ticket().state, TicketState::Blocked);
                    }
                }
                let data = fixture.store.read().unwrap();
                assert!(data.agents.iter().all(|a| a.status == AgentStatus::Retired));
                assert!(data.cleanup_retiring_agents.is_empty());
                assert!(fixture.h.fake.agent_records.borrow().is_empty());
                assert_eq!(
                    fixture
                        .h
                        .fake
                        .calls
                        .borrow()
                        .iter()
                        .filter(|call| call.starts_with("close_tab "))
                        .count(),
                    3
                );
                let emitted: Vec<serde_json::Value> = fs::read_to_string(events)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(emitted.len(), 3);
                for (event, role) in emitted.iter().zip(["coder", "reviewer", "qa"]) {
                    assert_eq!(event["agent"], role);
                    assert_eq!(event["task_id"], format!("task-{role}"));
                }
            }
        }
    }

    #[test]
    fn ticket_handoff_keeps_live_or_changed_recipients() {
        for blocked_by in ["close-error", "close-no-effect", "task", "session"] {
            let mut fixture = Fixture::new(TicketState::PrOpen);
            fixture.reported_flock();
            let events = fixture.retirement_events();
            fixture
                .h
                .fake
                .close_tab_effects
                .borrow_mut()
                .push_back(false);
            fixture
                .h
                .fake
                .close_tab_results
                .borrow_mut()
                .push_back(Err(ShepherdError::Usage("close failed".into())));
            assert!(
                retire_flock_with_config(&fixture.store, &fixture.h, "T1", &fixture.cfg).is_err()
            );
            match blocked_by {
                "task" => fixture
                    .store
                    .update(|data| {
                        data.agents[0].task_id = Some("replacement-task".into());
                        Ok(())
                    })
                    .unwrap(),
                "session" => {
                    fixture
                        .h
                        .fake
                        .pane_sessions
                        .borrow_mut()
                        .insert("w1:p1".into(), "replacement-session".into());
                }
                _ => {
                    fixture
                        .h
                        .fake
                        .close_tab_effects
                        .borrow_mut()
                        .push_back(false);
                    if blocked_by == "close-error" {
                        fixture
                            .h
                            .fake
                            .close_tab_results
                            .borrow_mut()
                            .push_back(Err(ShepherdError::Usage("close failed again".into())));
                    }
                }
            }
            assert!(
                retire_flock_with_config(&fixture.store, &fixture.h, "T1", &fixture.cfg).is_err()
            );
            assert!(fixture.h.pane_exists("w1:p1").unwrap());
            assert!(
                fixture
                    .store
                    .read()
                    .unwrap()
                    .agents
                    .iter()
                    .all(|a| a.status == AgentStatus::Reported)
            );
            assert!(!events.exists());
        }
    }

    #[test]
    fn automatic_terminal_retirement_keeps_unknown_or_busy_resources() {
        for state in [TicketState::PrOpen, TicketState::Closed] {
            for blocked_by in ["unknown", "busy", "session", "pane", "outside", "ticket"] {
                let fixture = Fixture::new(state);
                fixture.agent();
                if blocked_by == "unknown" {
                    fixture
                        .store
                        .update(|data| {
                            data.agents[0].set_prompt_delivery_uncertain(true);
                            data.agents[0].status = AgentStatus::Blocked;
                            Ok(())
                        })
                        .unwrap();
                }
                if blocked_by == "session" {
                    fixture
                        .store
                        .update(|data| {
                            data.agents[0].observed = Some(r#"{"session_ref":"saved"}"#.into());
                            Ok(())
                        })
                        .unwrap();
                }
                if blocked_by == "pane" {
                    fixture.h.fake.agent_records.borrow_mut()[0].pane_id = "w1:other".into();
                }
                if blocked_by == "outside" {
                    fixture.h.fake.agent_records.borrow_mut()[0].cwd = Some(fixture.h.main.clone());
                }
                if blocked_by == "ticket" {
                    fixture
                        .store
                        .update(|data| {
                            data.agents[0].ticket = "other".into();
                            Ok(())
                        })
                        .unwrap();
                }
                let _operation = (blocked_by == "busy").then(|| {
                    crate::flock::try_agent_operation(&fixture.store, "coder")
                        .unwrap()
                        .unwrap()
                });
                // Terminal ticket policy may still advance/escalate, but cleanup must stop.
                let result = fixture.run();
                assert!(
                    result.is_err() || result.unwrap() != MergeOutcome::CleanedUp,
                    "{blocked_by}"
                );
                assert!(fixture.path.exists());
                assert_ne!(fixture.ticket().state, TicketState::CleanedUp);
                assert_ne!(
                    fixture.store.read().unwrap().agents[0].status,
                    AgentStatus::Retired
                );
                assert!(
                    !fixture.h.fake.calls.borrow().iter().any(|call| call
                        .starts_with("send_keys ")
                        || call.starts_with("close_tab ")
                        || call.starts_with("remove_worktree ")
                        || call.starts_with("close_workspace ")),
                    "{blocked_by}: {:?}",
                    fixture.h.fake.calls.borrow()
                );
            }
        }
    }

    #[test]
    fn automatic_retirement_rechecks_project_ownership_after_transport() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fixture.agent();
        let store = fixture.store.clone();
        let outside = fixture.h.main.clone();
        *fixture.h.fake.on_get.borrow_mut() = Some(Box::new(move |_| {
            store
                .update(|data| {
                    data.tickets[0].worktree = Some(outside.clone());
                    Ok(())
                })
                .unwrap();
        }));
        assert!(retire_flock(&fixture.store, &fixture.h, "T1").is_err());
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            AgentStatus::Working
        );
        assert!(
            !fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("send_keys ") || call.starts_with("close_tab "))
        );
    }

    #[test]
    fn automatic_retirement_never_follows_a_reassigned_candidate() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fixture.agent();
        fixture
            .store
            .update(|data| {
                let mut sibling = data.agents[0].clone();
                sibling.name = "sibling".into();
                sibling.pane_id = Some("w1:p2".into());
                sibling.tab_id = Some("w1:t2".into());
                data.agents.push(sibling);
                let mut ticket = data.tickets[0].clone();
                ticket.id = "T2".into();
                data.tickets.push(ticket);
                Ok(())
            })
            .unwrap();
        let mut info = fixture.h.fake.agent_records.borrow()[0].clone();
        info.name = Some("sibling".into());
        info.pane_id = "w1:p2".into();
        fixture.h.fake.agent_records.borrow_mut().push(info);
        let store = fixture.store.clone();
        *fixture.h.fake.on_send_keys.borrow_mut() = Some(Box::new(move |name| {
            if name == "coder" {
                store
                    .update(|data| {
                        data.agents[1].ticket = "T2".into();
                        Ok(())
                    })
                    .unwrap();
            }
        }));
        let _ = retire_flock(&fixture.store, &fixture.h, "T1");
        assert_eq!(
            fixture.store.read().unwrap().agents[1].status,
            AgentStatus::Working
        );
        assert!(
            !fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("send_keys sibling ") || call == "close_tab w1:t2")
        );
    }

    #[test]
    fn automatic_retirement_preserves_reports_arriving_during_escape() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fixture.agent();
        let store = fixture.store.clone();
        *fixture.h.fake.on_send_keys.borrow_mut() = Some(Box::new(move |name| {
            crate::flock::store_report(&store, name, r#"{"status":"completed","summary":"real report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        }));
        assert!(
            !crate::flock::retire_automatically(
                &fixture.h,
                &fixture.store,
                &fixture.store.read().unwrap().agents[0]
            )
            .unwrap()
        );
        let rec = fixture.store.read().unwrap().agents[0].clone();
        assert_eq!(rec.status, AgentStatus::Reported);
        assert_eq!(rec.report.as_ref().unwrap().summary, "real report");
        assert!(
            !fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("close_tab "))
        );
        *fixture.h.fake.on_send_keys.borrow_mut() = None;
        assert!(
            crate::flock::retire_automatically(
                &fixture.h,
                &fixture.store,
                &fixture.store.read().unwrap().agents[0]
            )
            .unwrap()
        );
        assert_eq!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("send_keys "))
                .count(),
            1
        );
        assert_eq!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("close_tab "))
                .count(),
            1
        );
        assert_eq!(fixture.store.read().unwrap().agents[0].report, rec.report);
    }

    #[test]
    fn explicit_operator_cancel_can_retire_unknown_delivery() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fixture.agent();
        fixture
            .store
            .update(|data| {
                data.agents[0].set_prompt_delivery_uncertain(true);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            crate::flock::cancel(&fixture.h, &fixture.store, "coder", false)
                .unwrap()
                .status,
            AgentStatus::Retired
        );
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("send_keys "))
        );
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("close_tab "))
        );
        assert!(fixture.path.exists());
    }

    #[test]
    fn transport_open_failure_escalates_and_notifies() {
        let fixture = Fixture::new(TicketState::PrOpen);
        fixture
            .store
            .update(|data| {
                data.trust = None;
                Ok(())
            })
            .unwrap();
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
        assert!(fixture.path.exists());
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("notify"))
        );
    }

    #[test]
    fn merged_cleanup_never_executes_agent_clean_filter() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new(TicketState::PrOpen);
        fs::write(fixture.path.join(".gitattributes"), "*.txt filter=agent\n").unwrap();
        fs::write(fixture.path.join("tracked.txt"), "human content\n").unwrap();
        git(&fixture.path, &["add", ".gitattributes", "tracked.txt"]);
        git(&fixture.path, &["commit", "-m", "track attributed content"]);
        git(&fixture.path, &["push", "origin", "feat/t"]);
        git(&fixture.h.main, &["merge", "--ff-only", "feat/t"]);
        git(&fixture.h.main, &["push", "origin", "main"]);
        fixture.status.head_oid = Some(git(&fixture.path, &["rev-parse", "HEAD"]));
        let marker = fixture._dir.path().join("agent-clean-executed");
        let helper = fixture._dir.path().join("agent-clean");
        fs::write(
            &helper,
            format!("#!/bin/sh\nprintf ran > '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        git(
            &fixture.path,
            &["config", "filter.agent.clean", helper.to_str().unwrap()],
        );
        // Refresh the timestamp so status must examine the attributed file.
        fs::write(fixture.path.join("tracked.txt"), "human content\n").unwrap();
        fixture.h.fail_remove = true;
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(
            !marker.exists(),
            "merged-ticket observation executed agent Git config"
        );
        assert!(fixture.path.exists());
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
    fn terminal_hooks_can_reenter_ticket_ownership() {
        // Run cancellation from the shell hook's subprocess. Terminal tickets
        // must reject it normally, rather than blocking under parent locks.
        if let Some(root) = std::env::var_os("SHEPHERD_TEST_TERMINAL_HOOK_ROOT") {
            let root = PathBuf::from(root);
            let store = Store::open(&root.join("state"), &root.join("main")).unwrap();
            let error = crate::ticket::intake::cancel_agents(
                &FakeHerdr::new(vec![]),
                &store,
                "T1",
                "hook",
                3,
            )
            .unwrap_err();
            assert!(error.to_string().contains("illegal ticket transition"));
            fs::write(root.join("reentered"), "cancel rejected").unwrap();
            return;
        }
        for event in ["ticket.closed", "ticket.merged", "ticket.cleaned_up"] {
            let mut fixture = Fixture::new(TicketState::PrOpen);
            if event == "ticket.closed" {
                fixture.status.state = PrState::Closed;
            }
            let command: toml::Value = vec![
                "sh".to_owned(),
                "-c".into(),
                "exec env \"$@\"".into(),
                "hook".into(),
                format!(
                    "SHEPHERD_TEST_TERMINAL_HOOK_ROOT={}",
                    fixture._dir.path().display()
                ),
                std::env::current_exe().unwrap().display().to_string(),
                "--exact".into(),
                "ticket::merge::tests::terminal_hooks_can_reenter_ticket_ownership".into(),
                "--nocapture".into(),
            ]
            .into();
            let text =
                format!("[[hooks]]\non = '{event}'\ncommand = {command}\ntimeout_seconds = 1\n");
            fixture
                .store
                .update(|data| {
                    data.trust.as_mut().unwrap().config_text = Some(text);
                    Ok(())
                })
                .unwrap();
            let expected = if event == "ticket.closed" {
                MergeOutcome::Closed
            } else {
                MergeOutcome::CleanedUp
            };
            assert_eq!(fixture.run().unwrap(), expected);
            assert!(
                fixture._dir.path().join("reentered").exists(),
                "{event} hook must acquire ownership and reject cancellation normally"
            );
        }
    }

    #[test]
    fn missing_merged_head_keeps_existing_remote_worktree() {
        let mut fixture = Fixture::new(TicketState::PrOpen);
        fixture.status.head_oid = None;
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        assert!(fixture.path.exists());
        assert_eq!(fixture.ticket().state, TicketState::Blocked);
        assert!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("unpushed work"))
        );
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
    fn merged_while_blocked_notifies_once_across_ticks() {
        let fixture = Fixture::new(TicketState::Blocked);
        for _ in 0..3 {
            assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
        }
        let calls = fixture.h.fake.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.contains("merged while blocked"))
                .count(),
            1
        );
        assert!(fixture.path.exists());
    }

    #[test]
    fn merged_escalation_does_not_notify_again_when_blocked() {
        let fixture = Fixture::new(TicketState::Revising);
        assert_eq!(fixture.run().unwrap(), MergeOutcome::Escalated);
        for _ in 0..3 {
            assert_eq!(fixture.run().unwrap(), MergeOutcome::Unchanged);
        }
        assert_eq!(
            fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify ") && call.contains("merged"))
                .count(),
            1
        );
    }

    #[test]
    fn watcher_handles_ticket_merge_only_in_lifecycle_pass() {
        let fixture = Fixture::new(TicketState::Blocked);
        let mut state = crate::watch::WatchState {
            state_dir: Some(fixture._dir.path().join("state")),
            ..Default::default()
        };
        let mut open = fixture.status.clone();
        open.state = PrState::Open;
        open.merged_at = None;
        let backend = FakeBackend::new(vec![Ok(Some(open)), Ok(Some(fixture.status.clone()))]);
        let first = crate::watch::tick(
            &fixture.h,
            &backend,
            &fixture.cfg,
            &Notifier::new(&fixture.h, true),
            &mut state,
        );
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert!(
            !fixture
                .h
                .fake
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains("merged while blocked")),
            "the workspace status pass handled a second terminal observation"
        );
        for _ in 0..3 {
            let backend = FakeBackend::new(vec![
                Ok(Some(fixture.status.clone())),
                Ok(Some(fixture.status.clone())),
            ]);
            let report = crate::watch::tick(
                &fixture.h,
                &backend,
                &fixture.cfg,
                &Notifier::new(&fixture.h, true),
                &mut state,
            );
            assert!(report.errors.is_empty(), "{:?}", report.errors);
        }
        let calls = fixture.h.fake.calls.borrow();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.starts_with("notify ") && call.contains("merged"))
                .count(),
            1
        );
        assert!(fixture.path.exists());
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
        merged_children_case(true);
    }

    #[test]
    fn merged_clean_removes_workspace_free_children() {
        merged_children_case(false);
    }

    fn merged_children_case(legacy: bool) {
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
        if legacy {
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
        }
        fixture
            .store
            .update(|data| {
                data.tickets[0].children.push(crate::store::ChildWorktree {
                    unit: 1,
                    branch: "feat/t--1".into(),
                    path: child.clone(),
                    workspace_id: legacy.then(|| "child".into()),
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
            if legacy {
                vec!["remove_worktree child false", "remove_worktree w1 false"]
            } else {
                vec!["remove_worktree w1 false"]
            }
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
    fn terminal_pr_handling_drains_nonrevision_admission() {
        let fixture = Fixture::new(TicketState::Building);
        fixture.agent();
        let store = fixture.store.clone();
        let agent = store.update(|data| Ok(data.agents.remove(0))).unwrap();
        let admission = crate::ticket::operation::shared(&store, "T1").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let outcome = fixture.run();
            finished_tx.send(()).unwrap();
            (fixture, outcome)
        });
        started_rx.recv().unwrap();
        let early = finished_rx.recv_timeout(Duration::from_millis(200)).is_ok();
        store
            .update(|data| {
                data.agents.push(agent);
                Ok(())
            })
            .unwrap();
        drop(admission);
        let (fixture, outcome) = worker.join().unwrap();
        assert!(
            !early,
            "terminal handling skipped an admitted nonrevision launch"
        );
        assert_eq!(outcome.unwrap(), MergeOutcome::CleanedUp);
        assert_eq!(
            fixture.store.read().unwrap().agents[0].status,
            AgentStatus::Retired
        );
        assert!(fixture.h.fake.agent_records.borrow().is_empty());
        assert!(!fixture.path.exists());
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
