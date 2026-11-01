pub mod decide;
use crate::{
    config::{self, Config},
    error::ShepherdError,
    gate::BranchPolicy,
    harness::{Harness, harness_for},
    herdr::{AgentInfo, AgentStart, Herdr},
    hooks,
    notify::Notifier,
    proc,
    roles::{self},
    store::{AgentRecord, AgentStatus, BlockedReason, Store, StoreData, TicketState, Transition},
    ticket::state::transition_with_limit,
};
pub use decide::{Condition, Decision, decide};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fs, path::Path, process::Command, time::Duration};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct Observation {
    hash: String,
    head: Option<String>,
    condition: String,
    condition_since: u64,
    last_nudge_at: Option<u64>,
    last_action: Option<String>,
    session_ref: Option<String>,
    actions: Vec<String>,
    answered_hash: Option<String>,
    prompt_delivery_uncertain: bool,
    prompt_claim: Option<crate::store::PromptClaim>,
    notice_episode: String,
    lead_notice_sent: bool,
    user_notified: bool,
    escalated: bool,
    denied: bool,
}
fn prior(rec: &AgentRecord) -> Observation {
    let mut observation: Observation = rec
        .observed
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    observation.prompt_delivery_uncertain = rec.prompt_delivery_uncertain();
    observation
}
fn remember(rec: &mut AgentRecord, observation: &Observation) {
    rec.observed = Some(serde_json::to_string(observation).expect("observation serializes"));
}
/// Track the condition, not its live screen or its current decision. A safe
/// rule and a later escalation can both happen within one blocked episode.
fn notice_episode(rec: &mut AgentRecord, condition: Option<&Condition>, stall_minutes: u64) {
    let key = match condition {
        Some(Condition::PromptDeliveryUncertain) => "prompt_delivery_uncertain",
        Some(Condition::Blocked { .. }) => "blocked",
        Some(Condition::IdleNoReport { .. }) => "idle",
        Some(Condition::Stalled { since_min }) if *since_min >= stall_minutes => "stalled",
        Some(Condition::Exited) => "exited",
        Some(Condition::ProviderFailure) => "provider_failure",
        _ => "",
    };
    let mut observation = prior(rec);
    if observation.notice_episode != key {
        observation.notice_episode = key.into();
        observation.lead_notice_sent = false;
        observation.user_notified = false;
        observation.escalated = false;
        observation.denied = false;
        remember(rec, &observation);
    }
}

fn is_lead(data: &StoreData, rec: &AgentRecord) -> bool {
    rec.role == "lead"
        || data
            .lead
            .as_ref()
            .is_some_and(|lead| lead.agent_name == rec.name)
}

fn ask_lead(ctx: &ApplyContext<'_>, rec: &AgentRecord, text: &str) -> Result<(), ShepherdError> {
    let notify_user = ctx.store.update(|data| {
        let lead = is_lead(data, rec);
        let Some(current) = data
            .agents
            .iter_mut()
            .find(|a| a.name == rec.name && active(a.status))
        else {
            return Ok(false);
        };
        let mut observation = prior(current);
        if lead {
            let notify = !observation.user_notified;
            observation.user_notified = true;
            remember(current, &observation);
            return Ok(notify);
        }
        if !observation.lead_notice_sent {
            observation.lead_notice_sent = true;
            remember(current, &observation);
            data.enqueue_lead_episode_notice(
                &crate::flock::compact_ping(text),
                Some(&rec.ticket),
                Some(&rec.name),
            );
        }
        Ok(false)
    })?;
    if notify_user {
        Notifier::new(ctx.h, ctx.cfg.upkeep.notifications)
            .send(&format!("{} needs attention", rec.name), text);
    }
    Ok(())
}

fn seconds(text: &str, now: u64) -> u64 {
    text.parse().unwrap_or(now)
}
fn active(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
    )
}

fn cancellable(rec: &AgentRecord) -> bool {
    active(rec.status) && rec.report.is_none() && !rec.prompt_delivery_uncertain()
}

#[derive(Default, Debug, Serialize)]
pub struct SweepReport {
    pub projects: usize,
    pub observed: usize,
    pub acted: usize,
    pub errors: Vec<String>,
}

/// Observe a snapshot without changing persisted state. Sweeps use observe_at
/// to commit progress and nudge clocks together with the agent record.
pub fn observe(
    h: &dyn Herdr,
    rec: &AgentRecord,
    harness: &dyn Harness,
) -> Result<Option<Condition>, ShepherdError> {
    let mut snapshot = rec.clone();
    Ok(observe_at(
        h,
        &mut snapshot,
        harness,
        crate::launch::now().parse().unwrap_or(0),
        None,
    )?
    .0)
}

fn observe_at(
    h: &dyn Herdr,
    rec: &mut AgentRecord,
    harness: &dyn Harness,
    now: u64,
    data: Option<&StoreData>,
) -> Result<(Option<Condition>, Option<AgentInfo>), ShepherdError> {
    let Some(info) = h.agent_get(&rec.name)? else {
        return Ok((
            Some(if rec.prompt_delivery_uncertain() {
                Condition::PromptDeliveryUncertain
            } else {
                Condition::Exited
            }),
            None,
        ));
    };
    let session = h.pane_session_ref(&info.pane_id)?;
    // A name/status lookup alone cannot prove that this task is executing.
    // Validate before forming a decision or replacing saved session evidence.
    if rec.prompt_delivery_uncertain()
        && (!owned_terminal(rec, &info)
            || data.is_some_and(|data| !owned(data, rec, &info))
            || !same_session(rec, session.as_deref()))
    {
        rec.status = AgentStatus::Blocked;
        return Ok((Some(Condition::PromptDeliveryUncertain), Some(info)));
    }
    if rec.prompt_delivery_uncertain()
        && rec
            .prompt_claim()
            .is_some_and(|claim| claim.kind != "initial")
    {
        rec.status = AgentStatus::Blocked;
        return Ok((Some(Condition::PromptDeliveryUncertain), Some(info)));
    }
    let screen = h.read_visible(&rec.name)?;
    let mut previous = prior(rec);
    let hash = format!("{:x}", Sha256::digest(screen.as_bytes()));
    let head = proc::run(
        Command::new("git")
            .current_dir(&rec.worktree)
            .args(["rev-parse", "HEAD"]),
        Duration::from_secs(20),
    )
    .ok()
    .flatten()
    .map(|s| s.trim().to_owned());
    let status = info.agent_status.as_str();
    let condition = match status {
        "blocked" => "blocked",
        "idle" | "done" => "idle",
        "working" => "working",
        _ => "unknown",
    };
    let changed_condition = previous.condition != condition;
    if changed_condition {
        let recovered_block = condition == "blocked"
            && previous.condition.is_empty()
            && rec.status == AgentStatus::Blocked;
        previous.condition = condition.into();
        previous.condition_since = if recovered_block {
            seconds(&rec.last_progress_at, now)
        } else {
            now
        };
        previous.last_nudge_at = None;
        rec.nudges = 0;
    }
    if condition == "working" && (previous.hash != hash || previous.head != head) {
        rec.last_progress_at = now.to_string();
        rec.nudges = 0;
        previous.last_nudge_at = None;
    }
    if let Some(reference) = session {
        previous.session_ref = Some(reference);
    }
    previous.hash = hash;
    previous.head = head;
    let elapsed = now.saturating_sub(previous.condition_since);
    if status == "working" {
        previous.prompt_delivery_uncertain = false;
        previous.prompt_claim = None;
    }
    let condition = if previous.prompt_delivery_uncertain {
        rec.status = AgentStatus::Blocked;
        Some(Condition::PromptDeliveryUncertain)
    } else if status == "blocked" {
        rec.status = AgentStatus::Blocked;
        Some(Condition::Blocked { screen })
    } else if decide::provider_failure(rec, &screen, harness) {
        Some(Condition::ProviderFailure)
    } else if matches!(status, "idle" | "done") && rec.report.is_none() && rec.role != "lead" {
        Some(Condition::IdleNoReport { since_s: elapsed })
    } else if status == "working" {
        rec.status = AgentStatus::Working;
        Some(Condition::Stalled {
            since_min: now.saturating_sub(seconds(&rec.last_progress_at, now)) / 60,
        })
    } else {
        None
    };
    remember(rec, &previous);
    Ok((condition, Some(info)))
}

pub(crate) fn same_session(rec: &AgentRecord, session: Option<&str>) -> bool {
    prior(rec)
        .session_ref
        .as_deref()
        .is_none_or(|expected| session == Some(expected))
}

pub(crate) fn owned_terminal(rec: &AgentRecord, info: &AgentInfo) -> bool {
    if info.name.as_deref() != Some(rec.name.as_str())
        || rec.pane_id.as_deref() != Some(info.pane_id.as_str())
    {
        return false;
    }
    let Ok(root) = rec.worktree.canonicalize() else {
        return false;
    };
    let Some(cwd) = info.cwd.as_ref().and_then(|p| p.canonicalize().ok()) else {
        return false;
    };
    if !cwd.starts_with(&root) {
        return false;
    }
    true
}

pub(crate) fn owned(data: &StoreData, rec: &AgentRecord, info: &AgentInfo) -> bool {
    if !owned_terminal(rec, info) {
        return false;
    }
    let Ok(root) = rec.worktree.canonicalize() else {
        return false;
    };
    if rec.role == "lead" {
        return data.lead.as_ref().is_some_and(|lead| {
            lead.agent_name == rec.name
                && lead.pane_id == rec.pane_id
                && info
                    .workspace_id
                    .as_deref()
                    .is_none_or(|workspace| lead.workspace_id.as_deref() == Some(workspace))
        }) && data.project_root.canonicalize().is_ok_and(|p| p == root);
    }
    data.tickets
        .iter()
        .find(|t| t.id == rec.ticket)
        .is_some_and(|ticket| {
            let workspace = if ticket
                .worktree
                .as_ref()
                .is_some_and(|p| p.canonicalize().is_ok_and(|p| p == root))
            {
                ticket.workspace_id.as_deref()
            } else if let Some(child) = ticket
                .children
                .iter()
                .find(|child| child.path.canonicalize().is_ok_and(|p| p == root))
            {
                Some(child.workspace_id.as_str())
            } else {
                return false;
            };
            info.workspace_id
                .as_deref()
                .is_none_or(|observed| workspace == Some(observed))
        })
}

/// Apply rechecks the durable record and live ownership before any terminal
/// input. All state updates preserve an concurrently submitted report.
pub struct ApplyContext<'a> {
    pub h: &'a dyn Herdr,
    pub store: &'a Store,
    pub cfg: &'a Config,
    pub state_dir: &'a Path,
    pub now_s: u64,
}

fn save_observation(
    store: &Store,
    rec: &AgentRecord,
    live: Option<&AgentInfo>,
) -> Result<(), ShepherdError> {
    store.update(|data| {
        // Ticket/lead ownership can change while terminal observation runs.
        // Validate initial-task acceptance against the state being committed,
        // not the sweep's earlier project snapshot. Followups need their own
        // acknowledgement or a real report; an earlier Working turn is no proof.
        if let Some(current) = data
            .agents
            .iter()
            .find(|a| a.name == rec.name && active(a.status))
            && current.prompt_delivery_uncertain()
            && !rec.prompt_delivery_uncertain()
            && (current
                .prompt_claim()
                .is_some_and(|claim| claim.kind != "initial")
                || live.is_none_or(|info| !owned(data, current, info))
                || !same_session(current, prior(rec).session_ref.as_deref()))
        {
            return Ok(());
        }
        if let Some(current) = data
            .agents
            .iter_mut()
            .find(|a| a.name == rec.name && active(a.status))
        {
            current.observed = rec.observed.clone();
            current.last_progress_at = rec.last_progress_at.clone();
            current.nudges = rec.nudges;
            current.status = rec.status;
        }
        Ok(())
    })
}

fn log_action(
    store: &Store,
    rec: &AgentRecord,
    decision: &Decision,
    now: u64,
) -> Result<(), ShepherdError> {
    store.update(|data| {
        if let Some(ticket) = data.tickets.iter_mut().find(|t| t.id == rec.ticket) {
            ticket.history.push(Transition {
                from: ticket.state,
                to: ticket.state,
                reason: ticket.blocked_reason,
                at: now.to_string(),
            });
        }
        if let Some(current) = data
            .agents
            .iter_mut()
            .find(|r| r.name == rec.name && active(r.status))
        {
            let mut obs = prior(current);
            obs.last_action = Some(format!("{decision:?}"));
            obs.actions.push(format!("{now}: {decision:?}"));
            remember(current, &obs);
        }
        Ok(())
    })
}

