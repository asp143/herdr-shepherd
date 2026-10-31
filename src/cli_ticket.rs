use herdr_shepherd::{
    config::Config,
    error::ShepherdError,
    herdr::Herdr,
    launch, roles,
    store::{AgentStatus, BlockedReason, Store, TicketState},
    ticket::{intake, scheduler, state::transition_with_limit},
};
use std::path::Path;

fn ci_retry_marker(ticket: &herdr_shepherd::store::Ticket) -> String {
    use sha2::{Digest, Sha256};
    let identity = format!(
        "{}\0{}\0{}",
        ticket.ci_failure_task.as_deref().unwrap_or_default(),
        ticket.updated_at,
        ticket.history.len(),
    );
    format!(
        "CI retry {} round {} task {:x}",
        ticket.id,
        ticket.revision_round,
        Sha256::digest(identity.as_bytes()),
    )
}

#[cfg(test)]
pub fn dispatch(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    state: &Path,
    config_dir: &Path,
) -> Result<serde_json::Value, ShepherdError> {
    dispatch_with_backend(
        h,
        &herdr_shepherd::github::fake::FakeBackend::new(vec![]),
        store,
        cfg,
        state,
        config_dir,
    )
}

pub fn dispatch_with_backend(
    h: &dyn Herdr,
    gh: &dyn herdr_shepherd::github::PrBackend,
    store: &Store,
    cfg: &Config,
    state: &Path,
    config_dir: &Path,
) -> Result<serde_json::Value, ShepherdError> {
    let resolved = roles::resolve(cfg)?;
    let actions = scheduler::next_actions(&store.read()?, &resolved.shepherd);
    let mut started = Vec::new();
    let mut deferred = Vec::new();
    let mut errors = Vec::new();
    let mut results = Vec::new();
    let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
    for action in actions {
        let id = match &action {
            scheduler::Action::StartTicket(id)
            | scheduler::Action::ResumeRevising(id)
            | scheduler::Action::ResumeCiBuild(id) => id.clone(),
        };
        let active = store
            .read()?
            .agents
            .iter()
            .filter(|agent| {
                agent.role != "lead"
                    && matches!(
                        agent.status,
                        AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                    )
            })
            .count();
        if active >= resolved.shepherd.max_parallel_agents {
            let result = serde_json::json!({"ticket":id,"status":"deferred","code":"capacity"});
            deferred.push(result.clone());
            results.push(result);
            continue;
        }
        let mut entered_stage = None;
        let attempt = (|| {
            let ci_build = matches!(&action, scheduler::Action::ResumeCiBuild(_));
            let _ci_dispatch = if ci_build {
                Some(herdr_shepherd::ticket::revise::dispatch_lock(store, &id)?)
            } else {
                None
            };
            let (ticket, role, task) = match action {
                scheduler::Action::StartTicket(id) => {
                    let ticket = intake::start_ticket_with_backend(
                        h,
                        gh,
                        store,
                        &id,
                        &intake::StartTicketOptions {
                            state_dir: state,
                            config_dir,
                            bin: &bin,
                            settings: &resolved.shepherd,
                            now: &launch::now(),
                        },
                        cfg.upkeep.notifications,
                    )?;
                    entered_stage = Some(ticket.state);
                    herdr_shepherd::hooks::emit(
                        cfg,
                        "ticket.started",
                        &serde_json::json!({"ticket":ticket.id}),
                    );
                    (
                        ticket,
                        "planner",
                        "Create a concise plan with independent units, dependencies, risks and acceptance checks. Report the plan in summary; do not implement code.",
                    )
                }
                scheduler::Action::ResumeRevising(id) => {
                    let ticket = store
                        .read()?
                        .tickets
                        .into_iter()
                        .find(|ticket| ticket.id == id)
                        .ok_or_else(|| ShepherdError::Usage("ticket disappeared".into()))?;
                    let path = ticket
                        .worktree
                        .as_deref()
                        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
                    let mut gate = crate::cli_pr::gate(
                        path,
                        cfg,
                        Some(&ticket.base),
                        Some(&ticket.branch),
                        state,
                    )?;
                    gate.config_dir = config_dir.into();
                    entered_stage = Some(TicketState::Revising);
                    let outcome =
                        scheduler::resume_revising(h, gh, store, cfg, &gate, &id, &launch::now())?;
                    if let herdr_shepherd::ticket::revise::RevisionOutcome::Started {
                        agent, ..
                    } = &outcome
                        && store.read()?.agents.iter().any(|rec| {
                            rec.name == *agent && rec.status == AgentStatus::LaunchFailed
                        })
                    {
                        return Err(ShepherdError::Runtime(anyhow::anyhow!(
                            "all configured runners failed to launch"
                        )));
                    }
                    started.push(serde_json::json!({"ticket":id,"revision":outcome}));
                    return Ok(None);
                }
                scheduler::Action::ResumeCiBuild(id) => {
                    let data = store.read()?;
                    let ticket = data
                        .tickets
                        .iter()
                        .find(|ticket| ticket.id == id && ticket.state == TicketState::Building)
                        .ok_or_else(|| ShepherdError::Usage("CI retry no longer building".into()))?
                        .clone();
                    if ticket.ci_failure_task.is_none() {
                        return Ok(None);
                    }
                    let marker = ci_retry_marker(&ticket);
                    if data.agents.iter().any(|agent| {
                        agent.ticket == id
                            && agent.status != AgentStatus::LaunchFailed
                            && agent.task.contains(&marker)
                    }) {
                        store.update(|data| {
                            if let Some(current) =
                                data.tickets.iter_mut().find(|current| current.id == id)
                            {
                                current.ci_failure_task = None;
                            }
                            Ok(())
                        })?;
                        return Ok(None);
                    }
                    entered_stage = Some(TicketState::Building);
                    (ticket, "coder", "")
                }
            };
            let task = if ci_build {
                format!(
                    "{}\n{}",
                    ci_retry_marker(&ticket),
                    ticket.ci_failure_task.as_deref().unwrap_or(task)
                )
            } else {
                task.to_owned()
            };
            let rec = crate::cli_flock::spawn_task(h, store, cfg, state, &ticket, role, &task)?;
            if rec.status == AgentStatus::LaunchFailed {
                return Err(ShepherdError::Runtime(anyhow::anyhow!(
                    "all configured runners failed to launch"
                )));
            }
            if ci_build {
                store.update(|data| {
                    if let Some(current) = data.tickets.iter_mut().find(|current| current.id == id)
                    {
                        current.ci_failure_task = None;
                    }
                    Ok(())
                })?;
            }
            Ok(Some(rec))
        })();
        match attempt {
            Ok(None) => {}
            Ok(Some(rec)) => {
                started.push(serde_json::json!({"ticket":id,"agent":rec}));
                results.push(serde_json::json!({"ticket":id,"status":"started","agent":rec.name}));
            }
            Err(error) => {
                let blocked = store.update(|data| {
                    let Some(ticket) = data.tickets.iter_mut().find(|t| t.id == id) else {
                        return Ok(false);
                    };
                    if entered_stage == Some(ticket.state)
                        && herdr_shepherd::ticket::state::is_active(ticket.state)
                    {
                        transition_with_limit(
                            ticket,
                            TicketState::Blocked,
                            Some(BlockedReason::LaunchFailed),
                            &launch::now(),
                            resolved.shepherd.max_revision_rounds,
                        )?;
                    }
                    Ok(ticket.state == TicketState::Blocked)
                })?;
                let result = serde_json::json!({"ticket":id,"status":if blocked {"blocked"} else {"failed"},"code":error.code(),"message":error.to_string()});
                errors.push(result.clone());
                results.push(result);
                if blocked {
                    herdr_shepherd::hooks::emit(
                        cfg,
                        "ticket.blocked",
                        &serde_json::json!({"ticket":id,"reason":"launch_failed"}),
                    );
                }
            }
        }
    }
    Ok(serde_json::json!({"started":started,"deferred":deferred,"errors":errors,"results":results}))
}

