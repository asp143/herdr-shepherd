use herdr_shepherd::{
    config::Config,
    error::ShepherdError,
    herdr::Herdr,
    launch, roles,
    store::{AgentStatus, BlockedReason, Store, TicketState},
    ticket::{intake, scheduler, state::transition_with_limit},
};
use std::path::Path;

pub fn dispatch(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    state: &Path,
    config_dir: &Path,
) -> Result<serde_json::Value, ShepherdError> {
    let resolved = roles::resolve(cfg)?;
    let actions = scheduler::next_actions(&store.read()?, &resolved.shepherd);
    let mut started = Vec::new();
    let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
    for action in actions {
        let (ticket, role, task) = match action {
            scheduler::Action::StartTicket(id) => {
                let ticket = intake::start_ticket(
                    h,
                    store,
                    &id,
                    &intake::StartTicketOptions {
                        state_dir: state,
                        bin: &bin,
                        settings: &resolved.shepherd,
                        now: &launch::now(),
                    },
                )?;
                herdr_shepherd::gate::hook::install_configured(
                    ticket.worktree.as_deref().expect("setup worktree"),
                    &ticket.branch,
                    state,
                    &bin,
                    config_dir,
                )?;
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
                let ticket = store.update(|data| {
                    let t = data
                        .tickets
                        .iter_mut()
                        .find(|t| t.id == id)
                        .ok_or_else(|| ShepherdError::Usage("ticket disappeared".into()))?;
                    transition_with_limit(
                        t,
                        TicketState::Building,
                        None,
                        &launch::now(),
                        resolved.shepherd.max_revision_rounds,
                    )?;
                    Ok(t.clone())
                })?;
                (
                    ticket,
                    "coder",
                    "Revise the implementation using the ticket findings and latest plan. Commit the fix, run relevant checks and report.",
                )
            }
        };
        let rec = crate::cli_flock::spawn_task(h, store, cfg, state, &ticket, role, task)?;
        started.push(serde_json::json!({"ticket":ticket.id,"agent":rec}));
    }
    Ok(serde_json::json!({"started":started}))
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
