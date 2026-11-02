//! Admission and durable delivery for a reported harness's next assignment.
use super::*;
use crate::{
    flock::AgentOperationGuard,
    harness::ContextHealth,
    herdr::{TaskDelivery, TaskRecipient},
    store::{LaunchProfile, PromptClaim, StoreData},
};

pub(super) struct Transfer {
    pub original: AgentRecord,
    pub reserved: AgentRecord,
    pub _operation: AgentOperationGuard,
}

pub(super) enum Admission {
    Reused(Box<AgentRecord>),
    Fresh {
        reason: String,
        transfer: Option<Box<Transfer>>,
    },
}

pub(super) fn placement(store: &Store) -> Result<AgentOperationGuard, ShepherdError> {
    // Same file as explicit child placement. Distinct checkouts wait for
    // placement rather than losing an otherwise valid concurrent dispatch.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.root().join("spawn.lock"))
        .map_err(anyhow::Error::from)?;
    file.lock().map_err(anyhow::Error::from)?;
    Ok(AgentOperationGuard::from_file(file))
}

pub(super) fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

pub(super) fn unit(data: &StoreData, req: &LaunchRequest) -> Option<u32> {
    data.tickets
        .iter()
        .find(|t| t.id == req.ticket)
        .and_then(|t| {
            t.children
                .iter()
                .find(|c| canonical(&c.path) == canonical(&req.worktree))
        })
        .map(|c| c.unit)
}

pub(super) fn check_admission(
    data: &StoreData,
    req: &LaunchRequest,
    cfg: &Config,
) -> Result<(), ShepherdError> {
    let cwd = canonical(&req.worktree);
    if data.tickets.iter().any(|ticket| {
        ticket
            .child_cleanup
            .iter()
            .any(|r| cwd.starts_with(canonical(&r.path)))
    }) {
        return Err(ShepherdError::Usage(
            "child checkout has outstanding cleanup; finish cleanup before launching an agent"
                .into(),
        ));
    }
    if req.role.name != "lead"
        && data
            .tickets
            .iter()
            .find(|t| t.id == req.ticket)
            .is_some_and(|t| !crate::ticket::state::is_active(t.state))
    {
        return Err(ShepherdError::Usage("ticket is not active".into()));
    }
    let resolved = crate::roles::resolve(cfg)?;
    let writer = |role: &str| {
        role == "coder"
            || resolved
                .roles
                .iter()
                .any(|r| r.name == role && r.worktree == crate::roles::WorktreeMode::Child)
    };
    if req.role.name != "lead"
        && data
            .agents
            .iter()
            .filter(|a| {
                a.role != "lead"
                    && matches!(
                        a.status,
                        AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                    )
            })
            .count()
            >= resolved.shepherd.max_parallel_agents
    {
        return Err(ShepherdError::Capacity);
    }
    if writer(&req.role.name)
        && data.agents.iter().any(|a| {
            writer(&a.role)
                && canonical(&a.worktree) == cwd
                && (matches!(
                    a.status,
                    AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                ) || a.status == AgentStatus::Reported && a.prompt_delivery_uncertain())
        })
    {
        return Err(ShepherdError::Usage(
            "checkout already has an active or uncertain writer".into(),
        ));
    }
    Ok(())
}

