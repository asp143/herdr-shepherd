//! Synchronize and run a revision without creating another pull request.

use std::path::Path;

use crate::{
    error::ShepherdError,
    gate::Gate,
    store::{Ticket, TicketState},
};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct RevisionRun {
    pub base_sha: String,
    #[serde(default)]
    pub synced_sha: Option<String>,
    pub stage: RevisionStage,
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub tested_sha: Option<String>,
}

/// A durable reference to a single dispatch, including fallback attempts.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct AssignmentRef {
    pub agent: String,
    pub task_id: Option<String>,
    pub attempt_id: Option<String>,
}

impl AssignmentRef {
    pub fn from_agent(agent: &crate::store::AgentRecord) -> Self {
        Self {
            agent: agent.name.clone(),
            task_id: agent.task_id.clone(),
            attempt_id: agent.attempt_id.clone(),
        }
    }

    pub fn matches(&self, agent: &crate::store::AgentRecord) -> bool {
        self.agent == agent.name
            && self.task_id == agent.task_id
            && self.attempt_id == agent.attempt_id
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct CiRecovery {
    pub generation: String,
    pub assignment: AssignmentRef,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RevisionStage {
    Coder,
    Reviewer,
    Qa,
    Push,
}

#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RevisionOutcome {
    Waiting,
    Started { agent: String, role: String },
    Pushed { sha: String },
    Blocked,
}

pub fn ci_retry_generation(ticket: &Ticket) -> String {
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

/// Evidence retained when a legacy dispatch needs an explicit decision.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct LegacyDispatch {
    workflow: String,
    role: String,
    marker: String,
    pending_key: String,
    candidates: Vec<String>,
    #[serde(default)]
    discarded: bool,
}

// Exclude state/transition clocks: blocking and a public resume must preserve
// the original CI marker, which the baseline derived from those clocks.
fn legacy_pending_key(ticket: &Ticket, workflow: &str) -> String {
    serde_json::json!({
        "workflow": workflow, "ticket": ticket.id, "branch": ticket.branch,
        "base": ticket.base, "checkout": ticket.worktree, "pr": ticket.pr_number,
        "round": ticket.revision_round,
        "task": if workflow == "revision" { &ticket.revision_task } else { &ticket.ci_failure_task },
        "run": if workflow == "revision" { ticket.revision_run.as_ref() } else { None },
        "comments": if workflow == "revision" { accepted_revision_comments(ticket) } else { String::new() },
        "ci_head": if workflow == "CI recovery" { ticket.ci_head_sha.as_ref() } else { None },
    }).to_string()
}

// Only quarantine/resume transitions may be peeled back. A real workflow
// transition (especially CiWait -> Building) starts a new CI generation, even
// when a loop-cap reset lets its task text and round recur.
fn legacy_ci_markers(ticket: &Ticket) -> Vec<String> {
    let mut previous = ticket.clone();
    let mut markers = vec![ci_retry_generation(ticket)];
    loop {
        let history = &previous.history;
        let Some(last) = history.last() else {
            break;
        };
        if last.from == TicketState::Blocked && last.to == TicketState::Building {
            if !history.iter().rev().nth(1).is_some_and(|entry| {
                entry.from == TicketState::Building
                    && entry.to == TicketState::Blocked
                    && entry.reason == Some(crate::store::BlockedReason::Escalated)
            }) {
                break;
            }
            previous.history.pop();
        }
        if !previous.history.last().is_some_and(|entry| {
            entry.from == TicketState::Building
                && entry.to == TicketState::Blocked
                && entry.reason == Some(crate::store::BlockedReason::Escalated)
        }) {
            break;
        }
        previous.history.pop();
        previous.updated_at = previous
            .history
            .last()
            .map(|entry| &entry.at)
            .unwrap_or(&previous.created_at)
            .clone();
        markers.push(ci_retry_generation(&previous));
    }
    markers
}

fn legacy_ci_task_matches(ticket: &Ticket, task: &str) -> bool {
    let Some((marker, saved_task)) = task.split_once('\n') else {
        return false;
    };
    let prefix = format!(
        "CI retry {} round {} task ",
        ticket.id, ticket.revision_round
    );
    let Some(digest) = marker.strip_prefix(&prefix) else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && ticket.ci_failure_task.as_deref() == Some(saved_task)
}

fn legacy_ci_verified(ticket: &Ticket, agent: &crate::store::AgentRecord, marker: &str) -> bool {
    let saved = agent.task.lines().next().unwrap_or_default();
    saved == marker || legacy_ci_markers(ticket).iter().any(|known| known == saved)
}

fn legacy_candidates<'a>(
    ticket: &Ticket,
    agents: &'a [crate::store::AgentRecord],
    role: &str,
    workflow: &str,
    marker: &str,
) -> Vec<&'a crate::store::AgentRecord> {
    let checkout = ticket
        .worktree
        .as_ref()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()));
    agents
        .iter()
        .filter(|agent| {
            agent.ticket == ticket.id
                && agent.role == role
                && agent.unit.is_none()
                && agent.task_id.is_none()
                && agent.attempt_id.is_none()
                && agent
                    .report
                    .as_ref()
                    .is_some_and(|report| report.task_id.is_none())
                && agent
                    .completed_tasks
                    .iter()
                    .all(|task| task.task_id.is_none() && task.attempt_id.is_none())
                && matches!(
                    agent.status,
                    crate::store::AgentStatus::Reported | crate::store::AgentStatus::Retired
                )
                && if workflow == "CI recovery" {
                    legacy_ci_task_matches(ticket, &agent.task)
                        && (legacy_ci_verified(ticket, agent, marker)
                            || ticket.legacy_dispatch.as_ref().is_some_and(|receipt| {
                                receipt.workflow == workflow
                                    && receipt.pending_key == legacy_pending_key(ticket, workflow)
                                    && receipt.candidates.contains(&agent.name)
                            })
                            || (legacy_ci_markers(ticket).len() > 1
                                && agent.dispatch_reason.as_deref().is_some_and(|reason| {
                                    reason.starts_with(
                                        "blocked: unbound legacy dispatch for CI recovery coder:",
                                    )
                                })))
                } else {
                    agent.task.lines().next() == Some(marker)
                        && agent.task.contains(&format!(
                            "\nTask: {}\nAccepted comments:\n",
                            ticket.revision_task.as_deref().unwrap_or_default()
                        ))
                        && agent.task.contains(&format!(
                            "\nAccepted comments:\n{}\nNew diff:",
                            accepted_revision_comments(ticket)
                        ))
                }
                && checkout.as_ref().is_some_and(|path| {
                    agent
                        .worktree
                        .canonicalize()
                        .unwrap_or_else(|_| agent.worktree.clone())
                        == *path
                })
        })
        .collect()
}

