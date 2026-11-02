//! Automatic effects share recipient checks and durable prompt claims. Explicit
//! operator cancellation deliberately remains a separate escape hatch.
use crate::{
    error::ShepherdError,
    herdr::{AgentInfo, Herdr},
    store::{AgentRecord, AgentStatus, PromptClaim, Store},
};

fn active(rec: &AgentRecord) -> bool {
    matches!(
        rec.status,
        AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
    ) && rec.report.is_none()
}

/// Caller holds the recipient's operation lock. Recheck after transport reads:
/// reports do not wait for that lock, and a name alone is not terminal ownership.
pub(crate) fn recipient(
    h: &dyn Herdr,
    store: &Store,
    expected: &AgentRecord,
    reported: bool,
) -> Result<Option<AgentInfo>, ShepherdError> {
    let data = store.read()?;
    if !data.agents.contains(expected)
        || expected.prompt_delivery_uncertain()
        || !(active(expected) || reported && expected.status == AgentStatus::Reported)
    {
        return Ok(None);
    }
    let info = match h.agent_get(&expected.name)? {
        Some(info) => info,
        None if reported && expected.status == AgentStatus::Reported => {
            let Some(pane) = expected.pane_id.as_deref() else {
                return Ok(None);
            };
            // An exited reported process may leave its owned shell behind.
            // Never close a same-pane replacement or an unverified saved tab.
            if h.agents()?.iter().any(|a| a.pane_id == pane) {
                return Ok(None);
            }
            AgentInfo {
                name: Some(expected.name.clone()),
                agent: String::new(),
                agent_status: "exited".into(),
                cwd: h.pane_cwd(pane)?,
                pane_id: pane.into(),
                workspace_id: None,
            }
        }
        None => return Ok(None),
    };
    let session = h.pane_session_ref(&info.pane_id)?;
    let fresh = store.read()?;
    if !crate::supervise::owned(&fresh, expected, &info)
        || !crate::supervise::same_session(expected, session.as_deref())
        || !fresh.agents.contains(expected)
    {
        return Ok(None);
    }
    Ok(Some(info))
}

pub(crate) enum Prompt<'a> {
    Initial,
    Followup {
        text: &'a str,
        kind: &'a str,
        nudge_at: Option<u64>,
        queued: bool,
    },
}

/// Claim before invoking a non-idempotent transport. Unknown input retains its
/// payload outside the retry queue, including process interruption during input.
/// Only a proven pre-input rejection restores the original intent and clocks.
pub(crate) fn dispatch(
    h: &dyn Herdr,
    store: &Store,
    expected: &AgentRecord,
    prompt: Prompt<'_>,
) -> Result<bool, ShepherdError> {
    let (mut claim, nudge_at, queued, initial, recipient_info) = match prompt {
        Prompt::Initial => (
            PromptClaim {
                task_id: expected.task_id.clone(),
                attempt_id: expected.attempt_id.clone(),
                kind: "initial".into(),
                text: expected.task.clone(),
                queued_ping: None,
            },
            None,
            false,
            true,
            None,
        ),
        Prompt::Followup {
            text,
            kind,
            nudge_at,
            queued,
        } => {
            let Some(info) = recipient(h, store, expected, false)? else {
                return Ok(false);
            };
            if queued && !matches!(info.agent_status.as_str(), "idle" | "done") {
                return Ok(false);
            }
            (
                PromptClaim {
                    task_id: expected.task_id.clone(),
                    attempt_id: expected.attempt_id.clone(),
                    kind: kind.into(),
                    text: text.into(),
                    queued_ping: None,
                },
                nudge_at,
                queued,
                false,
                Some(info),
            )
        }
    };
    let claimed = store.update(|data| {
        if recipient_info
            .as_ref()
            .is_some_and(|info| !crate::supervise::owned(data, expected, info))
        {
            return Ok(false);
        }
        if queued {
            // Recipient reads may run callbacks or take time. Revalidate under
            // the state lock immediately before claiming the queued intent.
            let healed = data.heal_pending_pings(crate::launch::now().parse().unwrap_or(0));
            if healed > 0 {
                eprintln!("shepherd: healed {healed} pending notices");
            }
            if data.pending_pings.first().map(|ping| ping.text.as_str())
                != Some(claim.text.as_str())
            {
                return Ok(false);
            }
        }
        let Some(index) = data.agents.iter().position(|rec| rec == expected) else {
            return Ok(false);
        };
        let rec = &data.agents[index];
        if !active(rec) || rec.prompt_delivery_uncertain() || initial != rec.prompt_pending {
            return Ok(false);
        }
        if queued {
            claim.queued_ping = Some(data.pending_pings.remove(0));
        }
        let rec = &mut data.agents[index];
        rec.prompt_pending = false;
        rec.status = AgentStatus::Blocked;
        rec.set_prompt_delivery_uncertain(true);
        rec.set_prompt_claim(Some(claim.clone()));
        if let Some(now) = nudge_at {
            rec.nudges = rec.nudges.saturating_add(1);
            let mut observation: serde_json::Value =
                serde_json::from_str(rec.observed.as_deref().unwrap()).expect("claim observation");
            observation["last_nudge_at"] = now.into();
            rec.observed = Some(observation.to_string());
        }
        Ok(true)
    })?;
    if !claimed {
        return Ok(false);
    }
    let delivery = h
        .prompt_agent(&expected.name, &claim.text, initial)
        .map_err(ShepherdError::into_prompt_error);
    let uncertain = delivery
        .as_ref()
        .is_err_and(|error| error.prompt_submission_uncertain());
    store.update(|data| {
        if queued && delivery.is_err() && !uncertain {
            // Notification claims reserve queue capacity during transport. A
            // report may clear that claim and allow one concurrent enqueue;
            // retain this older request even then. The project-wide ping lock
            // allows at most one restoration overflow, and future enqueue
            // refuses the full queue until it drains.
            data.pending_pings.insert(
                0,
                claim
                    .queued_ping
                    .clone()
                    .expect("queued claim retains its entry"),
            );
        }
        // A real report or retirement wins, even when it arrives during input.
        let Some(rec) = data.agents.iter_mut().find(|rec| {
            rec.name == expected.name
                && active(rec)
                && rec.prompt_claim().as_ref() == Some(&claim)
                && rec.pane_id == expected.pane_id
                && rec.runner == expected.runner
                && rec.worktree == expected.worktree
        }) else {
            return Ok(());
        };
        if uncertain {
            return Ok(());
        }
        if delivery.is_err() {
            // Restore only our fields; leave independently persisted evidence alone.
            rec.nudges = expected.nudges;
            let mut observation: serde_json::Value =
                serde_json::from_str(rec.observed.as_deref().unwrap()).expect("claim observation");
            let prior: serde_json::Value = expected
                .observed
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            observation["last_nudge_at"] = prior["last_nudge_at"].clone();
            rec.observed = Some(observation.to_string());
        }
        rec.prompt_pending = initial && delivery.is_err();
        rec.status = if initial {
            if delivery.is_ok() {
                AgentStatus::Working
            } else {
                AgentStatus::Blocked
            }
        } else {
            expected.status
        };
        rec.set_prompt_delivery_uncertain(false);
        rec.set_prompt_claim(None);
        Ok(())
    })?;
    delivery.map(|_| true)
}