fn ineligible(
    data: &StoreData,
    req: &LaunchRequest,
    rec: &AgentRecord,
    cap: usize,
) -> Option<&'static str> {
    if rec.status != AgentStatus::Reported {
        return Some("agent is not reported (active, cancelled, retired or failed)");
    }
    if rec
        .dispatch_reason
        .as_deref()
        .is_some_and(|r| r.starts_with("unavailable:"))
    {
        return Some("earlier delivery failed");
    }
    if rec.prompt_delivery_uncertain() || rec.prompt_claim().is_some() {
        return Some("uncertain or unresolved prompt claim");
    }
    if rec.task_id.as_deref().is_none_or(str::is_empty)
        || rec.attempt_id.as_deref().is_none_or(str::is_empty)
        || rec
            .completed_tasks
            .last()
            .is_none_or(|c| c.task_id != rec.task_id || c.attempt_id != rec.attempt_id)
        || rec.report.as_ref().is_none_or(|r| r.task_id != rec.task_id)
    {
        return Some("legacy or unverifiable assignment identity");
    }
    if rec.unit != unit(data, req) {
        return Some("unit differs");
    }
    if canonical(&rec.worktree) != canonical(&req.worktree) {
        return Some("checkout differs");
    }
    if rec.unit.is_some()
        && data
            .tickets
            .iter()
            .find(|t| t.id == req.ticket)
            .is_none_or(|t| {
                !t.children.iter().any(|c| {
                    Some(c.unit) == rec.unit
                        && !c.integrated
                        && canonical(&c.path) == canonical(&rec.worktree)
                })
            })
    {
        return Some("child was integrated or removed");
    }
    if rec.launch_profile.as_ref().is_none_or(|profile| {
        rec.runner != profile.name
            || !req
                .runners
                .iter()
                .any(|runner| LaunchProfile::from(runner) == *profile)
    }) {
        return Some("launch profile differs");
    }
    if rec.completed_tasks.len() >= cap {
        return Some("completed assignment cap reached");
    }
    if rec.pane_id.as_deref().is_none_or(str::is_empty)
        || rec.tab_id.as_deref().is_none_or(str::is_empty)
    {
        return Some("missing pane or tab identity");
    }
    None
}