fn block(
    ctx: &ApplyContext<'_>,
    rec: &AgentRecord,
    reason: BlockedReason,
    text: &str,
) -> Result<(), ShepherdError> {
    let resolved = roles::resolve(ctx.cfg)?;
    let notify_user = ctx.store.update(|data| {
        let lead = is_lead(data, rec);
        let mut notify_user = false;
        let mut ping_lead = false;
        if let Some(ticket) = data.tickets.iter_mut().find(|t| t.id == rec.ticket)
            && ticket.state != TicketState::Blocked
        {
            transition_with_limit(
                ticket,
                TicketState::Blocked,
                Some(reason),
                &ctx.now_s.to_string(),
                resolved.shepherd.max_revision_rounds,
            )?;
        }
        if let Some(current) = data
            .agents
            .iter_mut()
            .find(|r| r.name == rec.name && active(r.status))
        {
            current.status = AgentStatus::Blocked;
            let mut observation = prior(current);
            observation.last_action = Some(text.into());
            notify_user = !observation.user_notified;
            ping_lead = !lead && !observation.lead_notice_sent;
            observation.user_notified = true;
            observation.lead_notice_sent |= ping_lead;
            match reason {
                BlockedReason::Policy => observation.denied = true,
                _ => observation.escalated = true,
            }
            remember(current, &observation);
        }
        if ping_lead {
            data.enqueue_lead_episode_notice(
                &crate::flock::compact_ping(text),
                Some(&rec.ticket),
                Some(&rec.name),
            );
        }
        Ok(notify_user)
    })?;
    hooks::emit(
        ctx.cfg,
        "agent.escalated",
        &serde_json::json!({"ticket":rec.ticket,"agent":rec.name,"text":text}),
    );
    if notify_user {
        Notifier::new(ctx.h, ctx.cfg.upkeep.notifications).send(
            &format!("{}: {} needs attention", rec.ticket, rec.name),
            text,
        );
    }
    Ok(())
}

pub fn apply(
    ctx: &ApplyContext<'_>,
    rec: &AgentRecord,
    decision: &Decision,
) -> Result<(), ShepherdError> {
    let _hooks = hooks::defer();
    let _ticket_operation = crate::ticket::operation::shared(ctx.store, &rec.ticket)?;
    let data = ctx.store.read()?;
    let Some(current) = data
        .agents
        .iter()
        .find(|r| r.name == rec.name && active(r.status) && r.report.is_none())
    else {
        return Ok(());
    };
    // Recheck quarantine against the durable record, including stale decisions
    // computed before an ambiguous delivery. Never mutate that agent's terminal.
    if current.prompt_delivery_uncertain()
        && !matches!(decision, Decision::AskLead { .. } | Decision::Nothing)
    {
        return Ok(());
    }
    let rec = current;
    let live = crate::flock::automatic::recipient(ctx.h, ctx.store, rec, false)?;
    let in_worktree = live.is_some();
    match decision {
        Decision::SendKeys { rule, keys } => {
            if !in_worktree
                || live
                    .as_ref()
                    .is_none_or(|info| info.agent_status != "blocked")
            {
                return Ok(());
            }
            // A status event and a watcher sweep can race a changing dialog.
            // Classify the live screen again immediately before terminal input.
            let screen = ctx.h.read_visible(&rec.name)?;
            let resolved = roles::resolve(ctx.cfg)?;
            let runner = resolved
                .runners
                .get(&rec.runner)
                .ok_or_else(|| ShepherdError::Usage("unknown pinned runner".into()))?;
            let harness = harness_for(&runner.harness)?;
            let default = crate::trusted::default_branch(&data.project_root, ctx.state_dir)?;
            let base = data
                .tickets
                .iter()
                .find(|t| t.id == rec.ticket)
                .map(|t| t.base.as_str())
                .filter(|s| !s.is_empty());
            let policy = BranchPolicy::new(
                &resolved.shepherd.protected_branches,
                default.as_deref(),
                base,
            )?;
            let fresh = decide(
                current,
                &Condition::Blocked {
                    screen: screen.clone(),
                },
                &resolved.supervision,
                &*harness,
                &policy,
                true,
                ctx.now_s,
            );
            if fresh != *decision {
                return if matches!(fresh, Decision::SendKeys { .. }) {
                    Ok(())
                } else {
                    apply(ctx, current, &fresh)
                };
            }
            let hash = format!("{:x}", Sha256::digest(screen.as_bytes()));
            if prior(current).answered_hash.as_deref() == Some(hash.as_str()) {
                return Ok(());
            }
            // Persist the input intent first. An interruption may require a
            // lead to retry, but must never auto-answer a later dialog twice.
            let claimed = ctx.store.update(|d| {
                let Some(a) = d.agents.iter_mut().find(|a| *a == rec) else {
                    return Ok(None);
                };
                let mut observation = prior(a);
                observation.answered_hash = Some(hash);
                remember(a, &observation);
                Ok(Some(a.clone()))
            })?;
            let Some(claimed) = claimed else {
                return Ok(());
            };
            if crate::flock::automatic::recipient(ctx.h, ctx.store, &claimed, false)?.is_none() {
                return Ok(());
            }
            ctx.h.send_keys(&rec.name, keys)?;
            hooks::emit(
                ctx.cfg,
                "agent.unblocked",
                &serde_json::json!({"ticket":rec.ticket,"agent":rec.name,"rule":rule}),
            );
        }
        Decision::AskLead { text } => ask_lead(ctx, rec, text)?,
        Decision::Deny { reason } => {
            if in_worktree {
                ctx.h.send_keys(&rec.name, &["esc".into()])?;
            }
            block(ctx, rec, BlockedReason::Policy, reason)?;
            // Reports do not wait for terminal operation ownership. Escape
            // may have raced the real report; do not close its terminal next.
            let fresh = ctx.store.read()?.agents.into_iter().find(|a| {
                a.name == rec.name
                    && cancellable(a)
                    && a.pane_id == rec.pane_id
                    && a.worktree == rec.worktree
                    && a.runner == rec.runner
            });
            if in_worktree
                && let Some(fresh) = fresh
                && crate::flock::automatic::recipient(ctx.h, ctx.store, &fresh, false)?.is_some()
            {
                if let Some(tab) = &rec.tab_id {
                    ctx.h.close_tab(tab)?;
                }
                ctx.store.update(|d| {
                    if let Some(a) = d
                        .agents
                        .iter_mut()
                        .find(|a| a.name == rec.name && cancellable(a))
                    {
                        a.status = AgentStatus::Retired;
                    }
                    Ok(())
                })?;
            }
            // The snapshot only schedules siblings. Never wait on another
            // agent while owning the primary: simultaneous denials must not
            // deadlock, and busy delivery is not permission to cancel it.
            for candidate in data.agents.iter().filter(|a| {
                a.ticket == rec.ticket && a.name != rec.name && a.role != "lead" && active(a.status)
            }) {
                let Some(_operation) =
                    crate::flock::try_agent_operation(ctx.store, &candidate.name)?
                else {
                    continue;
                };
                let fresh = ctx.store.read()?;
                let Some(sibling) = fresh.agents.iter().find(|a| {
                    a.name == candidate.name
                        && a.ticket == rec.ticket
                        && a.role != "lead"
                        && cancellable(a)
                }) else {
                    continue;
                };
                if let Some(info) = ctx.h.agent_get(&sibling.name)?
                    && owned(&fresh, sibling, &info)
                    && same_session(sibling, ctx.h.pane_session_ref(&info.pane_id)?.as_deref())
                    && ctx.store.read()?.agents.iter().any(|a| a == sibling)
                {
                    let _ = ctx.h.send_keys(&sibling.name, &["esc".into()]);
                    if !ctx.store.read()?.agents.iter().any(|a| a == sibling) {
                        continue;
                    }
                    if let Some(tab) = &sibling.tab_id {
                        ctx.h.close_tab(tab)?;
                    }
                    ctx.store.update(|d| {
                        if let Some(a) = d
                            .agents
                            .iter_mut()
                            .find(|a| a.name == sibling.name && cancellable(a))
                        {
                            a.status = AgentStatus::Retired;
                        }
                        Ok(())
                    })?;
                }
            }
        }
        Decision::Escalate { text } => block(ctx, rec, BlockedReason::Escalated, text)?,
        Decision::NudgeReport | Decision::NudgeStatus => {
            if !in_worktree {
                return Ok(());
            }
            let text = if matches!(decision, Decision::NudgeReport) {
                "Submit your JSON report with the report command in your task prompt. Include blockers if you cannot finish."
            } else {
                "Report one status line describing progress and any blocker; continue the assigned task."
            };
            crate::flock::automatic::dispatch(
                ctx.h,
                ctx.store,
                current,
                crate::flock::automatic::Prompt::Followup {
                    text,
                    kind: if matches!(decision, Decision::NudgeReport) {
                        "nudge_report"
                    } else {
                        "nudge_status"
                    },
                    nudge_at: Some(ctx.now_s),
                    queued: false,
                },
            )?;
        }
        Decision::Resume => resume(ctx, current)?,
        Decision::Respawn => respawn(ctx, current, &data, in_worktree)?,
        Decision::Nothing => return Ok(()),
    }
    log_action(ctx.store, rec, decision, ctx.now_s)
}

fn launch_env(state: &Path, project: &Path) -> HashMap<String, String> {
    let mut base = HashMap::new();
    for key in [
        "PATH",
        "LANG",
        "LC_ALL",
        "HERDR_SOCKET_PATH",
        "HERDR_BIN_PATH",
        "HERDR_PLUGIN_CONFIG_DIR",
    ] {
        if let Ok(value) = std::env::var(key) {
            base.insert(key.into(), value);
        }
    }
    let mut env = crate::gate::env::agent_env(&base, state);
    env.insert(
        "HERDR_PLUGIN_STATE_DIR".into(),
        state.to_string_lossy().into_owned(),
    );
    env.insert(
        "HERDR_SHEPHERD_PROJECT_ROOT".into(),
        project.to_string_lossy().into_owned(),
    );
    env
}

fn resume(ctx: &ApplyContext<'_>, rec: &AgentRecord) -> Result<(), ShepherdError> {
    // Exited identities are no longer discoverable by name. The saved pane is
    // the only permitted destination; never create another agent identity.
    if ctx.h.agent_get(&rec.name)?.is_some() {
        return Ok(());
    }
    let Some(pane) = rec.pane_id.as_deref() else {
        return block(
            ctx,
            rec,
            BlockedReason::Escalated,
            "Exited agent has no saved pane to resume",
        );
    };
    let data = ctx.store.read()?;
    let cwd = ctx.h.pane_cwd(pane)?;
    let own_info = AgentInfo {
        name: Some(rec.name.clone()),
        agent: String::new(),
        agent_status: String::new(),
        cwd,
        pane_id: pane.into(),
        workspace_id: None,
    };
    if !owned(&data, rec, &own_info)
        || !same_session(rec, ctx.h.pane_session_ref(pane)?.as_deref())
        || ctx.h.agents()?.iter().any(|a| a.pane_id == pane)
    {
        return block(
            ctx,
            rec,
            BlockedReason::Escalated,
            "Saved pane no longer has an available shell inside the assigned worktree",
        );
    }
    let session = prior(rec).session_ref.or(ctx.h.pane_session_ref(pane)?);

    let resolved = roles::resolve(ctx.cfg)?;
    let runner = resolved
        .runners
        .get(&rec.runner)
        .ok_or_else(|| ShepherdError::Usage("unknown pinned runner".into()))?;
    let harness = harness_for(&runner.harness)?;
    let claimed = ctx.store.update(|data| {
        let Some(a) = data.agents.iter_mut().find(|a| *a == rec) else {
            return Ok(false);
        };
        a.restarts = a.restarts.saturating_add(1);
        a.status = AgentStatus::Starting;
        Ok(true)
    })?;
    if !claimed {
        return Ok(());
    }
    let project = ctx.store.read()?.project_root;
    let result = ctx.h.start_agent(
        &rec.name,
        harness.kind(),
        pane,
        &harness.managed_args(
            harness.resume_args(runner, session.as_deref()),
            ctx.state_dir,
        )?,
        &launch_env(ctx.state_dir, &project),
    );
    match result {
        Ok(start) => ctx.store.update(|data| {
            if let Some(a) = data
                .agents
                .iter_mut()
                .find(|a| a.name == rec.name && active(a.status))
            {
                a.status = if start == AgentStart::Ready {
                    AgentStatus::Working
                } else {
                    AgentStatus::Blocked
                };
                a.last_progress_at = ctx.now_s.to_string();
            }
            Ok(())
        }),
        Err(error) => block(
            ctx,
            rec,
            BlockedReason::Escalated,
            &format!("{}: resume failed: {error}", rec.name),
        ),
    }
}

