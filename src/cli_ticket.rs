use herdr_shepherd::{
    config::Config,
    error::ShepherdError,
    herdr::Herdr,
    launch, roles,
    store::{AgentStatus, BlockedReason, Store, TicketState},
    ticket::{intake, scheduler, state::transition_with_limit},
};
use std::path::Path;

use herdr_shepherd::ticket::revise::ci_retry_generation;

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
            let _hooks = ci_build.then(herdr_shepherd::hooks::defer);
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
                    // Establish placement before launching tabs; optional Herdr
                    // events may be unavailable. Reuse their setting and lock.
                    // Upkeep failure must not discard a successfully owned ticket.
                    if let Err(error) = herdr_shepherd::upkeep::event::handle_event_in_state(
                        "worktree.created",
                        &serde_json::json!({"workspace_id": ticket.workspace_id}),
                        cfg,
                        h,
                        state,
                    ) {
                        eprintln!("shepherd: ticket workspace regroup warning: {error}");
                    }
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
                    if matches!(
                        outcome,
                        herdr_shepherd::ticket::revise::RevisionOutcome::Waiting
                            | herdr_shepherd::ticket::revise::RevisionOutcome::Blocked
                    ) {
                        deferred.push(serde_json::json!({"ticket":id,"revision":outcome}));
                    } else {
                        started.push(serde_json::json!({"ticket":id,"revision":outcome}));
                    }
                    return Ok(None);
                }
                scheduler::Action::ResumeCiBuild(id) => {
                    let mut data = store.read()?;
                    let mut ticket = data
                        .tickets
                        .iter()
                        .find(|ticket| ticket.id == id && ticket.state == TicketState::Building)
                        .ok_or_else(|| ShepherdError::Usage("CI retry no longer building".into()))?
                        .clone();
                    if ticket.ci_failure_task.is_none() {
                        return Ok(None);
                    }
                    if ticket.ci_recovery.is_none() {
                        let blocked = store.update(|data| {
                            let current = data
                                .tickets
                                .iter_mut()
                                .find(|current| {
                                    current.id == id && current.state == TicketState::Building
                                })
                                .ok_or_else(|| {
                                    ShepherdError::Usage(
                                        "CI retry changed before reconciliation".into(),
                                    )
                                })?;
                            if current.ci_recovery.is_some() {
                                return Ok(None);
                            }
                            herdr_shepherd::ticket::revise::reconcile_unbound_legacy_dispatch(
                                current,
                                &mut data.agents,
                                "coder",
                                "CI recovery",
                                &launch::now(),
                                resolved.shepherd.max_revision_rounds,
                            )
                        })?;
                        if let Some(message) = blocked {
                            return Err(ShepherdError::Usage(message));
                        }
                        // Legacy reconciliation may have consumed the completed
                        // CI task. Never dispatch from the pre-recovery snapshot.
                        data = store.read()?;
                        ticket = data
                            .tickets
                            .iter()
                            .find(|ticket| ticket.id == id)
                            .ok_or_else(|| ShepherdError::Usage("CI ticket disappeared".into()))?
                            .clone();
                        if ticket.ci_failure_task.is_none() {
                            return Ok(None);
                        }
                    }
                    let generation = ci_retry_generation(&ticket);
                    if let Some(recovery) = &ticket.ci_recovery
                        && recovery.generation == generation
                        && let Some(agent) = data
                            .agents
                            .iter()
                            .find(|agent| recovery.assignment.matches(agent))
                        && agent.status != AgentStatus::LaunchFailed
                        && (agent.status != AgentStatus::Retired || agent.report.is_some())
                    {
                        if agent.status == AgentStatus::Blocked && agent.report.is_none() {
                            return Ok(None);
                        }
                        store.update(|data| {
                            if let Some(current) = data.tickets.iter_mut().find(|current| {
                                current.id == id && current.ci_recovery == ticket.ci_recovery
                            }) {
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
                ticket.ci_failure_task.as_deref().unwrap_or(task).to_owned()
            } else {
                task.to_owned()
            };
            let generation = ci_retry_generation(&ticket);
            let rec = crate::cli_flock::spawn_task_with_reservation(
                h,
                store,
                cfg,
                crate::cli_flock::TaskRequest {
                    state,
                    ticket: &ticket,
                    role_name: role,
                    task: &task,
                    tab_label: None,
                },
                &|data, agent| {
                    if ci_build {
                        let current = data
                            .tickets
                            .iter_mut()
                            .find(|current| {
                                current.id == id && current.state == TicketState::Building
                            })
                            .ok_or_else(|| {
                                ShepherdError::Usage("CI recovery changed before dispatch".into())
                            })?;
                        if ci_retry_generation(current) != generation {
                            return Err(ShepherdError::Usage(
                                "CI generation changed before dispatch".into(),
                            ));
                        }
                        current.legacy_dispatch = None;
                        current.ci_recovery = Some(herdr_shepherd::ticket::revise::CiRecovery {
                            generation: generation.clone(),
                            assignment: herdr_shepherd::ticket::revise::AssignmentRef::from_agent(
                                agent,
                            ),
                        });
                    }
                    Ok(())
                },
            )?;
            if rec.status == AgentStatus::Blocked && rec.report.is_none() {
                deferred.push(serde_json::json!({"ticket":id,"status":"blocked","reason":rec.dispatch_reason}));
                return Ok(None);
            }
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
            // Nested launch scopes cannot flush the CI retry's outer queue.
            // Release dispatch and deliver hooks before reading public status.
            drop(_ci_dispatch);
            drop(_hooks);
            let rec = if ci_build {
                store
                    .read()?
                    .agents
                    .into_iter()
                    .find(|agent| agent.name == rec.name)
                    .ok_or_else(|| ShepherdError::Usage("launch record disappeared".into()))?
            } else {
                rec
            };
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
    let resolved = roles::resolve(cfg)?;
    let ticket = intake::cancel_agents_with_config(
        h,
        store,
        id,
        &launch::now(),
        resolved.shepherd.max_revision_rounds,
        cfg,
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
    let dispatch = herdr_shepherd::ticket::revise::dispatch_lock(store, id)?;
    let operation = herdr_shepherd::ticket::operation::exclusive(store, id)?;
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
    // Hooks may reenter ticket commands after the transition is durable.
    drop(operation);
    drop(dispatch);
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
    let _hooks = herdr_shepherd::hooks::defer();
    let _dispatch = herdr_shepherd::ticket::revise::dispatch_lock(store, id)?;
    let _operation = herdr_shepherd::ticket::operation::exclusive(store, id)?;
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
    // Final cleanup can own an unintegrated (including empty) child. Changing
    // that identity would invalidate its durable receipt and strand both retries.
    if ticket
        .child_cleanup
        .iter()
        .any(|receipt| !receipt.child.integrated || !ticket.children.contains(&receipt.child))
    {
        return Err(ShepherdError::Usage(
            "finish the outstanding cleanup before integrating this ticket".into(),
        ));
    }
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
            herdr_shepherd::flock::ping_notice(
                h,
                store,
                &format!("{id}: child integration conflict; both ranges retained"),
                Some(id),
                None,
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
                // Update only the identities whose Git integration completed.
                // Retain any independently registered or recovered children.
                for integrated in &ticket.children {
                    if let Some(current) = t.children.iter_mut().find(|current| {
                        current.unit == integrated.unit
                            && current.path == integrated.path
                            && current.branch == integrated.branch
                            && current.base_sha == integrated.base_sha
                            && current.workspace_id == integrated.workspace_id
                    }) {
                        current.integrated = integrated.integrated;
                    }
                }
                t.integration_ranges = report.ranges.clone();
                Ok(())
            })?;
            children::cleanup_integrated_recorded_with_config(h, store, &ticket, cfg)?;
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
                                task_id: None,
                                attempt_id: None,
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
    fn unrelated_legacy_history_does_not_quarantine_ci() {
        for status in [AgentStatus::Reported, AgentStatus::Retired] {
            let f = Fixture::new(2);
            f.ticket("ci", TicketState::Building);
            // Pre-upgrade completed implementation, before the first CI task.
            f.occupy_agent_slot();
            f.store
                .update(|data| {
                    let checkout = data.tickets[0].worktree.clone().unwrap();
                    data.tickets[0].ci_failure_task = Some("Fix new CI failure".into());
                    data.tickets.retain(|ticket| ticket.id == "ci");
                    let agent = &mut data.agents[0];
                    agent.ticket = "ci".into();
                    agent.worktree = checkout;
                    agent.status = status;
                    agent.task = "Original implementation".into();
                    agent.report = Some(
                        serde_json::from_value(serde_json::json!({
                            "status":"completed", "summary":"old work", "tests":[],
                            "changed_paths":[], "blockers":[], "findings":[], "commit_sha":null
                        }))
                        .unwrap(),
                    );
                    Ok(())
                })
                .unwrap();
            let result = f.dispatch();
            assert!(result["errors"].as_array().unwrap().is_empty(), "{result}");
            assert_eq!(f.store.read().unwrap().agents.len(), 2);
            assert_eq!(f.h.start_args.borrow().len(), 1);
            assert_eq!(f.state("ci"), TicketState::Building);
        }
    }

    #[test]
    fn interrupted_legacy_ci_never_repeats_completed_coder() {
        let f = Fixture::new(2);
        f.ticket("ci", TicketState::Building);
        let task = "Fix the legacy CI failure";
        f.store
            .update(|data| {
                data.tickets[0].ci_failure_task = Some(task.into());
                Ok(())
            })
            .unwrap();
        f.dispatch();
        let name = f.store.read().unwrap().agents[0].name.clone();
        f.store
            .update(|data| {
                data.tickets[0].ci_failure_task = Some(task.into());
                data.tickets[0].ci_recovery = None;
                let marker = ci_retry_generation(&data.tickets[0]);
                let agent = &mut data.agents[0];
                agent.task = format!("{marker}\n{task}");
                agent.task_id = None;
                agent.attempt_id = None;
                agent.launch_profile = None;
                agent.completed_tasks.clear();
                agent.dispatch_reason = None;
                Ok(())
            })
            .unwrap();
        herdr_shepherd::flock::store_report(&f.store, &name, r#"{"status":"completed","summary":"legacy CI fixed","tests":["fixture"],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        f.store
            .update(|data| {
                data.agents[0].completed_tasks.clear();
                Ok(())
            })
            .unwrap();
        let result = f.dispatch();
        let data = f.store.read().unwrap();
        assert_eq!(
            data.agents.len(),
            1,
            "must not repeat a completed legacy CI dispatch: {result}"
        );
        assert_eq!(f.h.start_args.borrow().len(), 1);
        assert_eq!(data.tickets[0].state, TicketState::Building);
        assert!(data.tickets[0].ci_failure_task.is_none());
        assert!(result["errors"].as_array().unwrap().is_empty(), "{result}");
        assert_eq!(
            data.tickets[0]
                .ci_recovery
                .as_ref()
                .unwrap()
                .assignment
                .agent,
            name
        );
        assert!(f.dispatch()["started"].as_array().unwrap().is_empty());
        assert_eq!(f.store.read().unwrap().agents.len(), 1);
    }

    #[test]
    fn cancelled_ci_reservation_does_not_consume_pending_task() {
        let f = Fixture::new(2);
        f.ticket("ci", TicketState::Building);
        f.store
            .update(|data| {
                data.tickets[0].ci_failure_task = Some("Fix CI".into());
                Ok(())
            })
            .unwrap();
        f.dispatch();
        let rec = f.store.read().unwrap().agents[0].clone();
        // Simulate interruption after launch reservation but before the caller
        // cleared its intent, followed by explicit cancellation.
        f.store
            .update(|data| {
                data.tickets[0].ci_failure_task = Some("Fix CI".into());
                Ok(())
            })
            .unwrap();
        herdr_shepherd::flock::cancel(&f.h, &f.store, &rec.name, false).unwrap();
        f.dispatch();
        let data = f.store.read().unwrap();
        assert_eq!(
            data.agents.len(),
            2,
            "cancelled assignment did not accept the pending task"
        );
        assert_eq!(data.agents[1].status, AgentStatus::Working);
        let recovery = data.tickets[0].ci_recovery.as_ref().unwrap();
        assert!(recovery.assignment.matches(&data.agents[1]));
    }

    #[test]
    fn waiting_revision_is_deferred_not_started() {
        let f = Fixture::new(2);
        f.ticket("revision", TicketState::Revising);
        f.dispatch();
        let rec = f.store.read().unwrap().agents[0].clone();
        herdr_shepherd::flock::store_report_for_task(&f.store, &rec.name, rec.task_id.as_deref(), r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        f.store
            .update(|data| {
                data.tickets[0].revision_run.as_mut().unwrap().task_id =
                    Some("stale-dispatch".into());
                Ok(())
            })
            .unwrap();
        let result = f.dispatch();
        assert!(
            result["started"].as_array().unwrap().is_empty(),
            "waiting revision was not accepted"
        );
        assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
        assert_eq!(f.store.read().unwrap().agents.len(), 1);
    }

    #[test]
    fn ci_ambiguous_reuse_keeps_pending_task() {
        let f = Fixture::new(2);
        f.ticket("ci", TicketState::Building);
        f.store
            .update(|d| {
                d.tickets[0].ci_failure_task = Some("Fix CI".into());
                Ok(())
            })
            .unwrap();
        f.dispatch();
        let rec = f.store.read().unwrap().agents[0].clone();
        f.h.pane_sessions
            .borrow_mut()
            .insert(rec.pane_id.clone().unwrap(), "session".into());
        f.h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        f.store
            .update(|d| {
                let mut observed: serde_json::Value =
                    serde_json::from_str(d.agents[0].observed.as_deref().unwrap()).unwrap();
                observed["session_ref"] = "session".into();
                d.agents[0].observed = Some(observed.to_string());
                d.tickets[0].revision_round += 1;
                d.tickets[0].ci_failure_task = Some("Fix CI again".into());
                Ok(())
            })
            .unwrap();
        herdr_shepherd::flock::store_report_for_task(&f.store,&rec.name,rec.task_id.as_deref(),r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        f.h.prompt_results.borrow_mut().push_back((
            true,
            Err(ShepherdError::HerdrCommand {
                code: "timeout".into(),
                message: "accepted but timed out".into(),
            }),
        ));
        let result = f.dispatch();
        let data = f.store.read().unwrap();
        assert!(
            data.tickets[0].ci_failure_task.is_some(),
            "ambiguous delivery is still pending"
        );
        assert_eq!(data.agents.len(), 1);
        assert_eq!(data.agents[0].status, AgentStatus::Blocked);
        assert!(
            result["started"].as_array().unwrap().is_empty(),
            "ambiguous is not accepted"
        );
        f.dispatch();
        assert_eq!(f.store.read().unwrap().agents.len(), 1);
    }

    #[test]
    fn repeated_ci_failures_reuse_coder_with_fresh_assignment() {
        let f = Fixture::new(2);
        f.ticket("ci", TicketState::Building);
        let mut prior = None;
        for round in 0..3 {
            f.store
                .update(|data| {
                    data.tickets[0].revision_round = round;
                    data.tickets[0].ci_failure_task = Some("Fix repeated failure".into());
                    Ok(())
                })
                .unwrap();
            f.dispatch();
            let rec = f.store.read().unwrap().agents.last().unwrap().clone();
            if let Some((name, task)) = prior {
                assert_eq!(rec.name, name, "CI must reuse the same cohort coder");
                assert_ne!(rec.task_id, task);
            }
            f.h.pane_sessions
                .borrow_mut()
                .insert(rec.pane_id.clone().unwrap(), "ci-session".into());
            f.h.agent_records.borrow_mut()[0].agent_status = "idle".into();
            f.store
                .update(|data| {
                    let agent = data.agents.iter_mut().find(|a| a.name == rec.name).unwrap();
                    let mut observed: serde_json::Value =
                        serde_json::from_str(agent.observed.as_deref().unwrap()).unwrap();
                    observed["session_ref"] = "ci-session".into();
                    agent.observed = Some(observed.to_string());
                    Ok(())
                })
                .unwrap();
            herdr_shepherd::flock::store_report_for_task(&f.store, &rec.name, rec.task_id.as_deref(), r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
            prior = Some((rec.name, rec.task_id));
        }
        assert_eq!(f.store.read().unwrap().agents.len(), 1);
        assert_eq!(f.h.start_args.borrow().len(), 1);
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
                    task_id: None,
                    attempt_id: None,
                    unit: None,
                    launch_profile: None,
                    completed_tasks: vec![],
                    dispatch_reason: None,
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

#[cfg(test)]
#[path = "ticket_layout_tests.rs"]
mod layout_tests;