pub fn cancel(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    id: &str,
) -> Result<herdr_shepherd::store::Ticket, ShepherdError> {
    for rec in store.read()?.agents.into_iter().filter(|a| {
        a.ticket == id && !matches!(a.status, AgentStatus::Retired | AgentStatus::LaunchFailed)
    }) {
        herdr_shepherd::flock::cancel(h, store, &rec.name, false)?;
    }
    let resolved = roles::resolve(cfg)?;
    let ticket = intake::cancel(
        store,
        id,
        &launch::now(),
        resolved.shepherd.max_revision_rounds,
    )?;
    herdr_shepherd::hooks::emit(cfg, "ticket.closed", &serde_json::json!({"ticket":id}));
    Ok(ticket)
}

pub fn transition(
    store: &Store,
    cfg: &Config,
    id: &str,
    to: TicketState,
    reason: Option<BlockedReason>,
) -> Result<herdr_shepherd::store::Ticket, ShepherdError> {
    let cap = roles::resolve(cfg)?.shepherd.max_revision_rounds;
    let ticket = store.update(|data| {
        if data.agents.iter().any(|a| {
            a.ticket == id
                && matches!(
                    a.status,
                    AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                )
        }) {
            return Err(ShepherdError::Usage(
                "wait for active agents to report or cancel them before changing stage".into(),
            ));
        }
        let t = data
            .tickets
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        transition_with_limit(t, to, reason, &launch::now(), cap)?;
        Ok(t.clone())
    })?;
    let event = if ticket.state == TicketState::Blocked {
        "ticket.blocked"
    } else {
        "ticket.stage_changed"
    };
    herdr_shepherd::hooks::emit(
        cfg,
        event,
        &serde_json::json!({"ticket":id,"state":ticket.state,"reason":ticket.blocked_reason}),
    );
    Ok(ticket)
}