fn respawn(
    ctx: &ApplyContext<'_>,
    rec: &AgentRecord,
    data: &StoreData,
    in_worktree: bool,
) -> Result<(), ShepherdError> {
    if !in_worktree {
        return Ok(());
    }
    let resolved = roles::resolve(ctx.cfg)?;
    let next = resolved
        .roles
        .iter()
        .find(|r| r.name == rec.role)
        .and_then(|role| {
            role.runners
                .iter()
                .position(|name| name == &rec.runner)
                .and_then(|i| role.runners.get(i + 1))
        })
        .and_then(|name| resolved.runners.get(name));
    let Some(runner) = next else {
        return block(
            ctx,
            rec,
            BlockedReason::Escalated,
            &format!("{}: {} exhausted provider runners", rec.ticket, rec.name),
        );
    };
    let ticket = data.tickets.iter().find(|t| t.id == rec.ticket);
    let child = ticket.and_then(|ticket| {
        ticket
            .children
            .iter()
            .find(|child| child.path.canonicalize().ok() == rec.worktree.canonicalize().ok())
    });
    let workspace = child
        .map(|child| child.workspace_id.as_str())
        .or_else(|| ticket.and_then(|ticket| ticket.workspace_id.as_deref()));
    let Some(workspace) = workspace else {
        return block(
            ctx,
            rec,
            BlockedReason::Escalated,
            "Provider fallback has no owned workspace",
        );
    };
    if crate::flock::automatic::recipient(ctx.h, ctx.store, rec, false)?.is_none() {
        return Ok(());
    }
    if let Some(tab) = &rec.tab_id {
        ctx.h.close_tab(tab)?;
    }
    if !ctx.store.read()?.agents.contains(rec) {
        return Ok(());
    }
    let label = child
        .filter(|child| child.unit > 1 && rec.role == "coder")
        .map(|child| format!("coder-{}", child.unit))
        .unwrap_or_else(|| rec.role.clone());
    let (tab, pane) = ctx.h.create_tab(workspace, &label, &rec.worktree)?;
    let replaced = ctx.store.update(|d| {
        let Some(a) = d.agents.iter_mut().find(|a| *a == rec) else {
            return Ok(false);
        };
        a.runner = runner.name.clone();
        a.tab_id = Some(tab);
        a.pane_id = Some(pane.clone());
        a.status = AgentStatus::Starting;
        a.prompt_pending = true;
        a.nudges = 0;
        // This pane starts a new task/session. Keep action history, but none
        // of the predecessor terminal's session, screen or nudge evidence.
        let observation = Observation {
            actions: prior(a).actions,
            ..Default::default()
        };
        remember(a, &observation);
        a.last_progress_at = ctx.now_s.to_string();
        Ok(true)
    })?;
    if !replaced {
        return Ok(());
    }
    let harness = harness_for(&runner.harness)?;
    let start = ctx.h.start_agent(
        &rec.name,
        harness.kind(),
        &pane,
        &harness.managed_args(harness.launch_args(runner), ctx.state_dir)?,
        &launch_env(ctx.state_dir, &data.project_root),
    )?;
    if start == AgentStart::Ready {
        match crate::flock::deliver_pending_prompt(ctx.h, ctx.store, &rec.name) {
            Ok(_) => {}
            Err(error)
                if error.prompt_submission_uncertain()
                    || matches!(
                        error.herdr_code(),
                        Some("agent_blocked" | "agent_not_ready")
                    ) => {}
            Err(error) => return Err(error),
        }
    } else {
        ctx.store.update(|d| {
            if let Some(a) = d
                .agents
                .iter_mut()
                .find(|a| a.name == rec.name && active(a.status))
            {
                a.status = AgentStatus::Blocked;
            }
            Ok(())
        })?;
    }
    if ctx.cfg.upkeep.model_badge {
        ctx.h.report_pane_tokens(
            &pane,
            &[
                (
                    crate::metadata::token(&ctx.cfg.upkeep.token_prefix, "model")?,
                    crate::launch::model_badge(runner),
                ),
                (
                    crate::metadata::token(&ctx.cfg.upkeep.token_prefix, "stage")?,
                    rec.role.clone(),
                ),
            ],
            &[],
        )?;
    }
    Ok(())
}

fn deliver_pending(h: &dyn Herdr, store: &Store, rec: &AgentRecord) -> Result<(), ShepherdError> {
    if !rec.prompt_pending {
        return Ok(());
    }
    let Some(info) = crate::flock::automatic::recipient(h, store, rec, false)? else {
        return Ok(());
    };
    if !matches!(info.agent_status.as_str(), "idle" | "done") {
        return Ok(());
    }
    match crate::flock::deliver_pending_prompt(h, store, &rec.name) {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.herdr_code(),
                Some("agent_blocked" | "agent_not_ready")
            ) || error.prompt_submission_uncertain() =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub fn supervise_project(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    state_dir: &Path,
    now: u64,
) -> Result<SweepReport, ShepherdError> {
    // Pane events and periodic sweeps share this outer ownership scope. Hooks
    // must wait for its project, ticket and agent locks, including nested apply.
    let _hooks = hooks::defer();
    let _guard = project_lock(store)?;
    store.heal_pending_pings()?;
    let resolved = roles::resolve(cfg)?;
    let mut report = SweepReport {
        projects: 1,
        ..Default::default()
    };
    let data = store.read()?;
    let default = crate::trusted::default_branch(&data.project_root, state_dir)?;
    for snapshot in data.agents.iter().filter(|a| active(a.status)) {
        let _ticket_operation = crate::ticket::operation::shared(store, &snapshot.ticket)?;
        let Some(_operation) = crate::flock::try_agent_operation(store, &snapshot.name)? else {
            continue;
        };
        // The snapshot only schedules work. A launcher or recovery may have
        // completed delivery (or the agent may have reported/retired) before
        // this lock was acquired. Observe the durable record and ownership now
        // so a stale observation cannot erase a newly persisted quarantine.
        let data = store.read()?;
        let Some(mut rec) = data
            .agents
            .iter()
            .find(|a| a.name == snapshot.name && active(a.status) && a.report.is_none())
            .cloned()
        else {
            continue;
        };
        report.observed += 1;
        let result = (|| -> Result<usize, ShepherdError> {
            let mut acted = 0;
            let runner = resolved
                .runners
                .get(&rec.runner)
                .ok_or_else(|| ShepherdError::Usage(format!("unknown runner {}", rec.runner)))?;
            let harness = harness_for(&runner.harness)?;
            let (condition, live) = observe_at(h, &mut rec, &*harness, now, Some(&data))?;
            notice_episode(
                &mut rec,
                condition.as_ref(),
                resolved.supervision.stall_minutes,
            );
            if rec.status == AgentStatus::Starting && live.is_none() {
                return Ok(0);
            }
            let in_worktree = live.as_ref().is_some_and(|info| owned(&data, &rec, info));
            let base = data
                .tickets
                .iter()
                .find(|t| t.id == rec.ticket)
                .map(|t| t.base.as_str())
                .filter(|s| !s.is_empty());
            let policy = BranchPolicy::new(
                &resolved.shepherd.protected_branches,
                default.as_deref(),
                base,
            )?;
            save_observation(store, &rec, live.as_ref())?;
            if let Some(condition) = condition {
                let mut decision = decide(
                    &rec,
                    &condition,
                    &resolved.supervision,
                    &*harness,
                    &policy,
                    in_worktree,
                    now,
                );
                let observation = prior(&rec);
                let already_notified = match decision {
                    Decision::AskLead { .. } if is_lead(&data, &rec) => observation.user_notified,
                    Decision::AskLead { .. } => observation.lead_notice_sent,
                    Decision::Escalate { .. } => observation.escalated,
                    Decision::Deny { .. } => observation.denied,
                    _ => false,
                };
                if already_notified {
                    decision = Decision::Nothing;
                }
                if let Some(nudged) = observation.last_nudge_at {
                    let gap = match condition {
                        Condition::IdleNoReport { .. } => resolved.supervision.report_grace_seconds,
                        Condition::Stalled { .. } => {
                            resolved.supervision.stall_minutes.saturating_mul(60)
                        }
                        _ => 0,
                    };
                    if matches!(decision, Decision::AskLead { .. })
                        && gap > 0
                        && now.saturating_sub(nudged) < gap
                    {
                        decision = Decision::Nothing;
                    }
                }
                if !matches!(decision, Decision::Nothing) {
                    apply(
                        &ApplyContext {
                            h,
                            store,
                            cfg,
                            state_dir,
                            now_s: now,
                        },
                        &rec,
                        &decision,
                    )?;
                    acted += 1;
                }
                // Safe lead input and its escalation clock are independent.
                // Recheck durable state and live status after keys, since the
                // rule may have resolved the block or changed the terminal.
                if matches!(decision, Decision::SendKeys { .. })
                    && is_lead(&data, &rec)
                    && !observation.escalated
                    && now.saturating_sub(observation.condition_since)
                        >= resolved.supervision.escalate_minutes.saturating_mul(60)
                    && let Condition::Blocked { screen } = &condition
                {
                    let current = store
                        .read()?
                        .agents
                        .into_iter()
                        .find(|a| a.name == rec.name && active(a.status) && a.report.is_none());
                    if let Some(current) = current
                        && !prior(&current).escalated
                        && h.agent_get(&current.name)?.is_some_and(|live| {
                            live.agent_status == "blocked"
                                && live.pane_id == rec.pane_id.as_deref().unwrap_or_default()
                        })
                    {
                        apply(
                            &ApplyContext {
                                h,
                                store,
                                cfg,
                                state_dir,
                                now_s: now,
                            },
                            &current,
                            &Decision::Escalate {
                                text: format!(
                                    "{}: {} still blocked: {screen}",
                                    rec.ticket, rec.name
                                ),
                            },
                        )?;
                        acted += 1;
                    }
                }
            }
            // Safe rules can make an agent ready immediately, or by next sweep.
            let current = store
                .read()?
                .agents
                .into_iter()
                .find(|a| a.name == rec.name);
            if let Some(current) = current.filter(|a| active(a.status)) {
                deliver_pending(h, store, &current)?;
            }
            Ok(acted)
        })();
        match result {
            Ok(acted) => report.acted += acted,
            Err(error) => report.errors.push(format!("{}: {error}", rec.name)),
        }
    }
    if let Err(error) = crate::flock::reconcile(h, store) {
        report.errors.push(error.to_string());
    }
    if let Err(error) = crate::lead::flush_pings(h, store) {
        report.errors.push(error.to_string());
    }
    Ok(report)
}

fn project_lock(store: &Store) -> Result<fs::File, ShepherdError> {
    fs::create_dir_all(store.root()).map_err(anyhow::Error::from)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.root().join("supervise.lock"))
        .map_err(anyhow::Error::from)?;
    file.lock().map_err(anyhow::Error::from)?;
    Ok(file)
}

pub(crate) fn project_stores(state_dir: &Path) -> Result<(Vec<Store>, Vec<String>), ShepherdError> {
    let entries = match fs::read_dir(state_dir.join("projects")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(anyhow::Error::from(error).into()),
    };
    let mut stores = Vec::new();
    let mut errors = Vec::new();
    for entry in entries {
        let result = (|| -> Result<Option<Store>, ShepherdError> {
            let entry = entry.map_err(anyhow::Error::from)?;
            if !entry.file_type().map_err(anyhow::Error::from)?.is_dir() {
                return Ok(None);
            }
            let path = entry.path().join("state.json");
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(anyhow::Error::from(error).into()),
            };
            if !metadata.file_type().is_file() {
                return Err(ShepherdError::Usage(format!(
                    "{} is not a regular project state file",
                    path.display()
                )));
            }
            let data: StoreData = serde_json::from_slice(
                &fs::read(&path).map_err(anyhow::Error::from)?,
            )
            .map_err(|error| ShepherdError::Usage(format!("{}: {error}", path.display())))?;
            let store = Store::open(state_dir, &data.project_root)?;
            if store.root() != entry.path() {
                return Err(ShepherdError::Usage(
                    "project state path hash mismatch".into(),
                ));
            }
            Ok(Some(store))
        })();
        match result {
            Ok(Some(store)) => stores.push(store),
            Ok(None) => {}
            Err(error) => errors.push(error.to_string()),
        }
    }
    Ok((stores, errors))
}