fn bind_legacy_report(
    ticket: &mut Ticket,
    workflow: &str,
    marker: &str,
    assignment: AssignmentRef,
) {
    if workflow == "revision" {
        let run = ticket
            .revision_run
            .as_mut()
            .expect("validated legacy revision");
        run.agent = Some(assignment.agent);
        run.task_id = assignment.task_id;
        run.attempt_id = assignment.attempt_id;
    } else {
        ticket.ci_recovery = Some(CiRecovery {
            generation: marker.into(),
            assignment,
        });
        ticket.ci_failure_task = None;
    }
    ticket.legacy_dispatch = None;
}

/// Recover the baseline's separate launch/reference writes using its exact
/// task marker. One matching completion binds automatically; multiple matches
/// retain a validated receipt for a public operator decision.
pub fn reconcile_unbound_legacy_dispatch(
    ticket: &mut Ticket,
    agents: &mut [crate::store::AgentRecord],
    role: &str,
    workflow: &str,
    now: &str,
    max_revision_rounds: u32,
) -> Result<Option<String>, ShepherdError> {
    let pending_key = legacy_pending_key(ticket, workflow);
    let previous = ticket.legacy_dispatch.as_ref().filter(|receipt| {
        receipt.workflow == workflow && receipt.role == role && receipt.pending_key == pending_key
    });
    if previous.is_some_and(|receipt| receipt.discarded) {
        return Ok(None);
    }
    let marker = if let Some(receipt) = previous {
        receipt.marker.clone()
    } else {
        match workflow {
            "revision" => match &ticket.revision_run {
                Some(run) => run_marker(run),
                None => return Ok(None),
            },
            "CI recovery" => ci_retry_generation(ticket),
            _ => return Err(ShepherdError::Usage("unknown legacy workflow".into())),
        }
    };
    let recipients = legacy_candidates(ticket, agents, role, workflow, &marker);
    if recipients.is_empty() {
        return Ok(None);
    }
    if let [agent] = recipients.as_slice()
        && (workflow != "CI recovery" || legacy_ci_verified(ticket, agent, &marker))
    {
        let assignment = AssignmentRef::from_agent(agent);
        bind_legacy_report(ticket, workflow, &marker, assignment);
        return Ok(None);
    }
    let candidates: Vec<_> = recipients.iter().map(|agent| agent.name.clone()).collect();
    let message = format!(
        "unbound legacy dispatch for {workflow} {role}: {}; inspect the saved reports, then run `ticket reconcile {} --accept-report <agent>` or `ticket reconcile {} --discard-reports`; no replacement task was started",
        candidates.join(", "),
        ticket.id,
        ticket.id
    );
    ticket.legacy_dispatch = Some(LegacyDispatch {
        workflow: workflow.into(),
        role: role.into(),
        marker,
        pending_key,
        candidates: candidates.clone(),
        discarded: false,
    });
    for agent in agents
        .iter_mut()
        .filter(|agent| candidates.contains(&agent.name))
    {
        agent.dispatch_reason = Some(format!("blocked: {message}"));
    }
    super::state::transition_with_limit(
        ticket,
        TicketState::Blocked,
        Some(crate::store::BlockedReason::Escalated),
        now,
        max_revision_rounds,
    )?;
    Ok(Some(message))
}

