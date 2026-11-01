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
    #[serde(default)]
    pub tested_sha: Option<String>,
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
                tested_sha: None,
            });
        }
        let run = ticket.revision_run.as_mut().expect("initialized run");
        // Recover a launch that was persisted before the dispatcher crashed.
        if run.agent.is_none() {
            let marker = run_marker(run);
            run.agent = data
                .agents
                .iter()
                .rev()
                .find(|agent| {
                    agent.ticket == id
                        && agent.task.contains(&marker)
                        && !matches!(
                            agent.status,
                            AgentStatus::Retired | AgentStatus::LaunchFailed
                        )
                })
                .map(|agent| agent.name.clone());
        }
        if let Some(name) = &run.agent {
            let agent = data
                .agents
                .iter_mut()
                .find(|agent| &agent.name == name)
                .ok_or_else(|| ShepherdError::Usage("revision agent record disappeared".into()))?;
            match agent.status {
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked => {
                    return Ok(Ok(None));
                }
                AgentStatus::Reported => {
                    let report = agent.report.as_ref().ok_or_else(|| {
                        ShepherdError::Usage("revision agent has no report".into())
                    })?;
                    if let Some(tab) = &agent.tab_id {
                        h.close_tab(tab)?;
                    }
                    agent.status = AgentStatus::Retired;
                    run.agent = None;
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
                AgentStatus::Retired | AgentStatus::LaunchFailed => run.agent = None,
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
    let marker = run_marker(run);
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
    let agent = crate::flock::spawn(
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
    )?;
    store.update(|data| {
        if let Some(ticket) = data
            .tickets
            .iter_mut()
            .find(|ticket| ticket.id == id && ticket.state == TicketState::Revising)
            && let Some(run) = &mut ticket.revision_run
            && run_marker(run) == marker
        {
            run.agent = Some(agent.name.clone());
        }
        Ok(())
    })?;
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
        for comment in ticket.comments.iter_mut().filter(|comment| comment.verdict.as_deref() == Some("accepted") && !comment.acknowledged) {
            comment.ack_sha = Some(head.clone());
            data.pending_pings.push(format!("{id}: run ticket ack {id} --comment {} --body-file <path>; explain the change and include commit {head}", comment.id));
        }
        ticket.revision_run = None;
        ticket.revision_task = None;
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
        data.pending_pings.push(message.clone());
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
        "{}\nTicket {}: {}\n{}\nTask: {}\nAccepted comments:\n{}\nNew diff: {}..HEAD\nAcceptance: {:?}\n{}\nCode changes must be test-first, scoped to this task, and committed on the assigned branch. Use conventional commits, type(scope): imperative subject under 50 characters, with a body explaining why. Do not add AI attribution or Co-Authored-By trailers. Reviewers and QA should inspect and test the complete revision diff and report concrete evidence; delegate code fixes back through the shepherd.\nStay on branch {} in worktree {}. Never switch branches or write outside it. Never read .env files, Credentials/ folders, *.pem, *.key, or other secret files. Never push, open PRs, or call GitHub. Write JSON report in the assigned worktree, run {} agent report --agent {{{{agent_name}}}} --report-file <path>, then remove the report file after success. Never commit report files. Report JSON: {{\"status\":\"completed|blocked|failed\",\"summary\":\"text\",\"tests\":[],\"changed_paths\":[],\"blockers\":[],\"findings\":[],\"commit_sha\":null}}",
        run_marker(run),
        ticket.id,
        ticket.title,
        ticket.body,
        ticket.revision_task.as_deref().unwrap_or_default(),
        comments,
        run.base_sha,
        ticket.acceptance,
        action,
        ticket.branch,
        ticket
            .worktree
            .as_deref()
            .unwrap_or(Path::new("."))
            .display(),
        std::env::current_exe()
            .unwrap_or_else(|_| "herdr-shepherd".into())
            .display()
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
