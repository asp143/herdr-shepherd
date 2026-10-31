//! Choose ticket work without performing side effects.

use serde::Serialize;

use crate::{
    roles::ShepherdSettings,
    store::{AgentStatus, StoreData, TicketState},
    ticket::state::is_active,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    StartTicket(String),
    ResumeRevising(String),
}

pub fn next_actions(data: &StoreData, settings: &ShepherdSettings) -> Vec<Action> {
    let has_agent = |id: &str| {
        data.agents.iter().any(|agent| {
            agent.ticket == id
                && matches!(
                    agent.status,
                    AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                )
        })
    };
    // A waiting revision already has an active state, but needs a dispatch slot.
    // Count it when selecting revisions, rather than once here and once there.
    let occupied = data
        .tickets
        .iter()
        .filter(|ticket| {
            is_active(ticket.state)
                && (ticket.state != TicketState::Revising || has_agent(&ticket.id))
        })
        .count();
    let slots = settings.max_concurrent_tickets.saturating_sub(occupied);
    let mut revising: Vec<_> = data
        .tickets
        .iter()
        .filter(|ticket| ticket.state == TicketState::Revising && !has_agent(&ticket.id))
        .collect();
    let mut queued: Vec<_> = data
        .tickets
        .iter()
        .filter(|ticket| ticket.state == TicketState::Queued)
        .collect();
    revising.sort_by_key(|ticket| (&ticket.created_at, &ticket.id));
    queued.sort_by_key(|ticket| (&ticket.created_at, &ticket.id));
    revising
        .into_iter()
        .map(|ticket| Action::ResumeRevising(ticket.id.clone()))
        .chain(
            queued
                .into_iter()
                .map(|ticket| Action::StartTicket(ticket.id.clone())),
        )
        .take(slots)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Store, Ticket, TicketState};

    fn data(states: &[TicketState]) -> crate::store::StoreData {
        let dir = tempfile::tempdir().unwrap();
        let mut data = Store::open(dir.path(), dir.path()).unwrap().read().unwrap();
        data.tickets = states
            .iter()
            .enumerate()
            .map(|(n, state)| Ticket {
                id: format!("T{n}"),
                state: *state,
                created_at: format!("{n:04}"),
                ..Ticket::default()
            })
            .collect();
        data
    }

    #[test]
    fn revising_before_queued() {
        let data = data(&[TicketState::Queued, TicketState::Revising]);
        let settings = crate::roles::ShepherdSettings {
            max_concurrent_tickets: 1,
            ..Default::default()
        };
        assert_eq!(
            next_actions(&data, &settings),
            vec![Action::ResumeRevising("T1".into())]
        );
    }

    #[test]
    fn long_queue_never_blocks_revising() {
        let mut states = vec![TicketState::Queued; 200];
        states.push(TicketState::Revising);
        let data = data(&states);
        let settings = crate::roles::ShepherdSettings {
            max_concurrent_tickets: 1,
            ..Default::default()
        };
        assert_eq!(
            next_actions(&data, &settings),
            vec![Action::ResumeRevising("T200".into())]
        );
    }

    #[test]
    fn respects_max_concurrent() {
        let data = data(&[
            TicketState::Planning,
            TicketState::Building,
            TicketState::Queued,
        ]);
        assert!(next_actions(&data, &Default::default()).is_empty());
    }

    #[test]
    fn pr_open_frees_slot() {
        let data = data(&[
            TicketState::PrOpen,
            TicketState::Planning,
            TicketState::Queued,
        ]);
        assert_eq!(
            next_actions(&data, &Default::default()),
            vec![Action::StartTicket("T2".into())]
        );
    }

    #[test]
    fn queue_uses_creation_order_and_ci_wait_consumes_slot() {
        let mut data = data(&[
            TicketState::Queued,
            TicketState::CiWait,
            TicketState::Queued,
        ]);
        data.tickets[0].created_at = "later".into();
        data.tickets[2].created_at = "earlier".into();
        assert_eq!(
            next_actions(&data, &Default::default()),
            vec![Action::StartTicket("T2".into())]
        );
    }
}