/// Resolve only the exact quarantined workflow. Reports remain immutable even
/// when discarded; the receipt authorizes one redispatch without accepting them.
pub fn reconcile_legacy_reports(
    store: &crate::store::Store,
    cfg: &crate::config::Config,
    id: &str,
    accept_report: Option<&str>,
    discard_reports: bool,
    now: &str,
) -> Result<Ticket, ShepherdError> {
    let _dispatch = dispatch_lock(store, id)?;
    let _operation = super::operation::exclusive(store, id)?;
    let max_rounds = crate::roles::resolve(cfg)?.shepherd.max_revision_rounds;
    store.update(|data| {
        let invalid =
            || ShepherdError::Usage("ticket has no matching legacy quarantine to reconcile".into());
        if accept_report.is_some() == discard_reports {
            return Err(ShepherdError::Usage(
                "choose --accept-report NAME or --discard-reports".into(),
            ));
        }
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(invalid)?;
        let receipt = ticket.legacy_dispatch.clone().ok_or_else(invalid)?;
        let target = match receipt.workflow.as_str() {
            "revision"
                if ticket
                    .revision_run
                    .as_ref()
                    .is_some_and(|run| run.agent.is_none() && run.stage != RevisionStage::Push) =>
            {
                TicketState::Revising
            }
            "CI recovery" if ticket.ci_recovery.is_none() && ticket.ci_failure_task.is_some() => {
                TicketState::Building
            }
            _ => return Err(invalid()),
        };
        if ticket.state != TicketState::Blocked
            || ticket.blocked_reason != Some(crate::store::BlockedReason::Escalated)
            || receipt.discarded
            || receipt.pending_key != legacy_pending_key(ticket, &receipt.workflow)
            || data.agents.iter().any(|agent| {
                agent.ticket == id
                    && matches!(
                        agent.status,
                        crate::store::AgentStatus::Starting
                            | crate::store::AgentStatus::Working
                            | crate::store::AgentStatus::Blocked
                    )
            })
        {
            return Err(invalid());
        }
        let candidates = legacy_candidates(
            ticket,
            &data.agents,
            &receipt.role,
            &receipt.workflow,
            &receipt.marker,
        );
        if candidates
            .iter()
            .map(|agent| &agent.name)
            .collect::<Vec<_>>()
            != receipt.candidates.iter().collect::<Vec<_>>()
        {
            return Err(invalid());
        }
        if let Some(name) = accept_report {
            let agent = candidates
                .iter()
                .find(|agent| agent.name == name)
                .ok_or_else(|| {
                    ShepherdError::Usage(
                        "--accept-report must name a quarantined legacy report".into(),
                    )
                })?;
            bind_legacy_report(
                ticket,
                &receipt.workflow,
                &receipt.marker,
                AssignmentRef::from_agent(agent),
            );
        } else {
            ticket
                .legacy_dispatch
                .as_mut()
                .expect("validated receipt")
                .discarded = true;
        }
        super::state::transition_with_limit(ticket, target, None, now, max_rounds)?;
        Ok(ticket.clone())
    })
}

/// Keep revision launch and terminal PR retirement mutually exclusive.
/// The separate lock permits agent launch to acquire the project state lock.
pub fn dispatch_lock(
    store: &crate::store::Store,
    id: &str,
) -> Result<std::fs::File, ShepherdError> {
    std::fs::create_dir_all(store.root()).map_err(anyhow::Error::from)?;
    let identity = crate::store::project_hash(Path::new(id));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.root().join(format!("revision-{identity}.lock")))
        .map_err(anyhow::Error::from)?;
    lock.lock().map_err(anyhow::Error::from)?;
    Ok(lock)
}

