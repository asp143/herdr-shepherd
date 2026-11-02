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

/// An old launcher saved agents before binding its workflow reference. Only
/// its exact generated marker and pending task can tie an unbound report to
/// this workflow. Unrelated completed history must not quarantine a new task.
/// Keep matching evidence intact until the workflow is reconciled.
pub fn block_unbound_legacy_dispatch(
    ticket: &mut Ticket,
    agents: &mut [crate::store::AgentRecord],
    role: &str,
    workflow: &str,
    now: &str,
    max_revision_rounds: u32,
) -> Result<Option<String>, ShepherdError> {
    let marker = match workflow {
        "revision" => match &ticket.revision_run {
            Some(run) => run_marker(run),
            None => return Ok(None),
        },
        "CI recovery" => ci_retry_generation(ticket),
        _ => return Err(ShepherdError::Usage("unknown legacy workflow".into())),
    };
    let checkout = ticket
        .worktree
        .as_ref()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()));
    let recipients: Vec<_> = agents
        .iter_mut()
        .filter(|agent| {
            agent.ticket == ticket.id
                && agent.role == role
                && agent.unit.is_none()
                && agent.task_id.is_none()
                && agent.attempt_id.is_none()
                && agent.report.is_some()
                && matches!(
                    agent.status,
                    crate::store::AgentStatus::Reported | crate::store::AgentStatus::Retired
                )
                && agent.task.lines().any(|line| line == marker)
                && (workflow != "revision"
                    || agent.task.contains(&format!(
                        "\nTask: {}\nAccepted comments:\n",
                        ticket.revision_task.as_deref().unwrap_or_default()
                    )))
                && checkout.as_ref().is_some_and(|path| {
                    agent
                        .worktree
                        .canonicalize()
                        .unwrap_or_else(|_| agent.worktree.clone())
                        == *path
                })
        })
        .collect();
    if recipients.is_empty() {
        return Ok(None);
    }
    let names = recipients
        .iter()
        .map(|agent| agent.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let message = format!(
        "unbound legacy dispatch for {workflow} {role}: {names}; inspect and reconcile the saved reports with the pending workflow before resuming; no replacement task was started"
    );
    for agent in recipients {
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
            if let Some(message) = block_unbound_legacy_dispatch(
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

fn revision_task(ticket: &Ticket, run: &RevisionRun, role: &str) -> String {
    let comments = ticket
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
        .join("\n");
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