/// A one-second watcher loop calls this; per-project persistent due clocks
/// honor distinct supervision cadences, including across watcher restarts.
pub fn sweep_projects(h: &dyn Herdr, state_dir: &Path, global: &Config, now: u64) -> SweepReport {
    let mut report = SweepReport::default();
    let stores = match project_stores(state_dir) {
        Ok((stores, errors)) => {
            report.errors.extend(errors);
            stores
        }
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    for store in stores {
        let result = (|| -> Result<Option<SweepReport>, ShepherdError> {
            // Healing is independent of agents, configuration and cadence. In
            // particular, an abandoned project's legacy queue must shrink too.
            store.heal_pending_pings()?;
            let data = store.read()?;
            if !data.agents.iter().any(|a| active(a.status)) {
                return Ok(None);
            }
            let cfg = config_for(global, &data.project_root, state_dir)?;
            if !cfg.enabled {
                return Ok(None);
            }
            let resolved = roles::resolve(&cfg)?;
            let due_path = store.root().join("supervise-last.json");
            let last = fs::read(&due_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<u64>(&bytes).ok());
            if last.is_some_and(|last| {
                now.saturating_sub(last) < resolved.supervision.sweep_seconds.max(1)
            }) {
                return Ok(None);
            }
            let outcome = supervise_project(h, &store, &cfg, state_dir, now)?;
            fs::write(
                due_path,
                serde_json::to_vec(&now).expect("timestamp serializes"),
            )
            .map_err(anyhow::Error::from)?;
            Ok(Some(outcome))
        })();
        match result {
            Ok(Some(outcome)) => {
                report.projects += outcome.projects;
                report.observed += outcome.observed;
                report.acted += outcome.acted;
                report.errors.extend(outcome.errors);
            }
            Ok(None) => {}
            Err(error) => report.errors.push(error.to_string()),
        }
    }
    report
}

pub(crate) fn config_for(
    global: &Config,
    root: &Path,
    state: &Path,
) -> Result<Config, ShepherdError> {
    let dir = match &global.source {
        config::ConfigSource::Global(path) => {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        }
        config::ConfigSource::Default => {
            // No global file was selected by this watcher. Do not rediscover
            // a different HOME config when --config-dir named a missing file.
            let empty = state.join("defaults-only-config");
            if empty.join("shepherd.toml").exists() {
                return Err(ShepherdError::Usage(
                    "unexpected defaults-only config".into(),
                ));
            }
            empty
        }
        _ => config::resolve_config_dir(None),
    };
    let mut selected = crate::trusted::committed_config(&dir, root, None, state)?;
    selected.upkeep = config::Upkeep {
        regroup: global.upkeep.regroup,
        pr_status: global.upkeep.pr_status,
        notifications: global.upkeep.notifications,
        model_badge: global.upkeep.model_badge,
        keybindings: global.upkeep.keybindings,
        token_prefix: global.upkeep.token_prefix.clone(),
    };
    Ok(selected)
}

pub fn handle_pane_event(
    h: &dyn Herdr,
    state_dir: &Path,
    global: &Config,
    event: &str,
    payload: &serde_json::Value,
) -> Result<usize, ShepherdError> {
    if !matches!(
        event,
        "pane.agent_status_changed" | "pane.agent_detected" | "pane.exited"
    ) {
        return Ok(0);
    }
    let pane = payload
        .get("pane_id")
        .or_else(|| payload.pointer("/pane/pane_id"))
        .or_else(|| payload.pointer("/data/pane_id"))
        .or_else(|| payload.pointer("/payload/pane_id"))
        .and_then(serde_json::Value::as_str);
    let Some(pane) = pane else {
        return Ok(0);
    };
    let mut count = 0;
    let mut first_error = None;
    let (stores, errors) = project_stores(state_dir)?;
    for error in errors {
        eprintln!("shepherd: project event: {error}");
    }
    for store in stores {
        let result = (|| -> Result<bool, ShepherdError> {
            let data = store.read()?;
            if !data
                .agents
                .iter()
                .any(|a| a.pane_id.as_deref() == Some(pane) && active(a.status))
            {
                return Ok(false);
            }
            let cfg = config_for(global, &data.project_root, state_dir)?;
            let report = supervise_project(
                h,
                &store,
                &cfg,
                state_dir,
                crate::launch::now().parse().unwrap_or(0),
            )?;
            for error in report.errors {
                eprintln!("shepherd: pane event: {error}");
            }
            Ok(true)
        })();
        match result {
            Ok(true) => count += 1,
            Ok(false) => {}
            Err(error) => {
                eprintln!("shepherd: pane event: {error}");
                first_error.get_or_insert(error);
            }
        }
    }
    if count == 0
        && let Some(error) = first_error
    {
        return Err(error);
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        herdr::{AgentInfo, fake::FakeHerdr},
        store::{AgentRecord, AgentStatus, Store, Ticket, TicketState},
    };
    fn fixture(dir: &std::path::Path) -> (Store, crate::config::Config, FakeHerdr) {
        assert!(
            std::process::Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["init", "--quiet", "--initial-branch", "main"])
                .arg(dir)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "fixture"
                ])
                .status()
                .unwrap()
                .success()
        );
        let store = Store::open(&dir.join("state"), dir).unwrap();
        let cfg = config::load(&dir.join("config"), None).unwrap();
        let rec = AgentRecord {
            name: "sh-owned-coder-1".into(),
            ticket: "T-1".into(),
            role: "coder".into(),
            runner: "codex-sol-high".into(),
            status: AgentStatus::Blocked,
            pane_id: Some("w1:p1".into()),
            tab_id: Some("w1:t1".into()),
            worktree: dir.join("ticket"),
            task: "do the task".into(),
            report: None,
            observed: None,
            last_progress_at: "0".into(),
            nudges: 0,
            restarts: 0,
            prompt_pending: false,
        };
        std::fs::create_dir(&rec.worktree).unwrap();
        store
            .update(|data| {
                data.tickets.push(Ticket {
                    id: "T-1".into(),
                    state: TicketState::Building,
                    worktree: Some(rec.worktree.clone()),
                    workspace_id: Some("w1".into()),
                    ..Default::default()
                });
                data.agents.push(rec.clone());
                Ok(())
            })
            .unwrap();
        let h = FakeHerdr::new(vec![]);
        h.agent_records.borrow_mut().push(AgentInfo {
            name: Some(rec.name),
            agent: "codex".into(),
            agent_status: "blocked".into(),
            cwd: Some(rec.worktree),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        });
        (store, cfg, h)
    }
    fn as_lead(store: &Store, h: &FakeHerdr, root: &Path) {
        store
            .update(|data| {
                let rec = &mut data.agents[0];
                rec.name = "sh-69c8fe-lead".into();
                rec.ticket.clear();
                rec.role = "lead".into();
                rec.runner = "claude-opus".into();
                rec.worktree = root.into();
                data.lead = Some(crate::store::LeadRecord {
                    agent_name: rec.name.clone(),
                    tab_id: rec.tab_id.clone(),
                    pane_id: rec.pane_id.clone(),
                    started_at: "0".into(),
                    workspace_id: Some("w1".into()),
                });
                Ok(())
            })
            .unwrap();
        let mut records = h.agent_records.borrow_mut();
        records[0].name = Some("sh-69c8fe-lead".into());
        records[0].cwd = Some(root.into());
        records[0].agent = "claude".into();
    }

    fn sweep_ok(h: &FakeHerdr, store: &Store, cfg: &Config, state: &Path, now: u64) {
        let report = supervise_project(h, store, cfg, state, now).unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
    }

    #[test]
    fn blocked_lead_changing_screen_notifies_user_once_per_episode_and_keeps_safe_rules() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        as_lead(&store, &h, dir.path());
        let state = dir.path().join("state");
        for now in 1..20 {
            h.visible_cache
                .borrow_mut()
                .insert("sh-69c8fe-lead".into(), format!("Choose something: {now}"));
            sweep_ok(&h, &store, &cfg, &state, now);
        }
        assert!(store.read().unwrap().pending_pings.is_empty());
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify "))
                .count(),
            1
        );
        // Reopening the store models restarting the supervisor, not a new episode.
        let store = Store::open(&state, dir.path()).unwrap();
        h.visible_cache
            .borrow_mut()
            .insert("sh-69c8fe-lead".into(), "A different dialog".into());
        sweep_ok(&h, &store, &cfg, &state, 1000);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify "))
                .count(),
            1
        );
        h.visible_cache.borrow_mut().insert(
            "sh-69c8fe-lead".into(),
            include_str!("../../tests/fixtures/claude-trust.txt").into(),
        );
        sweep_ok(&h, &store, &cfg, &state, 1001);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("send_keys ")
                    && call.contains("down")
                    && call.contains("enter"))
        );
        h.agent_records.borrow_mut()[0].agent_status = "working".into();
        sweep_ok(&h, &store, &cfg, &state, 1002);
        h.agent_records.borrow_mut()[0].agent_status = "blocked".into();
        h.visible_cache
            .borrow_mut()
            .insert("sh-69c8fe-lead".into(), "Choose something again".into());
        sweep_ok(&h, &store, &cfg, &state, 1003);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify "))
                .count(),
            2
        );
        assert!(store.read().unwrap().pending_pings.is_empty());
        assert!(h.accepted_prompts.borrow().is_empty());
    }

    #[test]
    fn blocked_flock_dedupes_screen_changes_resets_on_recovery_and_escalates_once() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let state = dir.path().join("state");
        let name = store.read().unwrap().agents[0].name.clone();
        for now in 1..20 {
            h.visible_cache
                .borrow_mut()
                .insert(name.clone(), format!("Choose something: {now}"));
            sweep_ok(&h, &store, &cfg, &state, now);
        }
        assert_eq!(store.read().unwrap().pending_pings.len(), 1);
        let store = Store::open(&state, dir.path()).unwrap();
        for now in [1000, 1001, 1002] {
            h.visible_cache
                .borrow_mut()
                .insert(name.clone(), format!("Still choose: {now}"));
            sweep_ok(&h, &store, &cfg, &state, now);
        }
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify "))
                .count(),
            1
        );
        assert_eq!(store.read().unwrap().pending_pings.len(), 1);
        assert_eq!(
            prior(&store.read().unwrap().agents[0])
                .actions
                .iter()
                .filter(|action| action.contains("Escalate"))
                .count(),
            1
        );
        h.agent_records.borrow_mut()[0].agent_status = "working".into();
        sweep_ok(&h, &store, &cfg, &state, 1003);
        h.agent_records.borrow_mut()[0].agent_status = "blocked".into();
        h.visible_cache
            .borrow_mut()
            .insert(name.clone(), "Choose something: 1".into());
        sweep_ok(&h, &store, &cfg, &state, 1004);
        assert_eq!(store.read().unwrap().pending_pings.len(), 2);
    }

    #[test]
    fn answered_safe_dialog_that_remains_blocked_escalates_once() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let state = dir.path().join("state");
        h.visible_cache.borrow_mut().insert(
            "sh-owned-coder-1".into(),
            "Trust this folder? Trust and continue".into(),
        );
        for now in [1, 2, 899, 900, 1000, 1001] {
            sweep_ok(&h, &store, &cfg, &state, now);
        }
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("send_keys "))
                .count(),
            1
        );
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify "))
                .count(),
            1
        );
        let observation = prior(&store.read().unwrap().agents[0]);
        assert_eq!(
            observation
                .actions
                .iter()
                .filter(|action| action.contains("Escalate"))
                .count(),
            1
        );
    }

    #[test]
    fn changing_safe_lead_dialogs_do_not_suppress_due_escalation() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        as_lead(&store, &h, dir.path());
        let state = dir.path().join("state");
        for now in [1, 899, 900, 901, 1000, 1001] {
            h.visible_cache.borrow_mut().insert(
                "sh-69c8fe-lead".into(),
                format!(
                    "{}\nFrame {now}",
                    include_str!("../../tests/fixtures/claude-trust.txt")
                ),
            );
            sweep_ok(&h, &store, &cfg, &state, now);
        }
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("send_keys "))
                .count(),
            6
        );
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("notify "))
                .count(),
            1
        );
        assert_eq!(
            prior(&store.read().unwrap().agents[0])
                .actions
                .iter()
                .filter(|action| action.contains("Escalate"))
                .count(),
            1
        );
        assert!(store.read().unwrap().pending_pings.is_empty());
    }

    #[test]
    fn every_human_attention_path_for_a_lead_avoids_the_lead_queue() {
        for condition in [
            Condition::PromptDeliveryUncertain,
            Condition::Blocked {
                screen: "Choose a package".into(),
            },
            Condition::Stalled { since_min: 60 },
            Condition::Exited,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            as_lead(&store, &h, dir.path());
            store
                .update(|data| {
                    data.agents[0].nudges = 1;
                    data.agents[0].restarts = 1;
                    Ok(())
                })
                .unwrap();
            let rec = store.read().unwrap().agents[0].clone();
            let settings = roles::resolve(&cfg).unwrap().supervision;
            let decision = decide(
                &rec,
                &condition,
                &settings,
                &*harness_for("claude").unwrap(),
                &BranchPolicy::new(&[], None, None).unwrap(),
                true,
                1,
            );
            assert!(matches!(decision, Decision::AskLead { .. }));
            let state = dir.path().join("state");
            let context = ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &state,
                now_s: 1,
            };
            apply(&context, &rec, &decision).unwrap();
            apply(
                &context,
                &rec,
                &Decision::Escalate {
                    text: "still blocked".into(),
                },
            )
            .unwrap();
            assert!(
                store.read().unwrap().pending_pings.is_empty(),
                "{condition:?}"
            );
            assert_eq!(
                h.calls
                    .borrow()
                    .iter()
                    .filter(|call| call.starts_with("notify "))
                    .count(),
                1,
                "{condition:?}"
            );
        }
    }

    #[test]
    fn concurrent_pending_delivery_submits_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _, _) = fixture(dir.path());
        store
            .update(|d| {
                d.agents[0].prompt_pending = true;
                Ok(())
            })
            .unwrap();
        let rec = store.read().unwrap().agents[0].clone();
        let barrier = std::sync::Barrier::new(2);
        let calls = std::thread::scope(|scope| {
            let mut threads = Vec::new();
            for _ in 0..2 {
                let rec = rec.clone();
                let store = &store;
                let barrier = &barrier;
                threads.push(scope.spawn(move || {
                    let h = FakeHerdr::new(vec![]);
                    h.agent_records.borrow_mut().push(AgentInfo {
                        name: Some(rec.name.clone()),
                        agent: "codex".into(),
                        agent_status: "idle".into(),
                        cwd: Some(rec.worktree.clone()),
                        pane_id: rec.pane_id.clone().unwrap(),
                        workspace_id: Some("w1".into()),
                    });
                    barrier.wait();
                    deliver_pending(&h, store, &rec).unwrap();
                    h.calls
                        .borrow()
                        .iter()
                        .filter(|c| c.starts_with("prompt_agent"))
                        .count()
                }));
            }
            threads
                .into_iter()
                .map(|t| t.join().unwrap())
                .sum::<usize>()
        });
        assert_eq!(calls, 1);
        assert!(!store.read().unwrap().agents[0].prompt_pending);
    }
    #[test]
    fn observation_does_not_resurrect_claimed_startup_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _, _) = fixture(dir.path());
        let mut rec = store.read().unwrap().agents[0].clone();
        rec.prompt_pending = true;
        store
            .update(|data| {
                data.agents[0].prompt_pending = false;
                Ok(())
            })
            .unwrap();
        save_observation(&store, &rec, None).unwrap();
        assert!(!store.read().unwrap().agents[0].prompt_pending);
    }

    /// Processing the first agent runs this callback after the sweep's snapshot
    /// and before it can acquire the second agent's operation lock. No timing
    /// assumptions or production scheduling hooks are needed.
    fn before_second_agent(
        store: &Store,
        h: &FakeHerdr,
        change: impl Fn(&Store, &str) + Send + 'static,
    ) {
        let target = store.read().unwrap().agents[0].clone();
        let mut first = target.clone();
        first.name = "sh-before-coder-1".into();
        first.pane_id = Some("w1:p2".into());
        first.tab_id = Some("w1:t2".into());
        first.prompt_pending = true;
        first.status = AgentStatus::Starting;
        store
            .update(|data| {
                data.agents.insert(0, first.clone());
                Ok(())
            })
            .unwrap();
        let live = h.agent_records.borrow()[0].clone();
        h.agent_records.borrow_mut().push(AgentInfo {
            name: Some(first.name.clone()),
            pane_id: first.pane_id.unwrap(),
            agent_status: "idle".into(),
            ..live
        });
        let store = store.clone();
        *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
            assert_eq!(
                name, first.name,
                "only the scheduling agent may be prompted"
            );
            let _operation = crate::flock::try_agent_operation(&store, &target.name)
                .unwrap()
                .expect("sweep has not acquired the second agent yet");
            change(&store, &target.name);
            crate::flock::store_report(&store, name, r#"{"status":"completed","summary":"scheduling callback completed","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        }));
    }

    #[test]
    fn sweep_snapshot_cannot_erase_concurrent_delivery_quarantine() {
        for accepted in [false, true] {
            for (status, screen) in [
                ("idle", ""),
                ("idle", "quota exhausted"),
                ("blocked", "quota exhausted"),
                ("blocked", "Trust this folder? Trust and continue"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let (store, mut cfg, h) = fixture(dir.path());
                cfg.raw = "[roles.coder]\nrunners=['omp-astra','codex-sol-high']\n"
                    .parse()
                    .unwrap();
                store
                    .update(|data| {
                        let rec = &mut data.agents[0];
                        rec.runner = "omp-astra".into();
                        rec.status = AgentStatus::Starting;
                        rec.prompt_pending = true;
                        Ok(())
                    })
                    .unwrap();
                let target = store.read().unwrap().agents[0].clone();
                h.agent_records.borrow_mut()[0].agent = "omp".into();
                h.agent_records.borrow_mut()[0].agent_status = status.into();
                h.visible_cache
                    .borrow_mut()
                    .insert(target.name.clone(), screen.into());
                before_second_agent(&store, &h, move |store, name| {
                    let transport = FakeHerdr::new(vec![]);
                    transport.prompt_results.borrow_mut().push_back((
                        accepted,
                        Err(ShepherdError::HerdrCommand {
                            code: "timeout".into(),
                            message: "acceptance unknown".into(),
                        }),
                    ));
                    assert!(
                        crate::flock::deliver_pending_prompt(&transport, store, name)
                            .unwrap_err()
                            .prompt_submission_uncertain()
                    );
                    assert_eq!(transport.calls.borrow().len(), 1);
                    assert_eq!(
                        transport.accepted_prompts.borrow().len(),
                        usize::from(accepted)
                    );
                    assert!(store.read().unwrap().agents[1].prompt_delivery_uncertain());
                });
                for now in [1, 1000, 2000] {
                    let sweep = supervise_project(&h, &store, &cfg, &dir.path().join("state"), now)
                        .unwrap();
                    assert!(sweep.errors.is_empty(), "{sweep:?}");
                    let current = store.read().unwrap().agents[1].clone();
                    assert!(current.prompt_delivery_uncertain(), "{status}: {screen}");
                    assert!(!current.prompt_pending);
                    assert_eq!(current.status, AgentStatus::Blocked);
                    assert_eq!(current.runner, target.runner);
                    assert_eq!(current.worktree, target.worktree);
                    assert_eq!(current.pane_id, target.pane_id);
                    assert_eq!(current.restarts, 0);
                }
                // Independent Working evidence may still release quarantine
                // after the racing sweep, without a second task submission.
                h.agent_records.borrow_mut()[0].agent_status = "working".into();
                h.visible_cache
                    .borrow_mut()
                    .insert(target.name.clone(), "fixture task progress".into());
                let sweep =
                    supervise_project(&h, &store, &cfg, &dir.path().join("state"), 2001).unwrap();
                assert!(sweep.errors.is_empty(), "{sweep:?}");
                let current = store.read().unwrap().agents[1].clone();
                assert!(!current.prompt_delivery_uncertain());
                assert_eq!(current.status, AgentStatus::Working);
                let calls = h.calls.borrow();
                assert_eq!(
                    calls
                        .iter()
                        .filter(|c| c.starts_with("prompt_agent"))
                        .count(),
                    1
                );
                assert!(!calls.iter().any(|c| c.starts_with("send_keys")
                    || c.starts_with("start_agent")
                    || c.starts_with("resume_agent")
                    || c.starts_with("close_tab")));
            }
        }
    }

    #[test]
    fn sweep_snapshot_skips_concurrently_reported_or_retired_agent() {
        for reported in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            let target = store.read().unwrap().agents[0].clone();
            before_second_agent(&store, &h, move |store, name| {
                if reported {
                    crate::flock::store_report(store, name, r#"{"status":"completed","summary":"real concurrent report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
                } else {
                    store
                        .update(|data| {
                            data.agents[1].status = AgentStatus::Retired;
                            Ok(())
                        })
                        .unwrap();
                }
            });
            let sweep = supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
            assert!(sweep.errors.is_empty(), "{sweep:?}");
            assert_eq!(
                sweep.observed, 1,
                "finished agents must be revalidated before observation"
            );
            let current = store.read().unwrap().agents[1].clone();
            assert_eq!(
                current.status,
                if reported {
                    AgentStatus::Reported
                } else {
                    AgentStatus::Retired
                }
            );
            if reported {
                assert_eq!(current.report.unwrap().summary, "real concurrent report");
            }
            assert!(!h.calls.borrow().iter().any(|c| c
                == &format!("read_visible {}", target.name)
                || c.starts_with("send_keys")));
        }
    }
    #[test]
    fn supervision_never_sends_keys_outside_owned_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
        assert!(!h.calls.borrow().iter().any(|c| c.starts_with("send_keys")));
        assert!(!store.read().unwrap().pending_pings.is_empty());
    }
    #[test]
    fn policy_denial_blocks_ticket_and_stops_owned_flock() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue. Allow git push origin main?".into());
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
        let data = store.read().unwrap();
        assert_eq!(data.tickets[0].state, TicketState::Blocked);
        assert_eq!(
            data.tickets[0].blocked_reason,
            Some(crate::store::BlockedReason::Policy)
        );
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c.contains("send_keys") && c.contains("esc"))
        );
        assert!(h.calls.borrow().iter().any(|c| c.starts_with("close_tab")));
        assert!(h.calls.borrow().iter().any(|c| c.starts_with("notify")));
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|c| c.contains("send_keys") && c.contains("enter"))
        );
    }

    fn add_sibling(store: &Store, h: &FakeHerdr) -> AgentRecord {
        let mut sibling = store.read().unwrap().agents[0].clone();
        sibling.name = "sh-owned-coder-2".into();
        sibling.pane_id = Some("w1:p2".into());
        sibling.tab_id = Some("w1:t2".into());
        store
            .update(|data| {
                data.agents.push(sibling.clone());
                Ok(())
            })
            .unwrap();
        let live = h.agent_records.borrow()[0].clone();
        h.agent_records.borrow_mut().push(AgentInfo {
            name: Some(sibling.name.clone()),
            pane_id: sibling.pane_id.clone().unwrap(),
            ..live
        });
        sibling
    }

    fn assert_no_terminal_mutation(h: &FakeHerdr, rec: &AgentRecord) {
        assert!(
            !h.calls.borrow().iter().any(|call| {
                call.starts_with(&format!("send_keys {} ", rec.name))
                    || call.starts_with(&format!("prompt_agent {} ", rec.name))
                    || call.starts_with(&format!("start_agent {} ", rec.name))
                    || call == &format!("resume_agent {}", rec.name)
                    || rec
                        .tab_id
                        .as_ref()
                        .is_some_and(|tab| call == &format!("close_tab {tab}"))
            }),
            "unexpected terminal mutation: {:?}",
            h.calls.borrow()
        );
    }

    #[test]
    fn denial_rechecks_sibling_quarantine_ownership_and_operation() {
        for case in [
            "uncertain",
            "outside",
            "pane",
            "session",
            "busy",
            "reported",
            "retired",
            "safe",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            let primary = store.read().unwrap().agents[0].clone();
            let sibling = add_sibling(&store, &h);
            store
                .update(|data| {
                    match case {
                        "uncertain" => data.agents[1].set_prompt_delivery_uncertain(true),
                        "retired" => data.agents[1].status = AgentStatus::Retired,
                        "session" => {
                            let mut observation = prior(&data.agents[1]);
                            observation.session_ref = Some("assigned-session".into());
                            remember(&mut data.agents[1], &observation);
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .unwrap();
            if case == "reported" {
                crate::flock::store_report(&store, &sibling.name, r#"{"status":"completed","summary":"real sibling report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
            }
            if case == "outside" {
                h.agent_records.borrow_mut()[1].cwd = Some(dir.path().into());
            }
            if case == "pane" {
                h.agent_records.borrow_mut()[1].pane_id = "w1:repurposed".into();
            }
            if case == "session" {
                h.pane_sessions
                    .borrow_mut()
                    .insert("w1:p2".into(), "different-session".into());
            }
            let _sibling_operation = (case == "busy").then(|| {
                crate::flock::try_agent_operation(&store, &sibling.name)
                    .unwrap()
                    .unwrap()
            });
            let before = store.read().unwrap().agents[1].clone();
            let _primary_operation = crate::flock::try_agent_operation(&store, &primary.name)
                .unwrap()
                .unwrap();
            apply(
                &ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 1,
                },
                &primary,
                &Decision::Deny {
                    reason: "fixture policy denial".into(),
                },
            )
            .unwrap();
            let data = store.read().unwrap();
            assert_eq!(data.tickets[0].state, TicketState::Blocked, "{case}");
            assert_eq!(data.tickets[0].blocked_reason, Some(BlockedReason::Policy));
            assert_eq!(data.agents[0].status, AgentStatus::Retired);
            if case == "safe" {
                assert_eq!(data.agents[1].status, AgentStatus::Retired);
                assert_eq!(
                    h.calls
                        .borrow()
                        .iter()
                        .filter(|c| *c == &format!("send_keys {} [\"esc\"]", sibling.name))
                        .count(),
                    1
                );
                assert_eq!(
                    h.calls
                        .borrow()
                        .iter()
                        .filter(|c| *c == "close_tab w1:t2")
                        .count(),
                    1
                );
            } else {
                assert_no_terminal_mutation(&h, &sibling);
                assert_eq!(data.agents[1], before, "{case}");
            }
        }
    }

    #[test]
    fn denial_snapshot_cannot_cancel_newly_uncertain_or_finished_sibling() {
        for case in [
            "unknown-unaccepted",
            "unknown-accepted",
            "accepted",
            "reported",
            "retired",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            let primary = store.read().unwrap().agents[0].clone();
            let sibling = add_sibling(&store, &h);
            let callback_store = store.clone();
            let sibling_name = sibling.name.clone();
            let primary_name = primary.name.clone();
            *h.on_send_keys.borrow_mut() = Some(Box::new(move |name| {
                if name != primary_name {
                    return;
                }
                let _operation = crate::flock::try_agent_operation(&callback_store, &sibling_name)
                    .unwrap()
                    .unwrap();
                match case {
                    "reported" => {
                        crate::flock::store_report(&callback_store, &sibling_name, r#"{"status":"completed","summary":"real report after denial snapshot","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
                    }
                    "retired" => callback_store
                        .update(|data| {
                            data.agents[1].status = AgentStatus::Retired;
                            Ok(())
                        })
                        .unwrap(),
                    _ => {
                        callback_store
                            .update(|data| {
                                data.agents[1].prompt_pending = true;
                                Ok(())
                            })
                            .unwrap();
                        let transport = FakeHerdr::new(vec![]);
                        let accepted = case != "unknown-unaccepted";
                        if case != "accepted" {
                            transport.prompt_results.borrow_mut().push_back((
                                accepted,
                                Err(ShepherdError::HerdrCommand {
                                    code: "timeout".into(),
                                    message: "unknown acceptance".into(),
                                }),
                            ));
                        }
                        let result = crate::flock::deliver_pending_prompt(
                            &transport,
                            &callback_store,
                            &sibling_name,
                        );
                        if case == "accepted" {
                            assert!(result.unwrap());
                        } else {
                            assert!(result.unwrap_err().prompt_submission_uncertain());
                        }
                        assert_eq!(transport.calls.borrow().len(), 1);
                        assert_eq!(
                            transport.accepted_prompts.borrow().len(),
                            usize::from(accepted)
                        );
                    }
                }
            }));
            let _operation = crate::flock::try_agent_operation(&store, &primary.name)
                .unwrap()
                .unwrap();
            apply(
                &ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 1,
                },
                &primary,
                &Decision::Deny {
                    reason: "fixture policy denial".into(),
                },
            )
            .unwrap();
            let data = store.read().unwrap();
            assert_eq!(data.tickets[0].state, TicketState::Blocked);
            let current = &data.agents[1];
            if case == "accepted" {
                assert_eq!(current.status, AgentStatus::Retired);
                assert_eq!(
                    h.calls
                        .borrow()
                        .iter()
                        .filter(|c| *c == "close_tab w1:t2")
                        .count(),
                    1
                );
            } else {
                assert_no_terminal_mutation(&h, &sibling);
                match case {
                    "reported" => {
                        assert_eq!(current.status, AgentStatus::Reported);
                        assert_eq!(
                            current.report.as_ref().unwrap().summary,
                            "real report after denial snapshot"
                        );
                    }
                    "retired" => assert_eq!(current.status, AgentStatus::Retired),
                    _ => {
                        assert_eq!(current.status, AgentStatus::Blocked);
                        assert!(current.prompt_delivery_uncertain());
                        assert!(!current.prompt_pending);
                    }
                }
            }
        }
    }

    #[test]
    fn denial_does_not_close_agent_that_reports_during_escape() {
        for target_sibling in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            let primary = store.read().unwrap().agents[0].clone();
            let sibling = add_sibling(&store, &h);
            let target = if target_sibling {
                sibling
            } else {
                primary.clone()
            };
            let callback_store = store.clone();
            let target_name = target.name.clone();
            *h.on_send_keys.borrow_mut() = Some(Box::new(move |name| {
                if name == target_name {
                    crate::flock::store_report(&callback_store, name, r#"{"status":"completed","summary":"real report during escape","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
                }
            }));
            let _operation = crate::flock::try_agent_operation(&store, &primary.name)
                .unwrap()
                .unwrap();
            apply(
                &ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 1,
                },
                &primary,
                &Decision::Deny {
                    reason: "fixture policy denial".into(),
                },
            )
            .unwrap();
            let data = store.read().unwrap();
            let current = data.agents.iter().find(|a| a.name == target.name).unwrap();
            assert_eq!(current.status, AgentStatus::Reported);
            assert_eq!(
                current.report.as_ref().unwrap().summary,
                "real report during escape"
            );
            assert!(
                !h.calls
                    .borrow()
                    .iter()
                    .any(|call| call == &format!("close_tab {}", target.tab_id.as_ref().unwrap())),
                "reported terminal was closed: {:?}",
                h.calls.borrow()
            );
            assert_eq!(data.tickets[0].state, TicketState::Blocked);
        }
    }

    #[test]
    fn simultaneous_denials_do_not_wait_on_each_others_operation() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        add_sibling(&store, &h);
        let records = store.read().unwrap().agents;
        let live_records = h.agent_records.borrow().clone();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let calls = std::thread::scope(|scope| {
            let threads: Vec<_> = records
                .iter()
                .map(|rec| {
                    let store = &store;
                    let cfg = &cfg;
                    let live_records = live_records.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        let _operation = crate::flock::try_agent_operation(store, &rec.name)
                            .unwrap()
                            .unwrap();
                        let h = FakeHerdr::new(vec![]);
                        *h.agent_records.borrow_mut() = live_records;
                        let name = rec.name.clone();
                        *h.on_send_keys.borrow_mut() = Some(Box::new(move |target| {
                            assert_eq!(target, name, "a busy sibling must not receive input");
                            barrier.wait();
                        }));
                        apply(
                            &ApplyContext {
                                h: &h,
                                store,
                                cfg,
                                state_dir: &store.root().join("fixture-state"),
                                now_s: 1,
                            },
                            rec,
                            &Decision::Deny {
                                reason: "simultaneous fixture denial".into(),
                            },
                        )
                        .unwrap();
                        h.calls.into_inner()
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        for rec in &records {
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call.starts_with(&format!("send_keys {} ", rec.name)))
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| *call == &format!("close_tab {}", rec.tab_id.as_ref().unwrap()))
                    .count(),
                1
            );
        }
        let data = store.read().unwrap();
        assert!(
            data.agents
                .iter()
                .all(|rec| rec.status == AgentStatus::Retired)
        );
        assert_eq!(data.tickets[0].state, TicketState::Blocked);
    }

    #[test]
    fn unowned_working_cannot_release_quarantine() {
        for mismatch in ["outside", "pane", "session", "missing-session", "ticket"] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            store
                .update(|data| {
                    let rec = &mut data.agents[0];
                    rec.set_prompt_delivery_uncertain(true);
                    let mut observation = prior(rec);
                    observation.session_ref = Some("owned-session".into());
                    remember(rec, &observation);
                    if mismatch == "ticket" {
                        data.tickets[0].worktree = Some(dir.path().into());
                    }
                    Ok(())
                })
                .unwrap();
            let original = store.read().unwrap().agents[0].clone();
            h.agent_records.borrow_mut()[0].agent_status = "working".into();
            let live_pane = if mismatch == "pane" {
                "w1:other-pane"
            } else {
                "w1:p1"
            };
            h.agent_records.borrow_mut()[0].pane_id = live_pane.into();
            h.pane_sessions.borrow_mut().insert(
                live_pane.into(),
                if mismatch == "session" {
                    "other-session"
                } else {
                    "owned-session"
                }
                .into(),
            );
            if mismatch == "missing-session" {
                h.pane_sessions.borrow_mut().clear();
            }
            if mismatch == "outside" {
                h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
            }
            let harness = harness_for("codex").unwrap();
            if mismatch != "ticket" {
                assert_eq!(
                    observe(&h, &original, &*harness).unwrap(),
                    Some(Condition::PromptDeliveryUncertain),
                    "{mismatch}"
                );
            }
            for now in [1, 1000] {
                let sweep =
                    supervise_project(&h, &store, &cfg, &dir.path().join("state"), now).unwrap();
                assert!(sweep.errors.is_empty(), "{sweep:?}");
                let current = store.read().unwrap().agents[0].clone();
                assert!(current.prompt_delivery_uncertain(), "{mismatch}");
                assert_eq!(current.status, AgentStatus::Blocked);
                assert_eq!(current.pane_id, original.pane_id);
                assert_eq!(prior(&current).session_ref, prior(&original).session_ref);
                assert_eq!(current.last_progress_at, original.last_progress_at);
                assert_no_terminal_mutation(&h, &current);
            }
            h.agent_records.borrow_mut()[0].cwd = Some(original.worktree.clone());
            h.agent_records.borrow_mut()[0].pane_id = original.pane_id.clone().unwrap();
            h.pane_sessions
                .borrow_mut()
                .insert("w1:p1".into(), "owned-session".into());
            store
                .update(|data| {
                    data.tickets[0].worktree = Some(original.worktree.clone());
                    Ok(())
                })
                .unwrap();
            h.visible_cache
                .borrow_mut()
                .insert(original.name.clone(), "owned task progress".into());
            assert!(matches!(
                observe(&h, &original, &*harness).unwrap(),
                Some(Condition::Stalled { .. })
            ));
            let sweep =
                supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1001).unwrap();
            assert!(sweep.errors.is_empty(), "{sweep:?}");
            let current = store.read().unwrap().agents[0].clone();
            assert!(!current.prompt_delivery_uncertain());
            assert_eq!(current.status, AgentStatus::Working);
            assert_eq!(current.last_progress_at, "1001");
            assert!(!current.prompt_pending);
            assert_no_terminal_mutation(&h, &current);
        }
    }

    #[test]
    fn resume_refuses_repurposed_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        h.agent_records.borrow_mut().clear();
        h.pane_cwds
            .borrow_mut()
            .insert("w1:p1".into(), dir.path().into());
        let rec = store.read().unwrap().agents[0].clone();
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::Resume,
        )
        .unwrap();
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("start_agent"))
        );
        assert_eq!(store.read().unwrap().agents[0].restarts, 0);
    }

    #[test]
    fn cancellation_drains_native_resume_before_retiring() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let rec = store.read().unwrap().agents[0].clone();
        h.agent_records.borrow_mut().clear();
        h.pane_cwds
            .borrow_mut()
            .insert("w1:p1".into(), rec.worktree.clone());
        let observed = store.clone();
        let (early_tx, early_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        *h.on_start.borrow_mut() = Some(Box::new(move |_| {
            let observed = observed.clone();
            let done = done_tx.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = crate::ticket::intake::cancel_agents(
                    &FakeHerdr::new(vec![]),
                    &observed,
                    "T-1",
                    "cancel",
                    3,
                );
                let _ = tx.send(());
                done.send(result).unwrap();
            });
            early_tx
                .send(rx.recv_timeout(Duration::from_millis(200)).is_ok())
                .unwrap();
        }));
        let result = apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::Resume,
        );
        let cancelled = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            !early_rx.recv().unwrap(),
            "cancellation returned while native resume could still start work"
        );
        result.unwrap();
        cancelled.unwrap();
        let data = store.read().unwrap();
        assert_eq!(data.tickets[0].state, TicketState::Closed);
        assert_eq!(data.agents[0].status, AgentStatus::Retired);
        let calls = h.calls.borrow().len();
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 2,
            },
            &rec,
            &Decision::Resume,
        )
        .unwrap();
        assert_eq!(
            h.calls.borrow().len(),
            calls,
            "retired work must never resume"
        );
    }

    #[test]
    fn native_resume_keeps_profile_session_and_restart_cap() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let rec = store.read().unwrap().agents[0].clone();
        h.agent_records.borrow_mut().clear();
        h.pane_cwds
            .borrow_mut()
            .insert("w1:p1".into(), rec.worktree.clone());
        h.pane_sessions
            .borrow_mut()
            .insert("w1:p1".into(), "fixture-session".into());
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::Resume,
        )
        .unwrap();
        assert_eq!(store.read().unwrap().agents[0].restarts, 1);
        assert!(h.calls.borrow().iter().any(|c| c.starts_with("start_agent")
            && c.contains("gpt-6.1-sol")
            && c.contains("fixture-session")
            && c.contains("resume")));
        h.agent_records.borrow_mut().clear();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 2).unwrap();
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("start_agent"))
                .count(),
            1
        );
        assert_eq!(store.read().unwrap().agents[0].restarts, 1);
    }
    #[test]
    fn managed_omp_native_resume_preserves_session_and_args() {
        for session in [None, Some("fixture-session")] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            store
                .update(|d| {
                    d.agents[0].runner = "omp-astra".into();
                    Ok(())
                })
                .unwrap();
            let rec = store.read().unwrap().agents[0].clone();
            h.agent_records.borrow_mut().clear();
            h.pane_cwds
                .borrow_mut()
                .insert("w1:p1".into(), rec.worktree.clone());
            if let Some(session) = session {
                h.pane_sessions
                    .borrow_mut()
                    .insert("w1:p1".into(), session.into());
            }
            apply(
                &ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 1,
                },
                &rec,
                &Decision::Resume,
            )
            .unwrap();
            let args = h.start_args.borrow();
            let pinned = roles::resolve(&cfg).unwrap().runners["omp-astra"]
                .args
                .clone();
            assert_eq!(&args[0][..pinned.len()], pinned);
            assert_eq!(args[0][args[0].len() - 2], "--config");
            assert_eq!(
                std::fs::read_to_string(args[0].last().unwrap()).unwrap(),
                "paste:\n  largeMenuThreshold: 0\n"
            );
            assert_eq!(args[0][pinned.len()], "--resume");
            if let Some(session) = session {
                assert_eq!(args[0][pinned.len() + 1], session);
            }
            assert!(h.accepted_prompts.borrow().is_empty());
            assert_eq!(store.read().unwrap().agents[0].runner, "omp-astra");
            assert_eq!(store.read().unwrap().agents[0].pane_id, rec.pane_id);
        }
    }

    #[test]
    fn owned_lead_gets_pending_prompt_after_safe_unblock() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        store
            .update(|d| {
                d.agents[0].role = "lead".into();
                d.agents[0].ticket = String::new();
                d.agents[0].worktree = dir.path().into();
                d.agents[0].prompt_pending = true;
                d.lead = Some(crate::store::LeadRecord {
                    agent_name: d.agents[0].name.clone(),
                    tab_id: d.agents[0].tab_id.clone(),
                    pane_id: d.agents[0].pane_id.clone(),
                    started_at: "0".into(),
                    workspace_id: Some("w1".into()),
                });
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
        assert!(store.read().unwrap().agents[0].prompt_pending);
        h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 2).unwrap();
        assert!(!store.read().unwrap().agents[0].prompt_pending);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt_agent") && c.contains("do the task"))
                .count(),
            1
        );
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 3).unwrap();
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt_agent") && c.contains("do the task"))
                .count(),
            1
        );
    }
    #[test]
    fn persisted_nudge_waits_another_grace_interval() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 0).unwrap();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 120).unwrap();
        assert_eq!(store.read().unwrap().agents[0].nudges, 1);
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 150).unwrap();
        assert!(store.read().unwrap().pending_pings.is_empty());
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 240).unwrap();
        assert!(!store.read().unwrap().pending_pings.is_empty());
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt_agent"))
                .count(),
            1
        );
    }
    #[test]
    fn automatic_working_proof_rechecks_project_at_observation_commit() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        store
            .update(|data| {
                data.agents[0].set_prompt_delivery_uncertain(true);
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut()[0].agent_status = "working".into();
        let callback_store = store.clone();
        *h.on_get.borrow_mut() = Some(Box::new(move |_| {
            callback_store
                .update(|data| {
                    data.tickets[0].worktree = None;
                    Ok(())
                })
                .unwrap();
        }));
        let sweep = supervise_project(&h, &store, &cfg, &dir.path().join("state"), 10).unwrap();
        assert!(sweep.errors.is_empty(), "{sweep:?}");
        let rec = store.read().unwrap().agents[0].clone();
        assert!(rec.prompt_delivery_uncertain());
        assert_eq!(rec.status, AgentStatus::Blocked);
        assert_no_terminal_mutation(&h, &rec);
    }

    #[test]
    fn automatic_nudges_claim_unknown_followups_before_dispatch() {
        for decision in [Decision::NudgeReport, Decision::NudgeStatus] {
            for outcome in [
                "timeout",
                "agent_prompt_stalled",
                "local-deadline",
                "malformed",
            ] {
                let dir = tempfile::tempdir().unwrap();
                let (store, cfg, h) = fixture(dir.path());
                h.agent_records.borrow_mut()[0].agent_status = "working".into();
                let error = match outcome {
                    "local-deadline" => {
                        ShepherdError::Runtime(crate::proc::DeadlineExceeded.into())
                    }
                    "malformed" => anyhow::anyhow!("malformed acknowledgement").into(),
                    code => ShepherdError::HerdrCommand {
                        code: code.into(),
                        message: "accepted without acknowledgement".into(),
                    },
                };
                h.prompt_results.borrow_mut().push_back((true, Err(error)));
                let callback_store = store.clone();
                *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
                    let rec = callback_store.read().unwrap().agents[0].clone();
                    assert!(
                        rec.prompt_delivery_uncertain(),
                        "claim must precede dispatch"
                    );
                    assert_eq!(rec.nudges, 1);
                    assert_eq!(prior(&rec).last_nudge_at, Some(10));
                    let observation: serde_json::Value =
                        serde_json::from_str(rec.observed.as_deref().unwrap()).unwrap();
                    assert!(
                        observation["prompt_claim"]["text"]
                            .as_str()
                            .unwrap()
                            .to_ascii_lowercase()
                            .contains("report")
                    );
                }));
                let ctx = ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 10,
                };
                let rec = store.read().unwrap().agents[0].clone();
                assert!(
                    apply(&ctx, &rec, &decision)
                        .unwrap_err()
                        .prompt_submission_uncertain()
                );
                *h.on_prompt.borrow_mut() = None;
                for now in [11, 1000] {
                    apply(&ctx, &rec, &decision).unwrap();
                    let sweep = supervise_project(&h, &store, &cfg, &dir.path().join("state"), now)
                        .unwrap();
                    assert!(sweep.errors.is_empty(), "{sweep:?}");
                    let current = store.read().unwrap().agents[0].clone();
                    assert!(
                        current.prompt_delivery_uncertain(),
                        "an earlier Working turn is not followup acceptance"
                    );
                    assert_eq!(current.nudges, 1);
                    assert_eq!(prior(&current).last_nudge_at, Some(10));
                }
                assert_eq!(h.accepted_prompts.borrow().len(), 1);
            }
        }
    }

    #[test]
    fn automatic_nudge_rejection_restores_clock_and_remains_recoverable() {
        for decision in [Decision::NudgeReport, Decision::NudgeStatus] {
            for code in ["agent_blocked", "agent_not_ready", "empty_agent_prompt"] {
                let dir = tempfile::tempdir().unwrap();
                let (store, cfg, h) = fixture(dir.path());
                h.agent_records.borrow_mut()[0].agent_status = "idle".into();
                h.prompt_results.borrow_mut().push_back((
                    false,
                    Err(ShepherdError::HerdrCommand {
                        code: code.into(),
                        message: "before input".into(),
                    }),
                ));
                let ctx = ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 10,
                };
                let rec = store.read().unwrap().agents[0].clone();
                assert!(
                    !apply(&ctx, &rec, &decision)
                        .unwrap_err()
                        .prompt_submission_uncertain()
                );
                let current = store.read().unwrap().agents[0].clone();
                assert!(!current.prompt_delivery_uncertain());
                assert_eq!(current.prompt_claim(), None);
                assert_eq!(current.nudges, 0);
                assert_eq!(prior(&current).last_nudge_at, None);
                apply(&ctx, &current, &decision).unwrap();
                assert_eq!(h.accepted_prompts.borrow().len(), 1);
                assert_eq!(store.read().unwrap().agents[0].nudges, 1);
                assert_eq!(
                    prior(&store.read().unwrap().agents[0]).last_nudge_at,
                    Some(10)
                );
            }
        }
    }

    #[test]
    fn automatic_nudge_crash_and_real_report_preserve_ownership() {
        for outcome in ["crash", "report", "retire"] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            h.agent_records.borrow_mut()[0].agent_status = "working".into();
            let callback_store = store.clone();
            *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
                assert!(callback_store.read().unwrap().agents[0].prompt_delivery_uncertain());
                match outcome {
                    "crash" => panic!("fixture interruption after durable claim"),
                    "report" => {
                        crate::flock::store_report(&callback_store, name, r#"{"status":"completed","summary":"real report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
                    }
                    _ => {
                        callback_store
                            .update(|data| {
                                data.agents[0].status = AgentStatus::Retired;
                                Ok(())
                            })
                            .unwrap();
                    }
                }
            }));
            h.prompt_results.borrow_mut().push_back((
                true,
                Err(ShepherdError::HerdrCommand {
                    code: "timeout".into(),
                    message: "unknown".into(),
                }),
            ));
            let rec = store.read().unwrap().agents[0].clone();
            let ctx = ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 10,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                apply(&ctx, &rec, &Decision::NudgeReport)
            }));
            if outcome == "crash" {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().unwrap_err().prompt_submission_uncertain());
            }
            *h.on_prompt.borrow_mut() = None;
            let current = store.read().unwrap().agents[0].clone();
            if outcome == "report" {
                assert_eq!(current.status, AgentStatus::Reported);
                assert_eq!(current.report.unwrap().summary, "real report");
            } else {
                assert!(current.prompt_delivery_uncertain());
                assert!(current.prompt_claim().unwrap().text.contains("JSON report"));
            }
            if outcome == "retire" {
                assert_eq!(current.status, AgentStatus::Retired);
            }
            for now in [11, 1000] {
                let sweep =
                    supervise_project(&h, &store, &cfg, &dir.path().join("state"), now).unwrap();
                assert!(sweep.errors.is_empty(), "{sweep:?}");
            }
            assert_eq!(
                h.calls
                    .borrow()
                    .iter()
                    .filter(|call| call.starts_with("prompt_agent "))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn automatic_fallback_forgets_only_predecessor_session() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut cfg, h) = fixture(dir.path());
        cfg.raw = "[roles.coder]\nrunners=['codex-sol-high','omp-astra']\n"
            .parse()
            .unwrap();
        store
            .update(|data| {
                let rec = &mut data.agents[0];
                rec.restarts = 2;
                rec.observed = Some(r#"{"session_ref":"old-codex-session"}"#.into());
                Ok(())
            })
            .unwrap();
        h.pane_sessions
            .borrow_mut()
            .insert("w1:p1".into(), "old-codex-session".into());
        h.prompt_results.borrow_mut().push_back((
            true,
            Err(ShepherdError::HerdrCommand {
                code: "timeout".into(),
                message: "accepted without acknowledgement".into(),
            }),
        ));
        let rec = store.read().unwrap().agents[0].clone();
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::Respawn,
        )
        .unwrap();
        let replacement = store.read().unwrap().agents[0].clone();
        assert_eq!(replacement.restarts, 2);
        assert!(replacement.prompt_delivery_uncertain());
        assert_eq!(prior(&replacement).session_ref, None);
        {
            let live = &mut h.agent_records.borrow_mut()[0];
            live.agent_status = "working".into();
            live.cwd = Some(rec.worktree);
            h.pane_sessions
                .borrow_mut()
                .insert(live.pane_id.clone(), "new-omp-session".into());
        }
        for now in [2, 3] {
            let sweep =
                supervise_project(&h, &store, &cfg, &dir.path().join("state"), now).unwrap();
            assert!(sweep.errors.is_empty(), "{sweep:?}");
            let current = store.read().unwrap().agents[0].clone();
            assert!(!current.prompt_delivery_uncertain());
            assert_eq!(current.status, AgentStatus::Working);
            assert_eq!(
                prior(&current).session_ref.as_deref(),
                Some("new-omp-session")
            );
        }
        assert_eq!(h.accepted_prompts.borrow().len(), 1);
    }

    #[test]
    fn provider_fallback_quarantines_unknown_delivery_and_stale_actions() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut cfg, h) = fixture(dir.path());
        cfg.raw = "[roles.coder]\nrunners=['codex-sol-high','omp-astra']\n"
            .parse()
            .unwrap();
        h.prompt_results.borrow_mut().push_back((
            false,
            Err(ShepherdError::Runtime(crate::proc::DeadlineExceeded.into())),
        ));
        let rec = store.read().unwrap().agents[0].clone();
        let ctx = ApplyContext {
            h: &h,
            store: &store,
            cfg: &cfg,
            state_dir: &dir.path().join("state"),
            now_s: 1,
        };
        apply(&ctx, &rec, &Decision::Respawn).unwrap();
        let current = store.read().unwrap().agents[0].clone();
        assert_eq!(current.runner, "omp-astra");
        assert_eq!(current.status, AgentStatus::Blocked);
        assert!(!current.prompt_pending);
        assert!(current.prompt_delivery_uncertain());
        let calls = h.calls.borrow().len();
        // These decisions can have been computed before quarantine was saved.
        for decision in [
            Decision::NudgeReport,
            Decision::NudgeStatus,
            Decision::Resume,
            Decision::Respawn,
            Decision::SendKeys {
                rule: "trust".into(),
                keys: vec!["enter".into()],
            },
            Decision::Deny {
                reason: "stale screen".into(),
            },
        ] {
            apply(&ctx, &rec, &decision).unwrap();
        }
        assert_eq!(h.calls.borrow().len(), calls);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt_agent"))
                .count(),
            1
        );
    }

    #[test]
    fn legacy_uncertain_lead_receives_no_notifications_or_keys() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        store
            .update(|data| {
                let a = &mut data.agents[0];
                a.role = "lead".into();
                a.observed = Some("prompt_delivery_uncertain".into());
                data.lead = Some(crate::store::LeadRecord {
                    agent_name: a.name.clone(),
                    tab_id: a.tab_id.clone(),
                    pane_id: a.pane_id.clone(),
                    workspace_id: Some("w1".into()),
                    started_at: "0".into(),
                });
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        crate::flock::ping(&h, &store, "queued notification");
        assert_eq!(crate::lead::flush_pings(&h, &store).unwrap(), 0);
        for now in [1, 1000] {
            let report =
                supervise_project(&h, &store, &cfg, &dir.path().join("state"), now).unwrap();
            assert!(report.errors.is_empty(), "{:?}", report.errors);
        }
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("prompt_agent") || c.starts_with("send_keys"))
        );
        assert!(store.read().unwrap().agents[0].prompt_delivery_uncertain());
        assert!(!store.read().unwrap().pending_pings.is_empty());
    }

    #[test]
    fn provider_fallback_pins_next_runner_without_resetting_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut cfg, h) = fixture(dir.path());
        cfg.raw = "[roles.coder]\nrunners=['codex-sol-high','omp-astra']\n"
            .parse()
            .unwrap();
        store
            .update(|d| {
                d.agents[0].restarts = 1;
                Ok(())
            })
            .unwrap();
        let rec = store.read().unwrap().agents[0].clone();
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::Respawn,
        )
        .unwrap();
        let current = store.read().unwrap().agents[0].clone();
        assert_eq!(current.runner, "omp-astra");
        assert_eq!(current.restarts, 1);
        let args = h.start_args.borrow();
        assert_eq!(args[0][args[0].len() - 2], "--config");
        assert_eq!(
            std::fs::read_to_string(args[0].last().unwrap()).unwrap(),
            "paste:\n  largeMenuThreshold: 0\n"
        );
        assert!(!current.prompt_pending);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c.contains("gpt-6-astra/medium"))
        );
    }
    #[test]
    fn pane_event_only_routes_recorded_panes() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let state = dir.path().join("state");
        assert_eq!(
            handle_pane_event(
                &h,
                &state,
                &cfg,
                "pane.agent_detected",
                &serde_json::json!({"pane_id":"foreign:p1"})
            )
            .unwrap(),
            0
        );
        assert!(h.calls.borrow().is_empty());
        store
            .update(|d| {
                d.agents[0].last_progress_at = crate::launch::now();
                Ok(())
            })
            .unwrap();
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        assert_eq!(
            handle_pane_event(
                &h,
                &state,
                &cfg,
                "pane.agent_detected",
                &serde_json::json!({"pane_id":"w1:p1"})
            )
            .unwrap(),
            1
        );
        assert!(h.calls.borrow().iter().any(|c| c.starts_with("send_keys")));
        assert_eq!(
            store.read().unwrap().tickets[0].state,
            TicketState::Building
        );
    }
    #[test]
    fn sweep_honors_each_project_interval() {
        let dir = tempfile::tempdir().unwrap();
        let (_store, cfg, h) = fixture(dir.path());
        let state = dir.path().join("state");
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        assert_eq!(sweep_projects(&h, &state, &cfg, 1).observed, 1);
        let calls = h.calls.borrow().len();
        assert_eq!(sweep_projects(&h, &state, &cfg, 10).observed, 0);
        assert_eq!(h.calls.borrow().len(), calls);
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        assert_eq!(sweep_projects(&h, &state, &cfg, 31).observed, 1);
    }
    #[test]
    fn new_block_gets_its_own_escalation_clock() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        store
            .update(|d| {
                d.agents[0].status = AgentStatus::Working;
                d.agents[0].observed =
                    Some(r#"{"condition":"working","condition_since":0}"#.into());
                Ok(())
            })
            .unwrap();
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 10_000).unwrap();
        assert_eq!(
            store.read().unwrap().tickets[0].state,
            TicketState::Building
        );
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("send_keys") && c.contains("enter"))
        );
    }

    #[test]
    fn claude_dialog_input_revalidates_screen_and_owned_cwd() {
        for (screen, owned, expected) in [
            (
                include_str!("../../tests/fixtures/claude-trust.txt").to_owned(),
                true,
                true,
            ),
            (
                include_str!("../../tests/fixtures/claude-trust.txt")
                    .replace(" ❯ No, exit", "   No, exit")
                    .replace("   Yes,", " ❯ Yes,"),
                true,
                false,
            ),
            (
                include_str!("../../tests/fixtures/claude-trust.txt").to_owned(),
                false,
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (store, cfg, h) = fixture(dir.path());
            store
                .update(|data| {
                    data.agents[0].runner = "claude-opus".into();
                    Ok(())
                })
                .unwrap();
            h.agent_records.borrow_mut()[0].agent = "claude".into();
            if !owned {
                h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
            }
            h.visible_screens.borrow_mut().push_back(screen);
            let rec = store.read().unwrap().agents[0].clone();
            apply(
                &ApplyContext {
                    h: &h,
                    store: &store,
                    cfg: &cfg,
                    state_dir: &dir.path().join("state"),
                    now_s: 1,
                },
                &rec,
                &Decision::SendKeys {
                    rule: "claude-trust".into(),
                    keys: vec!["down".into(), "enter".into()],
                },
            )
            .unwrap();
            assert_eq!(
                h.calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("send_keys")
                        && call.contains("down")
                        && call.contains("enter")),
                expected
            );
        }
    }

    #[test]
    fn changed_policy_dialog_is_rechecked_before_input() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let rec = store.read().unwrap().agents[0].clone();
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue. Allow git push origin main?".into());
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::SendKeys {
                rule: "codex-trust".into(),
                keys: vec!["enter".into()],
            },
        )
        .unwrap();
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|c| c.contains("send_keys") && c.contains("enter"))
        );
        assert_eq!(
            store.read().unwrap().tickets[0].blocked_reason,
            Some(BlockedReason::Policy)
        );
    }

    #[test]
    fn unchanged_safe_dialog_is_answered_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 2).unwrap();
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("send_keys"))
                .count(),
            1
        );
        let observed: serde_json::Value =
            serde_json::from_str(store.read().unwrap().agents[0].observed.as_ref().unwrap())
                .unwrap();
        assert_eq!(observed["actions"].as_array().unwrap().len(), 1);
    }
    #[test]
    fn periodic_sweep_preserves_agent_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (_, cfg, h) = fixture(dir.path());
        h.read_errors
            .borrow_mut()
            .insert("sh-owned-coder-1".into(), "fixture screen failure".into());
        let report = sweep_projects(&h, &dir.path().join("state"), &cfg, 1);
        assert_eq!(report.observed, 1);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("fixture screen failure")),
            "{report:?}"
        );
    }

    #[test]
    fn corrupt_and_missing_projects_do_not_stop_healthy_sweeps() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let state = dir.path().join("state");
        let bad = state.join("projects/bad");
        std::fs::create_dir(&bad).unwrap();
        std::fs::write(bad.join("state.json"), "{broken").unwrap();
        let mut missing = store.read().unwrap();
        missing.project_root = dir.path().join("removed-root");
        let path = state
            .join("projects")
            .join(crate::store::project_hash(&missing.project_root));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(
            path.join("state.json"),
            serde_json::to_vec(&missing).unwrap(),
        )
        .unwrap();
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        let result = sweep_projects(&h, &state, &cfg, 1);
        assert_eq!(result.observed, 1, "{result:?}");
        assert_eq!(result.errors.len(), 2);
        assert!(h.calls.borrow().iter().any(|c| c.starts_with("send_keys")));
    }

    #[test]
    fn one_agent_error_does_not_block_siblings_or_lead_pings() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let mut good = store.read().unwrap().agents[0].clone();
        good.name = "sh-owned-coder-2".into();
        good.pane_id = Some("w1:p2".into());
        good.tab_id = Some("w1:t2".into());
        let mut lead = good.clone();
        lead.name = "sh-owned-lead".into();
        lead.ticket = String::new();
        lead.role = "lead".into();
        lead.worktree = dir.path().into();
        lead.pane_id = Some("w1:p3".into());
        lead.tab_id = Some("w1:t3".into());
        lead.status = AgentStatus::Working;
        store
            .update(|d| {
                d.agents.push(good.clone());
                d.agents.push(lead.clone());
                d.lead = Some(crate::store::LeadRecord {
                    agent_name: lead.name.clone(),
                    pane_id: lead.pane_id.clone(),
                    tab_id: lead.tab_id.clone(),
                    started_at: "0".into(),
                    workspace_id: Some("w1".into()),
                });
                d.pending_pings.push("T-1: queued result".into());
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut().extend([
            AgentInfo {
                name: Some(good.name.clone()),
                agent: "codex".into(),
                agent_status: "blocked".into(),
                cwd: Some(good.worktree.clone()),
                pane_id: "w1:p2".into(),
                workspace_id: Some("w1".into()),
            },
            AgentInfo {
                name: Some(lead.name.clone()),
                agent: "codex".into(),
                agent_status: "idle".into(),
                cwd: Some(lead.worktree.clone()),
                pane_id: "w1:p3".into(),
                workspace_id: Some("w1".into()),
            },
        ]);
        h.read_errors
            .borrow_mut()
            .insert("sh-owned-coder-1".into(), "read unavailable".into());
        h.visible_cache.borrow_mut().insert(
            good.name.clone(),
            "Trust this folder? Trust and continue".into(),
        );
        let report = supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("send_keys sh-owned-coder-2"))
        );
        assert!(store.read().unwrap().pending_pings.is_empty());
    }

    #[test]
    fn pending_task_survives_spurious_working_signal() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        store
            .update(|d| {
                d.agents[0].prompt_pending = true;
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut()[0].agent_status = "working".into();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 1).unwrap();
        assert!(store.read().unwrap().agents[0].prompt_pending);
        h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        supervise_project(&h, &store, &cfg, &dir.path().join("state"), 2).unwrap();
        assert!(!store.read().unwrap().agents[0].prompt_pending);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt_agent") && c.contains("do the task"))
                .count(),
            1
        );
    }

    #[test]
    fn echoed_capacity_in_startup_prompt_is_not_provider_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (store, cfg, h) = fixture(dir.path());
        let data = store.read().unwrap();
        let resolved = crate::roles::resolve(&cfg).unwrap();
        let prompt = crate::prompts::lead_prompt(
            std::path::Path::new("/tmp/shepherd"),
            dir.path(),
            &resolved,
            &data,
        );
        assert!(prompt.to_lowercase().contains("capacity"));
        store
            .update(|d| {
                d.agents[0].task = prompt.clone();
                Ok(())
            })
            .unwrap();
        for (now, status) in [(1, "working"), (2, "idle")] {
            h.agent_records.borrow_mut()[0].agent_status = status.into();
            h.visible_cache
                .borrow_mut()
                .insert("sh-owned-coder-1".into(), prompt.clone());
            supervise_project(&h, &store, &cfg, &dir.path().join("state"), now).unwrap();
            assert_eq!(
                store.read().unwrap().tickets[0].state,
                TicketState::Building
            );
        }
        assert!(!h.calls.borrow().iter().any(|c| c.starts_with("close_tab")));
    }

    #[test]
    fn provider_fallback_stays_in_owned_child_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (store, mut cfg, h) = fixture(dir.path());
        cfg.raw = "[roles.coder]\nrunners=['codex-sol-high','omp-astra']\n"
            .parse()
            .unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();
        store
            .update(|d| {
                d.agents[0].worktree = child.clone();
                d.tickets[0].children.push(crate::store::ChildWorktree {
                    unit: 2,
                    branch: "feat/t--2".into(),
                    path: child.clone(),
                    workspace_id: "w-child".into(),
                    base_sha: "fixture".into(),
                    integrated: false,
                });
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut()[0].cwd = Some(child);
        h.agent_records.borrow_mut()[0].workspace_id = Some("w-child".into());
        let rec = store.read().unwrap().agents[0].clone();
        apply(
            &ApplyContext {
                h: &h,
                store: &store,
                cfg: &cfg,
                state_dir: &dir.path().join("state"),
                now_s: 1,
            },
            &rec,
            &Decision::Respawn,
        )
        .unwrap();
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("create_tab w-child coder-2"))
        );
    }
}