/// Advance one revision stage. Repeated calls wait for the current report.
/// The ticket stays `revising` until its existing PR branch is pushed.
pub fn resume_revision(
    h: &dyn crate::herdr::Herdr,
    gh: &dyn crate::github::PrBackend,
    store: &crate::store::Store,
    cfg: &crate::config::Config,
    gate: &Gate,
    id: &str,
    now: &str,
) -> Result<RevisionOutcome, ShepherdError> {
    use crate::store::{AgentStatus, BlockedReason};
    let _hooks = crate::hooks::defer();
    let _lock = dispatch_lock(store, id)?;
    let settings = crate::roles::resolve(cfg)?.shepherd;
    let initial = store
        .read()?
        .tickets
        .into_iter()
        .find(|ticket| ticket.id == id)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
    if initial.state != TicketState::Revising {
        return Err(ShepherdError::Usage("ticket must be revising".into()));
    }
    // Transport reads trusted state itself. Keep it outside state updates to
    // avoid recursively acquiring the project lock; dispatch_lock serializes
    // revision work with terminal PR handling.
    let preparation = (|| {
        validate_gate(&initial, gate)?;
        let base_head = verified_head(gate)?;
        if initial.revision_run.is_none() {
            if initial
                .revision_task
                .as_deref()
                .is_none_or(|task| task.trim().is_empty())
            {
                return Err(ShepherdError::Usage(
                    "revision task must not be empty".into(),
                ));
            }
            prepare_revision(&initial, gate)?;
        }
        Ok((base_head, verified_head(gate)?))
    })();
    let prepared = store.update(|data| {
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        if ticket.state != TicketState::Revising {
            return Err(ShepherdError::Usage("ticket must be revising".into()));
        }
        let (base_head, observed_head) = match preparation {
            Ok(heads) => heads,
            Err(error) if error.code() == "conflict" => {
                super::state::transition_with_limit(
                    ticket,
                    TicketState::Blocked,
                    Some(BlockedReason::Conflict),
                    now,
                    settings.max_revision_rounds,
                )?;
                return Ok(Err(error));
            }
            Err(error) => return Err(error),
        };
        if ticket.revision_run.is_none() {
            ticket.revision_run = Some(RevisionRun {
                base_sha: base_head,
                synced_sha: Some(observed_head.clone()),
                stage: RevisionStage::Coder,
                agent: None,
                task_id: None,
                attempt_id: None,
                tested_sha: None,
            });
        }
        let run = ticket.revision_run.as_ref().expect("initialized run");
        if initial.revision_run.is_some() && run.agent.is_none() && run.stage != RevisionStage::Push
        {
            let role = match run.stage {
                RevisionStage::Coder => "coder",
                RevisionStage::Reviewer => "reviewer",
                RevisionStage::Qa => "qa",
                RevisionStage::Push => unreachable!(),
            };
            if let Some(message) = reconcile_unbound_legacy_dispatch(
                ticket,
                &mut data.agents,
                role,
                "revision",
                now,
                settings.max_revision_rounds,
            )? {
                return Ok(Err(ShepherdError::Usage(message)));
            }
        }
        let run = ticket.revision_run.as_mut().expect("initialized run");
        if let Some(name) = &run.agent {
            let agent = data
                .agents
                .iter_mut()
                .find(|agent| &agent.name == name)
                .ok_or_else(|| ShepherdError::Usage("revision agent record disappeared".into()))?;
            // Legacy runs may bind their named, original assignment once. Never
            // relabel a reused record or recover a launch from prompt prose.
            if run.task_id.is_none() && run.attempt_id.is_none() && agent.completed_tasks.len() <= 1
            {
                run.task_id = agent.task_id.clone();
                run.attempt_id = agent.attempt_id.clone();
            }
            if run.task_id != agent.task_id || run.attempt_id != agent.attempt_id {
                return Ok(Ok(None));
            }
            match agent.status {
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked => {
                    return Ok(Ok(None));
                }
                AgentStatus::Reported | AgentStatus::Retired if agent.report.is_some() => {
                    let report = agent.report.as_ref().ok_or_else(|| {
                        ShepherdError::Usage("revision agent has no report".into())
                    })?;
                    if report.task_id != run.task_id
                        || !agent.completed_tasks.iter().any(|completed| {
                            completed.task_id == run.task_id
                                && completed.attempt_id == run.attempt_id
                                && &completed.report == report
                        })
                    {
                        // Legacy records have no task tokens/history; their original
                        // named assignment remains valid, but cannot be reused.
                        if run.task_id.is_some()
                            || run.attempt_id.is_some()
                            || !agent.completed_tasks.is_empty()
                        {
                            return Ok(Ok(None));
                        }
                    }
                    run.agent = None;
                    run.task_id = None;
                    run.attempt_id = None;
                    let rejected = report.status != "completed"
                        || !report.blockers.is_empty()
                        || report
                            .findings
                            .iter()
                            .any(|finding| finding.severity == crate::store::Severity::High);
                    if rejected {
                        if run.stage == RevisionStage::Coder || report.status == "blocked" {
                            super::state::transition_with_limit(
                                ticket,
                                TicketState::Blocked,
                                Some(BlockedReason::Escalated),
                                now,
                                settings.max_revision_rounds,
                            )?;
                            return Ok(Ok(None));
                        }
                        let next_round = ticket.revision_round.checked_add(1);
                        ticket.revision_round = next_round.unwrap_or(u32::MAX);
                        if next_round.is_none()
                            || ticket.revision_round > settings.max_revision_rounds
                        {
                            super::state::transition_with_limit(
                                ticket,
                                TicketState::Blocked,
                                Some(BlockedReason::LoopCap),
                                now,
                                settings.max_revision_rounds,
                            )?;
                            return Ok(Ok(None));
                        }
                        ticket
                            .revision_task
                            .get_or_insert_default()
                            .push_str(&format!(
                                "\nRevision feedback from {}: {}\nBlockers: {:?}\nFindings: {:?}",
                                agent.role, report.summary, report.blockers, report.findings
                            ));
                        run.stage = RevisionStage::Coder;
                    } else {
                        let head = observed_head.clone();
                        if report.commit_sha.as_ref().is_some_and(|sha| sha != &head)
                            || (run.stage != RevisionStage::Coder
                                && run.tested_sha.as_ref() != Some(&head))
                        {
                            super::state::transition_with_limit(
                                ticket,
                                TicketState::Blocked,
                                Some(BlockedReason::Conflict),
                                now,
                                settings.max_revision_rounds,
                            )?;
                            return Ok(Ok(None));
                        }
                        if run.stage == RevisionStage::Coder {
                            run.tested_sha = Some(head);
                        }
                        run.stage = match run.stage {
                            RevisionStage::Coder => RevisionStage::Reviewer,
                            RevisionStage::Reviewer => RevisionStage::Qa,
                            RevisionStage::Qa => RevisionStage::Push,
                            RevisionStage::Push => {
                                return Err(ShepherdError::Usage(
                                    "push stage cannot have an agent".into(),
                                ));
                            }
                        };
                    }
                }
                AgentStatus::Reported => return Ok(Ok(None)),
                AgentStatus::Retired | AgentStatus::LaunchFailed => {
                    run.agent = None;
                    run.task_id = None;
                    run.attempt_id = None;
                }
            }
        }
        if data.agents.iter().any(|agent| {
            agent.ticket == id
                && matches!(
                    agent.status,
                    AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                )
        }) {
            return Ok(Ok(None));
        }
        Ok(Ok(Some(ticket.clone())))
    })?;
    let Some(ticket) = prepared? else {
        return Ok(
            if store
                .read()?
                .tickets
                .iter()
                .any(|ticket| ticket.id == id && ticket.state == TicketState::Blocked)
            {
                RevisionOutcome::Blocked
            } else {
                RevisionOutcome::Waiting
            },
        );
    };
    let run = ticket.revision_run.as_ref().expect("initialized run");
    if run.stage == RevisionStage::Push {
        return finish_revision(h, gh, store, cfg, gate, id, now);
    }
    let role = match run.stage {
        RevisionStage::Coder => "coder",
        RevisionStage::Reviewer => "reviewer",
        RevisionStage::Qa => "qa",
        RevisionStage::Push => unreachable!(),
    };
    let expected_run = run.clone();
    let task = revision_task(&ticket, run, role);
    let resolved = crate::roles::resolve(cfg)?;
    let role_config = resolved
        .roles
        .iter()
        .find(|candidate| candidate.name == role)
        .cloned()
        .ok_or_else(|| ShepherdError::Usage(format!("unknown role: {role}")))?;
    let runners = role_config
        .runners
        .iter()
        .map(|name| resolved.runners[name].clone())
        .collect();
    let agent = crate::flock::spawn_with_reservation(
        h,
        store,
        cfg,
        crate::launch::LaunchRequest {
            ticket: id.into(),
            role: role_config,
            runners,
            worktree: gate.worktree.clone(),
            workspace_id: ticket
                .workspace_id
                .clone()
                .ok_or_else(|| ShepherdError::Usage("ticket has no workspace".into()))?,
            tab_label: format!("revision {role}"),
            prompt: task,
            state_dir: gate.state_dir.clone(),
        },
        &|data, agent| {
            let ticket = data
                .tickets
                .iter_mut()
                .find(|ticket| ticket.id == id && ticket.state == TicketState::Revising)
                .ok_or_else(|| ShepherdError::Usage("revision changed before dispatch".into()))?;
            let run = ticket
                .revision_run
                .as_mut()
                .ok_or_else(|| ShepherdError::Usage("revision disappeared".into()))?;
            // The callback can run again when proven rejection transfers this
            // same task to a replacement attempt.
            if run != &expected_run
                && (run.task_id != agent.task_id || run.stage != expected_run.stage)
            {
                return Err(ShepherdError::Usage("revision assignment changed".into()));
            }
            run.agent = Some(agent.name.clone());
            run.task_id = agent.task_id.clone();
            run.attempt_id = agent.attempt_id.clone();
            ticket.legacy_dispatch = None;
            Ok(())
        },
    )?;
    if agent.status == AgentStatus::Blocked && agent.report.is_none() {
        return Ok(RevisionOutcome::Blocked);
    }
    Ok(RevisionOutcome::Started {
        agent: agent.name,
        role: role.into(),
    })
}