/// Nonblocking automatic retirement. A deferred retirement is NOT cleanup
/// success: callers must retain the worktree and retry only after resolution.
pub fn retire(h: &dyn Herdr, store: &Store, expected: &AgentRecord) -> Result<bool, ShepherdError> {
    retire_inner(h, store, expected, None)
}

/// The integration receipt also journals tab closure. A missing terminal is
/// completion evidence only after its exact owned recipient was persisted.
pub(crate) fn retire_for_cleanup(
    h: &dyn Herdr,
    store: &Store,
    expected: &AgentRecord,
    receipt: &crate::store::ChildCleanup,
) -> Result<bool, ShepherdError> {
    retire_inner(h, store, expected, Some(receipt))
}

fn cleanup_current(
    data: &crate::store::StoreData,
    rec: &AgentRecord,
    receipt: &crate::store::ChildCleanup,
) -> bool {
    data.agents.contains(rec)
        && data.tickets.iter().any(|ticket| {
            ticket.id == rec.ticket
                && ticket.children.contains(&receipt.child)
                && ticket.child_cleanup.contains(receipt)
        })
}

fn retire_inner(
    h: &dyn Herdr,
    store: &Store,
    expected: &AgentRecord,
    cleanup: Option<&crate::store::ChildCleanup>,
) -> Result<bool, ShepherdError> {
    let name = &expected.name;
    let Some(_operation) = super::try_agent_operation(store, name)? else {
        return Ok(false);
    };
    let Some(rec) = store.read()?.agents.into_iter().find(|a| a == expected) else {
        return Ok(false);
    };
    if rec.status == AgentStatus::Retired {
        return Ok(true);
    }
    let mut cleanup = cleanup.cloned();
    if let Some(receipt) = &cleanup {
        if !cleanup_current(&store.read()?, &rec, receipt) {
            return Ok(false);
        }
        if receipt.retiring_agents.contains(&rec)
            && let Some(pane) = &rec.pane_id
            && h.agent_get(name)?.is_none()
            && !h
                .agents()?
                .iter()
                .any(|a| a.pane_id == *pane || a.name.as_deref() == Some(name))
            && !h.pane_exists(pane)?
        {
            return store.update(|data| {
                if !cleanup_current(data, &rec, receipt) {
                    return Ok(false);
                }
                data.agents.iter_mut().find(|a| **a == rec).unwrap().status = AgentStatus::Retired;
                Ok(true)
            });
        }
    }
    let Some(info) = recipient(h, store, &rec, true)? else {
        return Ok(false);
    };
    if rec.status != AgentStatus::Reported
        && !matches!(info.agent_status.as_str(), "exited" | "stopped")
    {
        h.send_keys(name, &["esc".into()])?;
    }
    if recipient(h, store, &rec, true)?.is_none() {
        return Ok(false);
    }
    if let Some(receipt) = &mut cleanup
        && !receipt.retiring_agents.contains(&rec)
    {
        let saved = store.update(|data| {
            if !cleanup_current(data, &rec, receipt) {
                return Ok(false);
            }
            data.tickets
                .iter_mut()
                .find(|t| t.id == rec.ticket)
                .unwrap()
                .child_cleanup
                .iter_mut()
                .find(|r| *r == receipt)
                .unwrap()
                .retiring_agents
                .push(rec.clone());
            Ok(true)
        })?;
        if !saved {
            return Ok(false);
        }
        receipt.retiring_agents.push(rec.clone());
        if recipient(h, store, &rec, true)?.is_none() {
            return Ok(false);
        }
    }
    if let Some(tab) = &rec.tab_id {
        h.close_tab(tab)?;
    }
    store.update(|data| {
        if cleanup
            .as_ref()
            .is_some_and(|receipt| !cleanup_current(data, &rec, receipt))
        {
            return Ok(false);
        }
        let Some(current) = data.agents.iter_mut().find(|a| **a == rec) else {
            return Ok(false);
        };
        current.status = AgentStatus::Retired;
        Ok(true)
    })
}
