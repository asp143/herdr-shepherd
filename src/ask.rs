//! User messages share the lead's durable notification queue.
use crate::{error::ShepherdError, herdr::Herdr, store::Store};
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
    let Some(lead) = store.read()?.lead else {
        return Ok(false);
    };
    Ok(h.agent_get(&lead.agent_name)?.is_some())
}

/// Persist the complete request before attempting the shared delivery path.
pub fn send(h: &dyn Herdr, store: &Store, text: &str) -> Result<AskReport, ShepherdError> {
    validate_text(text)?;
    let agent_name = store.update(|data| {
        let lead = data
            .lead
            .as_ref()
            .ok_or_else(|| ShepherdError::Usage("project has no running lead".into()))?;
        let name = lead.agent_name.clone();
        data.pending_pings.push(text.into());
        Ok(name)
    })?;
    crate::lead::flush_pings(h, store)?;
    Ok(AskReport {
        agent_name,
        queued: !store.read()?.pending_pings.is_empty(),
        started: false,
    })
}