fn finish_revision(
    h: &dyn crate::herdr::Herdr,
    gh: &dyn crate::github::PrBackend,
    store: &crate::store::Store,
    cfg: &crate::config::Config,
    gate: &Gate,
    id: &str,
    now: &str,
) -> Result<RevisionOutcome, ShepherdError> {
    let initial = store
        .read()?
        .tickets
        .into_iter()
        .find(|ticket| ticket.id == id)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
    if initial.state != TicketState::Revising
        || initial
            .revision_run
            .as_ref()
            .is_none_or(|run| run.stage != RevisionStage::Push)
    {
        return Err(ShepherdError::Usage("revision is not ready to push".into()));
    }
    validate_gate(&initial, gate)?;
    let head = verified_head(gate)?;
    if initial
        .revision_run
        .as_ref()
        .and_then(|run| run.tested_sha.as_ref())
        != Some(&head)
        || !crate::gate::transport::PushTransport::open(&gate.state_dir, &gate.worktree)?
            .worktree_status(&initial.branch)?
            .is_empty()
    {
        store.update(|data| {
            let ticket = data
                .tickets
                .iter_mut()
                .find(|ticket| ticket.id == id)
                .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
            super::state::transition(
                ticket,
                TicketState::Blocked,
                Some(crate::store::BlockedReason::Conflict),
                now,
            )
        })?;
        return Ok(RevisionOutcome::Blocked);
    }
    let status = gh
        .view(&gate.worktree, &initial.branch)?
        .ok_or_else(|| ShepherdError::GithubUnavailable("revision PR is missing".into()))?;
    if status.state != crate::github::PrState::Open
        || initial.pr_number != Some(status.number)
        || status.head != initial.branch
        || status.base != initial.base
    {
        return Err(ShepherdError::PolicyDenied(
            "revision PR is no longer open on the recorded branch and base".into(),
        ));
    }
    let synced_sha = initial
        .revision_run
        .as_ref()
        .and_then(|run| run.synced_sha.as_deref());
    let remote_tip = crate::gate::transport::PushTransport::open(&gate.state_dir, &gate.worktree)?
        .remote_tip(&initial.branch)?;
    let already_pushed = remote_tip.as_deref() == Some(head.as_str());
    if !already_pushed && (synced_sha.is_none() || remote_tip.as_deref() != synced_sha) {
        return block_push_conflict(
            h,
            store,
            cfg,
            id,
            now,
            "remote branch changed after revision sync",
        );
    }
    // The dispatch lock prevents merge handling from retiring this revision.
    // The gated transport must be able to acquire its own shared store lock.
    if !already_pushed && let Err(error) = gh.push_expected(gate, &head) {
        let moved = crate::gate::transport::PushTransport::open(&gate.state_dir, &gate.worktree)?
            .remote_tip(&initial.branch)
            .is_ok_and(|tip| tip.as_deref() != synced_sha);
        let rejection = error.to_string().to_ascii_lowercase();
        if moved
            || error.code() == "conflict"
            || rejection.contains("non-fast-forward")
            || rejection.contains("fetch first")
            || rejection.contains("remote contains work")
        {
            return block_push_conflict(
                h,
                store,
                cfg,
                id,
                now,
                "the ticket branch changed or rejected the tested revision",
            );
        }
        return Err(error);
    }
    let outcome = store.update(|data| {
        let ticket = data.tickets.iter_mut().find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        super::state::transition(ticket, TicketState::CiWait, None, now)?;
        ticket.ci_head_sha = Some(head.clone());
        ticket.ci_none_since = None;
        let mut notifications = Vec::new();
        for comment in ticket.comments.iter_mut().filter(|comment| comment.verdict.as_deref() == Some("accepted") && !comment.acknowledged) {
            comment.ack_sha = Some(head.clone());
            notifications.push(format!("{id}: run ticket ack {id} --comment {} --body-file <path>; explain the change and include commit {head}", comment.id));
        }
        ticket.revision_run = None;
        ticket.revision_task = None;
        for notification in notifications {
            data.enqueue_lead_notice(&notification, Some(id), None);
        }
        Ok(RevisionOutcome::Pushed { sha: head })
    })?;
    if let RevisionOutcome::Pushed { sha } = &outcome {
        crate::hooks::emit(
            cfg,
            "ticket.revised",
            &serde_json::json!({"ticket":id,"sha":sha}),
        );
    }
    Ok(outcome)
}