fn session(rec: &AgentRecord) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(rec.observed.as_deref()?).ok()?["session_ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(super) fn admit(
    h: &dyn Herdr,
    store: &Store,
    req: &LaunchRequest,
    cfg: &Config,
) -> Result<Admission, ShepherdError> {
    let data = store.read()?;
    check_admission(&data, req, cfg)?;
    if req.role.name == "lead" {
        return Ok(Admission::Fresh {
            reason: "fresh: lead launch".into(),
            transfer: None,
        });
    }
    let resolved = crate::roles::resolve(cfg)?;
    let writer = |role: &str| {
        role == "coder"
            || resolved
                .roles
                .iter()
                .any(|r| r.name == role && r.worktree == crate::roles::WorktreeMode::Child)
    };
    if writer(&req.role.name) {
        for old in data.agents.iter().filter(|a| {
            a.status == AgentStatus::Reported
                && writer(&a.role)
                && canonical(&a.worktree) == canonical(&req.worktree)
        }) {
            if h.agent_get(&old.name).ok().flatten().is_some_and(|info| {
                crate::supervise::owned_terminal(old, &info)
                    && !matches!(
                        info.agent_status.as_str(),
                        "idle" | "done" | "exited" | "stopped"
                    )
            }) {
                return Err(ShepherdError::Usage(
                    "reported writer is still busy; wait for idle before dispatch".into(),
                ));
            }
        }
    }
    let cap = resolved.shepherd.max_completed_assignments;
    let mut reasons = Vec::new();
    for old in data
        .agents
        .iter()
        .rev()
        .filter(|a| a.ticket == req.ticket && a.role == req.role.name)
    {
        if let Some(reason) = ineligible(&data, req, old, cap) {
            reasons.push(format!("{}: {reason}", old.name));
            continue;
        }
        let Some(operation) = crate::flock::try_agent_operation(store, &old.name)? else {
            reasons.push(format!("{}: operation busy", old.name));
            continue;
        };
        let Some(reference) = session(old) else {
            reasons.push(format!("{}: missing session identity", old.name));
            continue;
        };
        let live = match h.agent_get(&old.name) {
            Ok(Some(live)) => live,
            Ok(None) => {
                reasons.push(format!("{}: agent is dead", old.name));
                continue;
            }
            Err(_) => {
                reasons.push(format!("{}: agent is unresponsive", old.name));
                continue;
            }
        };
        let profile = old.launch_profile.as_ref().expect("verified profile");
        if !crate::supervise::owned(&store.read()?, old, &live)
            || live.agent != profile.harness
            || h.pane_session_ref(&live.pane_id).ok().flatten().as_deref() != Some(&reference)
        {
            reasons.push(format!("{}: pane, session or ownership differs", old.name));
            continue;
        }
        if !matches!(live.agent_status.as_str(), "idle" | "done") {
            if req.role.name == "coder" || req.role.worktree == crate::roles::WorktreeMode::Child {
                return Err(ShepherdError::Usage(
                    "reported writer is still busy; wait for idle before dispatch".into(),
                ));
            }
            reasons.push(format!("{}: live agent is busy", old.name));
            continue;
        }
        let screen = match h.read_visible(&old.name) {
            Ok(s) => s,
            Err(_) => {
                reasons.push(format!("{}: agent is unresponsive", old.name));
                continue;
            }
        };
        let harness = crate::harness::harness_for(&profile.harness)?;
        if matches!(
            harness.context_health(&screen),
            ContextHealth::NearLimit | ContextHealth::Exhausted
        ) {
            reasons.push(format!("{}: context limit reached", old.name));
            continue;
        }
        let reserved = store.update(|data| {
            check_admission(data, req, cfg)?;
            let Some(rec) = data.agents.iter_mut().find(|a| *a == old) else {
                return Err(ShepherdError::Usage(
                    "reuse recipient changed; retry dispatch".into(),
                ));
            };
            let task_id = new_task_id(&rec.name);
            rec.task = req
                .prompt
                .replace("{{agent_name}}", &rec.name)
                .replace("{{task_id}}", &task_id)
                .replace("{{subagent_hint}}", harness.subagent_hint());
            rec.attempt_id = Some(format!("{task_id}-1"));
            rec.task_id = Some(task_id);
            rec.report = None;
            rec.status = AgentStatus::Blocked;
            rec.prompt_pending = false;
            rec.observed = Some(
                serde_json::json!({"tab_workspace_id":req.workspace_id,"session_ref":reference})
                    .to_string(),
            );
            rec.last_progress_at = now();
            rec.nudges = 0;
            rec.restarts = 0;
            rec.dispatch_reason = Some("reused: owned idle agent; context health eligible".into());
            rec.set_prompt_delivery_uncertain(true);
            rec.set_prompt_claim(Some(PromptClaim {
                task_id: rec.task_id.clone(),
                attempt_id: rec.attempt_id.clone(),
                kind: "assignment".into(),
                text: rec.task.clone(),
                queued_ping: None,
            }));
            Ok(rec.clone())
        })?;
        let recipient = TaskRecipient {
            name: reserved.name.clone(),
            harness: profile.harness.clone(),
            pane_id: live.pane_id,
            session_ref: reference,
        };
        // Err is guaranteed pre-input by the transport contract. Panics leave
        // the persisted uncertain claim in place for recovery, never a replay.
        let delivery = h
            .deliver_task(&recipient, &reserved.task)
            .unwrap_or_else(|_| TaskDelivery::Rejected {
                reason: "preflight failed before input".into(),
            });
        let current = store
            .read()?
            .agents
            .into_iter()
            .find(|a| a.name == reserved.name)
            .expect("reserved record retained");
        if current.task_id != reserved.task_id
            || current.attempt_id != reserved.attempt_id
            || current.report.is_some()
            || current.status == AgentStatus::Retired
        {
            return Ok(Admission::Reused(Box::new(current)));
        }
        let failure = match delivery {
            TaskDelivery::Accepted => {
                let rec = store.update(|data| {
                    let rec = data
                        .agents
                        .iter_mut()
                        .find(|a| a.name == reserved.name)
                        .expect("record retained");
                    if rec.task_id == reserved.task_id
                        && rec.attempt_id == reserved.attempt_id
                        && rec.report.is_none()
                        && rec.status != AgentStatus::Retired
                    {
                        rec.status = AgentStatus::Working;
                        rec.set_prompt_delivery_uncertain(false);
                        rec.set_prompt_claim(None);
                    }
                    Ok(rec.clone())
                })?;
                return Ok(Admission::Reused(Box::new(rec)));
            }
            TaskDelivery::Rejected { reason } | TaskDelivery::Dead { reason } => reason,
            TaskDelivery::Ambiguous { reason } => {
                // A missing name alone does not prove termination. Only a
                // vanished exact pane plus no process on it permits transfer.
                let stopped = h
                    .pane_exists(&recipient.pane_id)
                    .is_ok_and(|exists| !exists)
                    && h.agents()
                        .is_ok_and(|agents| !agents.iter().any(|a| a.pane_id == recipient.pane_id));
                if !stopped {
                    let rec = store.update(|data| {
                        let rec = data
                            .agents
                            .iter_mut()
                            .find(|a| a.name == reserved.name)
                            .expect("record retained");
                        if rec.task_id == reserved.task_id
                            && rec.attempt_id == reserved.attempt_id
                            && rec.report.is_none()
                            && rec.status != AgentStatus::Retired
                        {
                            rec.dispatch_reason = Some(format!(
                                "reused: delivery ambiguous; task pending: {reason}"
                            ));
                        }
                        Ok(rec.clone())
                    })?;
                    return Ok(Admission::Reused(Box::new(rec)));
                }
                format!("ambiguous delivery; exact pane stopped: {reason}")
            }
        };
        let mut original = old.clone();
        if writer(&req.role.name)
            && let Some(live) = h.agent_get(&reserved.name).ok().flatten()
            && !matches!(
                live.agent_status.as_str(),
                "idle" | "done" | "exited" | "stopped"
            )
        {
            // A pre-input rejection may mean the old reported turn resumed
            // before delivery. Stop only our exact owned busy recipient before
            // starting another writer in its checkout. Unverified termination
            // leaves the durable task quarantined instead of duplicating it.
            let fresh = store.read()?;
            let can_stop = fresh.agents.contains(&reserved)
                && crate::supervise::owned(&fresh, &reserved, &live)
                && live.agent == recipient.harness
                && h.pane_session_ref(&recipient.pane_id)
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(recipient.session_ref.as_str());
            let stopped = can_stop
                && h.close_tab(reserved.tab_id.as_deref().expect("verified tab"))
                    .is_ok()
                && h.pane_exists(&recipient.pane_id)
                    .is_ok_and(|exists| !exists)
                && h.agents()
                    .is_ok_and(|agents| !agents.iter().any(|a| a.pane_id == recipient.pane_id));
            if !stopped {
                let current = store.update(|data| {
                    let rec = data.agents.iter_mut().find(|a| a.name == reserved.name).expect("record retained");
                    if rec.task_id == reserved.task_id && rec.attempt_id == reserved.attempt_id && rec.report.is_none() && rec.status != AgentStatus::Retired {
                        rec.dispatch_reason = Some("reused: rejected delivery; task pending until busy predecessor is confirmed stopped".into());
                    }
                    Ok(rec.clone())
                })?;
                return Ok(Admission::Reused(Box::new(current)));
            }
            // Transfer restores this retired predecessor in the same store
            // transaction that reserves its replacement. That transaction's
            // caller emits agent.retired only after the retirement is durable.
            original.status = AgentStatus::Retired;
        }
        return Ok(Admission::Fresh {
            reason: format!("fresh: reuse delivery failed: {failure}"),
            transfer: Some(Box::new(Transfer {
                original,
                reserved,
                _operation: operation,
            })),
        });
    }
    Ok(Admission::Fresh {
        reason: if reasons.is_empty() {
            "fresh: no existing role agent".into()
        } else {
            format!("fresh: {}", reasons.join("; "))
        },
        transfer: None,
    })
}
