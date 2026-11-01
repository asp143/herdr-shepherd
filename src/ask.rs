//! User messages share the lead's durable notification queue.
use crate::{
    error::ShepherdError,
    herdr::Herdr,
    store::{AgentRecord, AgentStatus, Store, StoreData},
};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct AskReport {
    pub agent_name: String,
    pub queued: bool,
    pub started: bool,
}

pub fn validate_text(text: &str) -> Result<(), ShepherdError> {
    if text.trim().is_empty() {
        return Err(ShepherdError::Usage("ask text must not be empty".into()));
    }
    Ok(())
}

pub fn lead_running(h: &dyn Herdr, store: &Store) -> Result<bool, ShepherdError> {
    Ok(running_lead(h, store)?.is_some())
}

fn active_lead(data: &StoreData) -> Option<&AgentRecord> {
    let lead = data.lead.as_ref()?;
    data.agents.iter().find(|rec| {
        rec.name == lead.agent_name
            && rec.role == "lead"
            && rec.report.is_none()
            && matches!(
                rec.status,
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
            )
    })
}

fn running_lead(h: &dyn Herdr, store: &Store) -> Result<Option<AgentRecord>, ShepherdError> {
    let data = store.read()?;
    let Some(rec) = active_lead(&data) else {
        return Ok(None);
    };
    let Some(live) = h.agent_get(&rec.name)? else {
        return Ok(None);
    };
    let session = h.pane_session_ref(&live.pane_id)?;
    let fresh = store.read()?;
    if active_lead(&fresh) != Some(rec) || fresh.lead != data.lead {
        return Err(ShepherdError::Usage(
            "lead changed while checking delivery; retry ask".into(),
        ));
    }
    if !crate::supervise::owned(&fresh, rec, &live)
        || !crate::supervise::same_session(rec, session.as_deref())
    {
        return Err(ShepherdError::PolicyDenied(
            "lead terminal no longer matches its saved ownership".into(),
        ));
    }
    Ok(Some(rec.clone()))
}

/// Persist the complete request before attempting the shared delivery path.
pub fn send(h: &dyn Herdr, store: &Store, text: &str) -> Result<AskReport, ShepherdError> {
    validate_text(text)?;
    let rec = running_lead(h, store)?
        .ok_or_else(|| ShepherdError::Usage("project has no running lead".into()))?;
    let agent_name = store.update(|data| {
        if active_lead(data) != Some(&rec) {
            return Err(ShepherdError::Usage(
                "lead changed before queueing request; retry ask".into(),
            ));
        }
        data.enqueue_lead_request(text)?;
        Ok(rec.name.clone())
    })?;
    if let Err(error) = crate::lead::flush_pings(h, store)
        && !matches!(
            error.herdr_code(),
            Some("agent_blocked" | "agent_not_ready")
        )
    {
        return Err(error);
    }
    Ok(AskReport {
        agent_name,
        queued: !store.read()?.pending_pings.is_empty(),
        started: false,
    })
}
