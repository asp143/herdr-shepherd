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
}
fn prior(rec: &AgentRecord) -> Observation {
    rec.observed
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}
fn remember(rec: &mut AgentRecord, observation: &Observation) {
    rec.observed = Some(serde_json::to_string(observation).expect("observation serializes"));
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
    )?
    .0)
}

fn observe_at(
    h: &dyn Herdr,
    rec: &mut AgentRecord,
    harness: &dyn Harness,
    now: u64,
) -> Result<(Option<Condition>, Option<AgentInfo>), ShepherdError> {
    let Some(info) = h.agent_get(&rec.name)? else {
        return Ok((Some(Condition::Exited), None));
    };
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
    if let Some(reference) = h.pane_session_ref(&info.pane_id)? {
        previous.session_ref = Some(reference);
    }
    previous.hash = hash;
    previous.head = head;
    let elapsed = now.saturating_sub(previous.condition_since);
    let condition = if status == "blocked" {
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

fn owned(data: &StoreData, rec: &AgentRecord, info: &AgentInfo) -> bool {
    if info.name.as_deref() != Some(rec.name.as_str()) {
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
    if rec.role == "lead" {
        return data
            .lead
            .as_ref()
            .is_some_and(|lead| lead.agent_name == rec.name)
            && data.project_root.canonicalize().is_ok_and(|p| p == root);
    }
    data.tickets
        .iter()
        .find(|t| t.id == rec.ticket)
        .is_some_and(|ticket| {
            ticket
                .worktree
                .as_ref()
                .and_then(|p| p.canonicalize().ok())
                .as_ref()
                == Some(&root)
                || ticket
                    .children
                    .iter()
                    .any(|child| child.path.canonicalize().is_ok_and(|p| p == root))
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

fn save_observation(store: &Store, rec: &AgentRecord) -> Result<(), ShepherdError> {
    store.update(|data| {
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
    ctx.store.update(|data| {
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
            remember(current, &observation);
        }
        Ok(())
    })?;
    hooks::emit(
        ctx.cfg,
        "agent.escalated",
        &serde_json::json!({"ticket":rec.ticket,"agent":rec.name,"text":text}),
    );
    Notifier::new(ctx.h, ctx.cfg.upkeep.notifications).send(
        &format!("{}: {} needs attention", rec.ticket, rec.name),
        text,
    );
    crate::flock::ping(ctx.h, ctx.store, text);
    Ok(())
}

pub fn apply(
    ctx: &ApplyContext<'_>,
    rec: &AgentRecord,
    decision: &Decision,
) -> Result<(), ShepherdError> {
    let _ticket_operation = crate::ticket::operation::shared(ctx.store, &rec.ticket)?;
    let data = ctx.store.read()?;
    let Some(current) = data
        .agents
        .iter()
        .find(|r| r.name == rec.name && active(r.status))
    else {
        return Ok(());
    };
    let live = ctx.h.agent_get(&rec.name)?;
    let in_worktree = live
        .as_ref()
        .is_some_and(|info| owned(&data, current, info));
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
            ctx.store.update(|d| {
                if let Some(a) = d
                    .agents
                    .iter_mut()
                    .find(|a| a.name == rec.name && active(a.status))
                {
                    let mut observation = prior(a);
                    observation.answered_hash = Some(hash);
                    remember(a, &observation);
                }
                Ok(())
            })?;
            ctx.h.send_keys(&rec.name, keys)?;
            hooks::emit(
                ctx.cfg,
                "agent.unblocked",
                &serde_json::json!({"ticket":rec.ticket,"agent":rec.name,"rule":rule}),
            );
        }
        Decision::AskLead { text } => crate::flock::ping(ctx.h, ctx.store, text),
        Decision::Deny { reason } => {
            if in_worktree {
                ctx.h.send_keys(&rec.name, &["esc".into()])?;
            }
            block(ctx, rec, BlockedReason::Policy, reason)?;
            if in_worktree {
                if let Some(tab) = &rec.tab_id {
                    ctx.h.close_tab(tab)?;
                }
                ctx.store.update(|d| {
                    if let Some(a) = d
                        .agents
                        .iter_mut()
                        .find(|a| a.name == rec.name && active(a.status))
                    {
                        a.status = AgentStatus::Retired;
                    }
                    Ok(())
                })?;
            }
            // Every sibling is validated independently; a moved pane is never
            // controlled just because its ticket has been denied.
            for sibling in data.agents.iter().filter(|a| {
                a.ticket == rec.ticket && a.name != rec.name && a.role != "lead" && active(a.status)
            }) {
                if let Some(info) = ctx.h.agent_get(&sibling.name)?
                    && owned(&data, sibling, &info)
                {
                    let _ = ctx.h.send_keys(&sibling.name, &["esc".into()]);
                    if let Some(tab) = &sibling.tab_id {
                        ctx.h.close_tab(tab)?;
                    }
                    ctx.store.update(|d| {
                        if let Some(a) = d
                            .agents
                            .iter_mut()
                            .find(|a| a.name == sibling.name && active(a.status))
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
            ctx.h.prompt_agent(&rec.name, text, false)?;
            ctx.store.update(|d| {
                if let Some(a) = d
                    .agents
                    .iter_mut()
                    .find(|a| a.name == rec.name && active(a.status))
                {
                    a.nudges = a.nudges.saturating_add(1);
                    let mut obs = prior(a);
                    obs.last_nudge_at = Some(ctx.now_s);
                    remember(a, &obs);
                }
                Ok(())
            })?;
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
    if !owned(&data, rec, &own_info) || ctx.h.agents()?.iter().any(|a| a.pane_id == pane) {
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
    ctx.store.update(|data| {
        let a = data
            .agents
            .iter_mut()
            .find(|a| a.name == rec.name)
            .expect("owned record");
        a.restarts = a.restarts.saturating_add(1);
        a.status = AgentStatus::Starting;
        Ok(())
    })?;
    let project = ctx.store.read()?.project_root;
    let result = ctx.h.start_agent(
        &rec.name,
        harness.kind(),
        pane,
        &harness.resume_args(runner, session.as_deref()),
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
    if let Some(tab) = &rec.tab_id {
        ctx.h.close_tab(tab)?;
    }
    let label = child
        .filter(|child| child.unit > 1 && rec.role == "coder")
        .map(|child| format!("coder-{}", child.unit))
        .unwrap_or_else(|| rec.role.clone());
    let (tab, pane) = ctx.h.create_tab(workspace, &label, &rec.worktree)?;
    ctx.store.update(|d| {
        let a = d
            .agents
            .iter_mut()
            .find(|a| a.name == rec.name)
            .expect("owned record");
        a.runner = runner.name.clone();
        a.tab_id = Some(tab);
        a.pane_id = Some(pane.clone());
        a.status = AgentStatus::Starting;
        a.prompt_pending = true;
        a.nudges = 0;
        a.last_progress_at = ctx.now_s.to_string();
        Ok(())
    })?;
    let harness = harness_for(&runner.harness)?;
    let start = ctx.h.start_agent(
        &rec.name,
        harness.kind(),
        &pane,
        &harness.launch_args(runner),
        &launch_env(ctx.state_dir, &data.project_root),
    )?;
    let status = if start == AgentStart::Ready {
        match ctx.h.prompt_agent(&rec.name, &rec.task, true) {
            Ok(()) => AgentStatus::Working,
            Err(error) if error.prompt_was_submitted() => AgentStatus::Working,
            Err(error) if error.herdr_code() == Some("agent_blocked") => AgentStatus::Blocked,
            Err(error) => return Err(error),
        }
    } else {
        AgentStatus::Blocked
    };
    ctx.store.update(|d| {
        if let Some(a) = d
            .agents
            .iter_mut()
            .find(|a| a.name == rec.name && active(a.status))
        {
            a.status = status;
            a.prompt_pending = status != AgentStatus::Working;
        }
        Ok(())
    })?;
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
    let data = store.read()?;
    let Some(info) = h.agent_get(&rec.name)? else {
        return Ok(());
    };
    if !owned(&data, rec, &info) || !matches!(info.agent_status.as_str(), "idle" | "done") {
        return Ok(());
    }
    match crate::flock::deliver_pending_prompt(h, store, &rec.name) {
        Ok(_) => Ok(()),
        Err(error)
            if error.herdr_code() == Some("agent_blocked") || error.prompt_was_submitted() =>
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
    let _guard = project_lock(store)?;
    let resolved = roles::resolve(cfg)?;
    let mut report = SweepReport {
        projects: 1,
        ..Default::default()
    };
    let data = store.read()?;
    let default = crate::trusted::default_branch(&data.project_root, state_dir)?;
    for mut rec in data.agents.iter().filter(|a| active(a.status)).cloned() {
        let _ticket_operation = crate::ticket::operation::shared(store, &rec.ticket)?;
        let Some(_operation) = crate::flock::try_agent_operation(store, &rec.name)? else {
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
            let (condition, live) = observe_at(h, &mut rec, &*harness, now)?;
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
            save_observation(store, &rec)?;
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
                if matches!(
                    decision,
                    Decision::AskLead { .. } | Decision::Escalate { .. } | Decision::Deny { .. }
                ) && observation.last_action.as_deref() == Some(format!("{decision:?}").as_str())
                {
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
        save_observation(&store, &rec).unwrap();
        assert!(!store.read().unwrap().agents[0].prompt_pending);
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