pub fn integrate(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    id: &str,
) -> Result<herdr_shepherd::ticket::children::IntegrateReport, ShepherdError> {
    use herdr_shepherd::ticket::children;
    let data = store.read()?;
    if data.agents.iter().any(|a| {
        a.ticket == id
            && matches!(
                a.status,
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
            )
    }) {
        return Err(ShepherdError::Usage(
            "wait for coders to report before integrating".into(),
        ));
    }
    let mut ticket = data
        .tickets
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
    let path = ticket
        .worktree
        .clone()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    if crate::cli_pr::git(&path, &["branch", "--show-current"])? != ticket.branch {
        return Err(ShepherdError::PolicyDenied(
            "ticket worktree changed branch".into(),
        ));
    }
    let ranges = children::conflict_ranges(&ticket.children);
    let result = children::integrate(&path, &mut ticket.children);
    match result {
        Err(error @ ShepherdError::Conflict { .. }) => {
            let cap = roles::resolve(cfg)?.shepherd.max_revision_rounds;
            store.update(|data| {
                let t = data
                    .tickets
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| ShepherdError::Usage("ticket disappeared".into()))?;
                t.integration_ranges = ranges;
                transition_with_limit(
                    t,
                    TicketState::Blocked,
                    Some(BlockedReason::Conflict),
                    &launch::now(),
                    cap,
                )
            })?;
            herdr_shepherd::hooks::emit(
                cfg,
                "ticket.blocked",
                &serde_json::json!({"ticket":id,"reason":"conflict"}),
            );
            herdr_shepherd::flock::ping(
                h,
                store,
                &format!("{id}: child integration conflict; both ranges retained"),
            );
            herdr_shepherd::notify::Notifier::new(h, cfg.upkeep.notifications)
                .send(&format!("{id}: integration blocked"), &error.to_string());
            Err(error)
        }
        Err(error) => Err(error),
        Ok(report) => {
            store.update(|data| {
                let t = data
                    .tickets
                    .iter_mut()
                    .find(|t| t.id == id)
                    .ok_or_else(|| ShepherdError::Usage("ticket disappeared".into()))?;
                t.children = ticket.children.clone();
                t.integration_ranges = report.ranges.clone();
                Ok(())
            })?;
            for child in &ticket.children {
                for rec in store.read()?.agents.into_iter().filter(|a| {
                    a.ticket == id && a.worktree == child.path && a.status == AgentStatus::Reported
                }) {
                    herdr_shepherd::flock::cancel(h, store, &rec.name, false)?;
                }
                children::cleanup_integrated(h, &path, std::slice::from_ref(child))?;
                store.update(|data| {
                    if let Some(t) = data.tickets.iter_mut().find(|t| t.id == id) {
                        t.children.retain(|c| c.unit != child.unit);
                    }
                    Ok(())
                })?;
            }
            Ok(report)
        }
    }
}