fn block_push_conflict(
    h: &dyn crate::herdr::Herdr,
    store: &crate::store::Store,
    cfg: &crate::config::Config,
    id: &str,
    now: &str,
    reason: &str,
) -> Result<RevisionOutcome, ShepherdError> {
    let message = format!("{id}: revision blocked (conflict): {reason}");
    store.update(|data| {
        let ticket = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id)
            .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
        super::state::transition(
            ticket,
            TicketState::Blocked,
            Some(crate::store::BlockedReason::Conflict),
            now,
        )?;
        data.enqueue_lead_notice(&message, Some(id), None);
        Ok(())
    })?;
    crate::notify::Notifier::new(h, cfg.upkeep.notifications).send(&message, "");
    Ok(RevisionOutcome::Blocked)
}

fn run_marker(run: &RevisionRun) -> String {
    format!("Revision {} {:?}", run.base_sha, run.stage)
}

fn accepted_revision_comments(ticket: &Ticket) -> String {
    ticket
        .comments
        .iter()
        .filter(|comment| comment.verdict.as_deref() == Some("accepted") && !comment.acknowledged)
        .map(|comment| {
            format!(
                "Comment {} by {} at {:?}:{:?}: {}",
                comment.id, comment.author, comment.path, comment.line, comment.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn revision_task(ticket: &Ticket, run: &RevisionRun, role: &str) -> String {
    let comments = accepted_revision_comments(ticket);
    let action = match role {
        "coder" => {
            "Implement the revision task and accepted comments. Commit the changes and run relevant tests."
        }
        "reviewer" => {
            "Review the new diff and accepted comments against the actual code. Reproduce concerns where possible. Label findings High/Mid/Low/Wish."
        }
        "qa" => {
            "Test the new diff and every ticket acceptance criterion. Report failures as blockers."
        }
        _ => unreachable!(),
    };
    format!(
        "{}\nTicket {}: {}\n{}\nTask: {}\nAccepted comments:\n{}\nNew diff: {}..HEAD\nAcceptance: {:?}\n{}\nCode changes must be test-first, scoped to this task, and committed on the assigned branch. Use conventional commits, type(scope): imperative subject under 50 characters, with a body explaining why. Do not add AI attribution or Co-Authored-By trailers. Reviewers and QA should inspect and test the complete revision diff and report concrete evidence; delegate code fixes back through the shepherd.\nStay on branch {} in worktree {}. Never switch branches or write outside it. Never read .env files, Credentials/ folders, *.pem, *.key, or other secret files. Never push, open PRs, or call GitHub. {}",
        run_marker(run),
        ticket.id,
        ticket.title,
        ticket.body,
        ticket.revision_task.as_deref().unwrap_or_default(),
        comments,
        run.base_sha,
        ticket.acceptance,
        format_args!(
            "{action}\n{}",
            crate::prompts::verification_instructions(
                role,
                &run.base_sha,
                run.tested_sha.as_deref().unwrap_or(&run.base_sha)
            )
        ),
        ticket.branch,
        ticket
            .worktree
            .as_deref()
            .unwrap_or(Path::new("."))
            .display(),
        crate::prompts::report_instructions(
            &std::env::current_exe().unwrap_or_else(|_| "herdr-shepherd".into()),
            ticket.worktree.as_deref().unwrap_or(Path::new(".")),
            "{{agent_name}}",
            "{{task_id}}",
        )
    )
}

fn conflict(reason: impl Into<String>) -> ShepherdError {
    ShepherdError::Conflict {
        unit: 0,
        range: reason.into(),
    }
}

pub fn prepare_revision(ticket: &Ticket, gate: &Gate) -> Result<(), ShepherdError> {
    if ticket.state != TicketState::Revising {
        return Err(ShepherdError::Usage("ticket must be revising".into()));
    }
    validate_gate(ticket, gate)?;
    let path = &gate.worktree;
    let transport = crate::gate::transport::PushTransport::open(&gate.state_dir, path)?;
    if !transport.worktree_status(&ticket.branch)?.is_empty() {
        return Err(conflict("ticket worktree contains uncommitted changes"));
    }
    let head = transport.verified_ticket_head(&ticket.branch)?;
    let synced = transport.fetch_ticket(&ticket.id, &ticket.branch)?;
    if !transport.is_ancestor(&head, &synced)? {
        return Err(conflict("remote branch diverged from the ticket"));
    }
    transport.fast_forward_ticket(&ticket.branch, &head, &synced)?;
    Ok(())
}

fn validate_gate(ticket: &Ticket, gate: &Gate) -> Result<(), ShepherdError> {
    let path = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    if path.canonicalize().map_err(anyhow::Error::from)?
        != gate.worktree.canonicalize().map_err(anyhow::Error::from)?
        || ticket.branch != gate.ticket_branch
    {
        return Err(ShepherdError::PolicyDenied(
            "revision gate differs from ticket worktree or branch".into(),
        ));
    }
    crate::gate::check_push(
        &crate::gate::parse_push_args(&["origin", &ticket.branch]),
        &ticket.branch,
        &gate.policy,
    )?;
    verified_head(gate)?;
    Ok(())
}

fn verified_head(gate: &Gate) -> Result<String, ShepherdError> {
    crate::gate::transport::PushTransport::open(&gate.state_dir, &gate.worktree)?
        .verified_ticket_head(&gate.ticket_branch)
}

#[cfg(test)]
mod assignment_tests {
    use super::*;

    #[test]
    fn legacy_ci_correlation_requires_exact_task_and_round() {
        let mut ticket = Ticket {
            id: "T1".into(),
            revision_round: 2,
            ci_failure_task: Some("Fix check A\nDetails".into()),
            ..Default::default()
        };
        let task = format!(
            "{}\n{}",
            ci_retry_generation(&ticket),
            ticket.ci_failure_task.as_deref().unwrap()
        );
        assert!(legacy_ci_task_matches(&ticket, &task));
        // Public block/resume transitions change clocks, but not the task.
        ticket.updated_at = "resumed".into();
        assert!(legacy_ci_task_matches(&ticket, &task));
        ticket.revision_round += 1;
        assert!(!legacy_ci_task_matches(&ticket, &task));
        ticket.revision_round -= 1;
        ticket.ci_failure_task = Some("Fix check B".into());
        assert!(!legacy_ci_task_matches(&ticket, &task));
        assert!(!legacy_ci_task_matches(
            &ticket,
            "CI retry T1 round 2 task not-a-digest\nFix check B"
        ));
    }

    #[test]
    fn revision_prompt_binds_report_to_assignment_file_and_token() {
        let ticket = Ticket {
            worktree: Some("/ticket checkout".into()),
            ..Default::default()
        };
        let run = RevisionRun {
            base_sha: "base".into(),
            synced_sha: None,
            stage: RevisionStage::Coder,
            agent: None,
            task_id: None,
            attempt_id: None,
            tested_sha: None,
        };
        let prompt = revision_task(&ticket, &run, "coder");
        assert!(prompt.contains("--task-id '{{task_id}}'"));
        assert!(prompt.contains(".shepherd-report-{{agent_name}}-{{task_id}}.json"));
        assert!(prompt.contains("\"task_id\":\"{{task_id}}\""));
        assert!(!prompt.contains("--report-file <path>"));
    }
}