#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use herdr_shepherd::{
        config,
        herdr::{AgentStart, fake::FakeHerdr},
        store::{AgentRecord, Ticket},
    };
    use std::{fs, path::PathBuf, process::Command};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        state: PathBuf,
        config_dir: PathBuf,
        store: Store,
        cfg: Config,
        h: FakeHerdr,
    }
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
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    impl Fixture {
        fn new(max_agents: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("repo");
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
            let state = dir.path().join("state");
            let config_dir = dir.path().join("config");
            fs::create_dir(&config_dir).unwrap();
            fs::write(
                config_dir.join("shepherd.toml"),
                format!("[shepherd]\nmax_parallel_agents={max_agents}\nmax_concurrent_tickets=4\n"),
            )
            .unwrap();
            let cfg = config::load(&config_dir, None).unwrap();
            let store = Store::open(&state, &root).unwrap();
            herdr_shepherd::trusted::ensure_snapshot(&state, &root).unwrap();
            Self {
                _dir: dir,
                root,
                state,
                config_dir,
                store,
                cfg,
                h: FakeHerdr::new(vec![]),
            }
        }
        fn ticket(&self, id: &str, state: TicketState) {
            let branch = format!("feat/{id}");
            let path = self
                .state
                .join("worktrees")
                .join(herdr_shepherd::store::project_hash(&self.root))
                .join(branch.replace('/', "--"));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            git(
                &self.root,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_str().unwrap(),
                    "main",
                ],
            );
            let head = git(&path, &["rev-parse", "HEAD"]);
            self.store
                .update(|d| {
                    d.tickets.push(Ticket {
                        id: id.into(),
                        slug: id.into(),
                        title: id.into(),
                        branch,
                        base: "main".into(),
                        state,
                        created_at: id.into(),
                        pr_number: (state == TicketState::Revising).then_some(7),
                        revision_task: (state == TicketState::Revising)
                            .then(|| "Apply review feedback".into()),
                        revision_run: (state == TicketState::Revising).then_some(
                            herdr_shepherd::ticket::revise::RevisionRun {
                                synced_sha: Some(head.clone()),
                                base_sha: head,
                                stage: herdr_shepherd::ticket::revise::RevisionStage::Coder,
                                agent: None,
                                tested_sha: None,
                            },
                        ),
                        worktree: (state != TicketState::Queued).then_some(path),
                        workspace_id: (state != TicketState::Queued)
                            .then(|| format!("prepared-{id}")),
                        ..Ticket::default()
                    });
                    Ok(())
                })
                .unwrap();
        }
        fn occupy_agent_slot(&self) {
            self.ticket("busy", TicketState::Building);
            let data = self.store.read().unwrap();
            let ticket = data.tickets.iter().find(|t| t.id == "busy").unwrap();
            self.store
                .update(|d| {
                    d.agents.push(AgentRecord {
                        name: "sh-fixture-coder-1".into(),
                        ticket: "busy".into(),
                        role: "coder".into(),
                        runner: "codex-sol-high".into(),
                        status: AgentStatus::Working,
                        pane_id: Some("busy-pane".into()),
                        tab_id: Some("busy-tab".into()),
                        worktree: ticket.worktree.clone().unwrap(),
                        task: "busy".into(),
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
        }
        fn dispatch(&self) -> serde_json::Value {
            dispatch(
                &self.h,
                &self.store,
                &self.cfg,
                &self.state,
                &self.config_dir,
            )
            .unwrap()
        }
        fn state(&self, id: &str) -> TicketState {
            self.store
                .read()
                .unwrap()
                .tickets
                .into_iter()
                .find(|t| t.id == id)
                .unwrap()
                .state
        }
    }
    #[test]
    fn dispatch_recovers_ci_launch_before_task_clear() {
        let f = Fixture::new(1);
        f.ticket("ci", TicketState::Building);
        let task = "Fix unit: https://example.invalid/check/1";
        f.store
            .update(|data| {
                data.tickets[0].pr_number = Some(7);
                data.tickets[0].ci_failure_task = Some(task.into());
                Ok(())
            })
            .unwrap();
        f.dispatch();
        f.store
            .update(|data| {
                // A launcher can persist the agent before its caller clears the
                // task. The completed report must recover that same launch.
                data.tickets[0].ci_failure_task = Some(task.into());
                data.agents[0].status = AgentStatus::Reported;
                Ok(())
            })
            .unwrap();
        f.dispatch();
        let data = f.store.read().unwrap();
        assert_eq!(data.agents.len(), 1);
        assert!(data.tickets[0].ci_failure_task.is_none());
        f.store
            .update(|data| {
                data.tickets[0].revision_round = 1;
                data.tickets[0].ci_failure_task = Some(task.into());
                Ok(())
            })
            .unwrap();
        f.dispatch();
        assert_eq!(f.store.read().unwrap().agents.len(), 2);
    }

    #[test]
    fn dispatch_starts_ci_fix_once_on_existing_ticket() {
        let f = Fixture::new(1);
        f.ticket("ci", TicketState::Building);
        f.store
            .update(|data| {
                data.tickets[0].pr_number = Some(7);
                data.tickets[0].ci_failure_task =
                    Some("Fix unit: https://example.invalid/check/1".into());
                Ok(())
            })
            .unwrap();
        let report = f.dispatch();
        assert_eq!(report["started"].as_array().unwrap().len(), 1);
        let data = f.store.read().unwrap();
        assert_eq!(data.tickets[0].state, TicketState::Building);
        assert_eq!(data.tickets[0].pr_number, Some(7));
        assert!(data.tickets[0].ci_failure_task.is_none());
        assert_eq!(data.agents[0].role, "coder");
        assert!(
            data.agents[0]
                .task
                .contains("https://example.invalid/check/1")
        );
        assert!(f.dispatch()["started"].as_array().unwrap().is_empty());
    }

    #[test]
    fn dispatch_defers_queued_and_revising_before_capacity_transition() {
        let f = Fixture::new(1);
        f.occupy_agent_slot();
        f.ticket("q", TicketState::Queued);
        f.ticket("r", TicketState::Revising);
        let report = f.dispatch();
        assert_eq!(f.state("q"), TicketState::Queued);
        assert_eq!(f.state("r"), TicketState::Revising);
        assert_eq!(report["deferred"].as_array().unwrap().len(), 2);
        assert!(f.h.calls.borrow().is_empty());
    }
    #[test]
    fn dispatch_blocks_launch_failed_tickets_and_continues_queue() {
        for state in [TicketState::Queued, TicketState::Revising] {
            let f = Fixture::new(2);
            f.ticket("a", state);
            f.ticket("z", TicketState::Queued);
            f.h.start_results
                .borrow_mut()
                .extend([AgentStart::NotReady, AgentStart::Ready]);
            f.h.visible_screens
                .borrow_mut()
                .push_back("rate limit exceeded".into());
            let report = f.dispatch();
            let data = f.store.read().unwrap();
            let failed = data.tickets.iter().find(|t| t.id == "a").unwrap();
            assert_eq!(failed.state, TicketState::Blocked);
            assert_eq!(failed.blocked_reason, Some(BlockedReason::LaunchFailed));
            assert_eq!(f.state("z"), TicketState::Planning);
            assert_eq!(report["started"].as_array().unwrap().len(), 1);
            assert_eq!(report["errors"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn dispatch_blocks_runtime_failure_and_starts_later_ticket() {
        let mut f = Fixture::new(2);
        f.ticket("a", TicketState::Revising);
        f.ticket("z", TicketState::Queued);
        f.cfg.raw="[shepherd]\nmax_parallel_agents=2\nmax_concurrent_tickets=4\n[roles.coder]\nrunners=['broken']\n[runners.broken]\nharness='unsupported-test-harness'\nargs=[]".parse().unwrap();
        let report = f.dispatch();
        assert_eq!(f.state("a"), TicketState::Blocked);
        assert_eq!(f.state("z"), TicketState::Planning);
        assert_eq!(report["errors"][0]["code"], "config_invalid");
        assert_eq!(report["started"].as_array().unwrap().len(), 1);
    }

    struct CapacityDuringCreate<'a> {
        h: &'a FakeHerdr,
        store: Store,
        root: PathBuf,
    }
    impl Herdr for CapacityDuringCreate<'_> {
        fn create_worktree(
            &self,
            repo: &Path,
            branch: &str,
            label: &str,
            base: &str,
            path: &Path,
        ) -> Result<herdr_shepherd::herdr::Workspace, ShepherdError> {
            self.store.update(|data| {
                data.tickets.push(Ticket {
                    id: "busy".into(),
                    state: TicketState::Building,
                    ..Ticket::default()
                });
                data.agents.push(AgentRecord {
                    name: "sh-concurrent-coder-1".into(),
                    ticket: "busy".into(),
                    role: "coder".into(),
                    runner: "codex-sol-high".into(),
                    status: AgentStatus::Working,
                    pane_id: Some("concurrent-pane".into()),
                    tab_id: Some("concurrent-tab".into()),
                    worktree: self.root.clone(),
                    task: "busy".into(),
                    report: None,
                    observed: None,
                    last_progress_at: "0".into(),
                    nudges: 0,
                    restarts: 0,
                    prompt_pending: false,
                });
                Ok(())
            })?;
            self.h.create_worktree(repo, branch, label, base, path)
        }
        fn workspaces(&self) -> Result<Vec<herdr_shepherd::herdr::Workspace>, ShepherdError> {
            self.h.workspaces()
        }
        fn agents(&self) -> Result<Vec<herdr_shepherd::herdr::AgentInfo>, ShepherdError> {
            self.h.agents()
        }
        fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
            self.h.rename_workspace(id, label)
        }
        fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError> {
            self.h.move_block(ids, before)
        }
        fn report_workspace_tokens(
            &self,
            id: &str,
            set: &[(String, String)],
            clear: &[String],
        ) -> Result<(), ShepherdError> {
            self.h.report_workspace_tokens(id, set, clear)
        }
        fn report_pane_tokens(
            &self,
            id: &str,
            set: &[(String, String)],
            clear: &[String],
        ) -> Result<(), ShepherdError> {
            self.h.report_pane_tokens(id, set, clear)
        }
        fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError> {
            self.h.notify(title, body)
        }
        fn remove_worktree(&self, id: &str, force: bool) -> Result<(), ShepherdError> {
            self.h.remove_worktree(id, force)
        }
        fn close_workspace(&self, id: &str) -> Result<(), ShepherdError> {
            self.h.close_workspace(id)
        }
    }
    #[test]
    fn dispatch_blocks_capacity_race_and_reports_later_action() {
        let f = Fixture::new(1);
        f.ticket("a", TicketState::Queued);
        f.ticket("z", TicketState::Queued);
        let h = CapacityDuringCreate {
            h: &f.h,
            store: f.store.clone(),
            root: f.root.clone(),
        };
        let report = dispatch(&h, &f.store, &f.cfg, &f.state, &f.config_dir).unwrap();
        assert_eq!(f.state("a"), TicketState::Blocked);
        assert_eq!(f.state("z"), TicketState::Queued);
        assert_eq!(report["errors"][0]["code"], "capacity");
        assert_eq!(report["deferred"][0]["ticket"], "z");
        assert_eq!(report["results"].as_array().unwrap().len(), 2);
    }
}
