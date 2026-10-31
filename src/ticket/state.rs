use crate::{
    error::ShepherdError,
    store::{BlockedReason, Ticket, TicketState},
};

/// Apply a transition with the default revision cap of three rounds.
pub fn transition(
    t: &mut Ticket,
    to: TicketState,
    reason: Option<BlockedReason>,
    now: &str,
) -> Result<(), ShepherdError> {
    transition_with_limit(t, to, reason, now, 3)
}

/// Apply a transition using the project's configured maximum revision rounds.
pub fn transition_with_limit(
    t: &mut Ticket,
    to: TicketState,
    reason: Option<BlockedReason>,
    now: &str,
    max_revision_rounds: u32,
) -> Result<(), ShepherdError> {
    use TicketState::*;
    if max_revision_rounds == 0 {
        return Err(ShepherdError::Usage(
            "max_revision_rounds must be at least 1".into(),
        ));
    }
    if (to == Blocked) != reason.is_some() {
        return Err(ShepherdError::Usage(
            "a blocked reason is required only when entering blocked".into(),
        ));
    }
    let allowed = matches!(
        (t.state, to),
        (Queued, Planning)
            | (Planning, Researching | Building)
            | (Researching, Building)
            | (Building, Reviewing)
            | (Reviewing, Building | Qa)
            | (Qa, Building | ReadyForPr)
            | (ReadyForPr, CiWait | PrOpen)
            | (CiWait, Building | PrOpen)
            | (PrOpen, Revising | Merged | Closed)
            | (Revising, Building | Reviewing | Qa | CiWait | PrOpen)
            | (Merged, CleanedUp)
    );
    let blocked_origin = t
        .history
        .iter()
        .rev()
        .find(|entry| entry.to == Blocked && entry.from != Blocked)
        .map(|entry| entry.from);
    let allowed = allowed
        || (is_active(t.state) && matches!(to, Blocked | Failed))
        || (t.state == Blocked && blocked_origin == Some(to))
        || (to == Closed && !matches!(t.state, Merged | CleanedUp | Closed | Failed));
    if !allowed {
        return Err(ShepherdError::Usage(format!(
            "illegal ticket transition {:?} -> {to:?}",
            t.state
        )));
    }
    let mut to = to;
    let mut reason = reason;
    if matches!((t.state, to), (Reviewing | Qa | CiWait, Building)) {
        let next = t.revision_round.checked_add(1);
        t.revision_round = next.unwrap_or(u32::MAX);
        if next.is_none() || t.revision_round > max_revision_rounds {
            to = Blocked;
            reason = Some(BlockedReason::LoopCap);
        }
    }
    t.history.push(crate::store::Transition {
        from: t.state,
        to,
        reason,
        at: now.into(),
    });
    t.state = to;
    t.blocked_reason = reason;
    t.updated_at = now.into();
    Ok(())
}

/// Whether a state consumes a concurrent ticket slot.
pub fn is_active(s: TicketState) -> bool {
    use TicketState::*;
    matches!(
        s,
        Planning | Researching | Building | Reviewing | Qa | Revising | CiWait
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use TicketState::*;

    #[test]
    fn happy_path_edges() {
        let paths = [
            vec![
                Queued,
                Planning,
                Researching,
                Building,
                Reviewing,
                Qa,
                ReadyForPr,
                PrOpen,
                Merged,
                CleanedUp,
            ],
            vec![
                Queued, Planning, Building, Reviewing, Building, Reviewing, Qa, Building,
                Reviewing, Qa, ReadyForPr, PrOpen, Revising, Building,
            ],
            vec![PrOpen, Revising, Reviewing],
            vec![PrOpen, Revising, Qa],
            vec![PrOpen, Revising, PrOpen],
        ];
        for path in paths {
            let mut ticket = Ticket {
                state: path[0],
                ..Ticket::default()
            };
            for &next in &path[1..] {
                transition(&mut ticket, next, None, "now").unwrap();
                assert_eq!(ticket.state, next);
            }
        }
    }

    #[test]
    fn illegal_edge_rejected() {
        let mut ticket = Ticket::default();
        let before = ticket.clone();
        assert!(matches!(
            transition(&mut ticket, Qa, None, "later"),
            Err(ShepherdError::Usage(_))
        ));
        assert_eq!(ticket, before);
    }

    #[test]
    fn loop_cap_blocks() {
        let mut ticket = Ticket {
            state: Reviewing,
            ..Ticket::default()
        };
        for count in 1..=3 {
            transition(&mut ticket, Building, None, "bounce").unwrap();
            assert_eq!(ticket.state, Building);
            assert_eq!(ticket.revision_round, count);
            transition(&mut ticket, Reviewing, None, "review").unwrap();
        }
        transition(&mut ticket, Building, None, "capped").unwrap();
        assert_eq!(ticket.state, Blocked);
        assert_eq!(ticket.blocked_reason, Some(BlockedReason::LoopCap));
        assert_eq!(ticket.revision_round, 4);
        assert_eq!(ticket.history.last().unwrap().from, Reviewing);
        assert_eq!(ticket.history.last().unwrap().to, Blocked);
        assert_eq!(
            ticket.history.last().unwrap().reason,
            Some(BlockedReason::LoopCap)
        );
    }

    #[test]
    fn unblock_returns_to_prior() {
        for prior in [
            Planning,
            Researching,
            Building,
            Reviewing,
            Qa,
            Revising,
            CiWait,
        ] {
            let mut ticket = Ticket {
                state: prior,
                ..Ticket::default()
            };
            transition(&mut ticket, Blocked, Some(BlockedReason::Policy), "blocked").unwrap();
            let snapshot = ticket.clone();
            let wrong = if prior == Qa { Building } else { Qa };
            assert!(transition(&mut ticket, wrong, None, "wrong").is_err());
            assert_eq!(ticket, snapshot);
            transition(&mut ticket, prior, None, "resume").unwrap();
            assert_eq!(ticket.state, prior);
            assert_eq!(ticket.blocked_reason, None);
            assert_eq!(ticket.history[1].from, Blocked);
            assert_eq!(ticket.history[1].to, prior);
        }
    }

    #[test]
    fn active_states_match_final_state_list() {
        for state in [
            Planning,
            Researching,
            Building,
            Reviewing,
            Qa,
            Revising,
            CiWait,
        ] {
            assert!(is_active(state));
            let mut ticket = Ticket {
                state,
                ..Ticket::default()
            };
            transition(&mut ticket, Failed, None, "failure").unwrap();
            assert_eq!(ticket.state, Failed);
        }
        for state in [
            Queued, ReadyForPr, PrOpen, Merged, CleanedUp, Closed, Failed, Blocked,
        ] {
            assert!(!is_active(state));
        }
    }

    #[test]
    fn cancel_from_any_nonterminal() {
        for state in [
            Queued,
            Planning,
            Researching,
            Building,
            Reviewing,
            Qa,
            ReadyForPr,
            CiWait,
            PrOpen,
            Revising,
            Blocked,
        ] {
            let mut ticket = Ticket {
                state,
                ..Ticket::default()
            };
            transition(&mut ticket, Closed, None, "cancel").unwrap();
            assert_eq!(ticket.state, Closed);
            assert_eq!(ticket.history[0].from, state);
            assert_eq!(ticket.history[0].to, Closed);
        }
        for state in [Merged, CleanedUp, Closed, Failed] {
            let mut ticket = Ticket {
                state,
                ..Ticket::default()
            };
            assert!(transition(&mut ticket, Closed, None, "cancel").is_err());
        }
    }

    #[test]
    fn ci_wait_edges() {
        for path in [
            vec![ReadyForPr, CiWait, PrOpen],
            vec![Revising, CiWait, Building],
        ] {
            let mut ticket = Ticket {
                state: path[0],
                ..Ticket::default()
            };
            for &next in &path[1..] {
                transition(&mut ticket, next, None, "ci").unwrap();
                assert_eq!(ticket.state, next);
            }
        }
    }

    #[test]
    fn red_ci_counts_toward_loop_cap() {
        let mut ticket = Ticket {
            state: CiWait,
            revision_round: 3,
            ..Ticket::default()
        };
        transition(&mut ticket, Building, None, "red CI").unwrap();
        assert_eq!(ticket.state, Blocked);
        assert_eq!(ticket.revision_round, 4);
        assert_eq!(ticket.blocked_reason, Some(BlockedReason::LoopCap));
        assert_eq!(ticket.history[0].from, CiWait);
        assert_eq!(ticket.history[0].to, Blocked);
    }

    #[test]
    fn reasons_required_only_for_blocked() {
        for (from, to, reason) in [
            (Building, Blocked, None),
            (Building, Reviewing, Some(BlockedReason::Policy)),
            (Building, Failed, Some(BlockedReason::Escalated)),
        ] {
            let mut ticket = Ticket {
                state: from,
                ..Ticket::default()
            };
            let before = ticket.clone();
            assert!(matches!(
                transition(&mut ticket, to, reason, "invalid"),
                Err(ShepherdError::Usage(_))
            ));
            assert_eq!(ticket, before);
        }
    }

    #[test]
    fn unblock_without_history_rejected() {
        let mut ticket = Ticket {
            state: Blocked,
            blocked_reason: Some(BlockedReason::Escalated),
            ..Ticket::default()
        };
        let before = ticket.clone();
        assert!(transition(&mut ticket, Building, None, "invalid").is_err());
        assert_eq!(ticket, before);
    }

    #[test]
    fn configurable_loop_limit() {
        let mut ticket = Ticket {
            state: Qa,
            revision_round: 1,
            ..Ticket::default()
        };
        transition_with_limit(&mut ticket, Building, None, "limit", 1).unwrap();
        assert_eq!(ticket.state, Blocked);
        assert_eq!(ticket.blocked_reason, Some(BlockedReason::LoopCap));
        let mut ticket = Ticket::default();
        assert!(transition_with_limit(&mut ticket, Planning, None, "zero", 0).is_err());
        assert_eq!(ticket, Ticket::default());
    }

    #[test]
    fn transitions_record_history_and_timestamp() {
        let mut ticket = Ticket {
            created_at: "creation".into(),
            ..Ticket::default()
        };
        transition(&mut ticket, Planning, None, "planning time").unwrap();
        transition(&mut ticket, Building, None, "building time").unwrap();
        assert_eq!(ticket.created_at, "creation");
        assert_eq!(ticket.updated_at, "building time");
        assert_eq!(
            ticket.history,
            vec![
                crate::store::Transition {
                    from: Queued,
                    to: Planning,
                    reason: None,
                    at: "planning time".into()
                },
                crate::store::Transition {
                    from: Planning,
                    to: Building,
                    reason: None,
                    at: "building time".into()
                },
            ]
        );
    }

    #[test]
    fn qa_bounce_and_counter_overflow_block() {
        let mut ticket = Ticket {
            state: Qa,
            revision_round: 2,
            ..Ticket::default()
        };
        transition(&mut ticket, Building, None, "bounce").unwrap();
        assert_eq!(ticket.state, Building);
        assert_eq!(ticket.revision_round, 3);
        ticket.state = CiWait;
        ticket.revision_round = u32::MAX;
        transition_with_limit(&mut ticket, Building, None, "overflow", u32::MAX).unwrap();
        assert_eq!(ticket.state, Blocked);
        assert_eq!(ticket.blocked_reason, Some(BlockedReason::LoopCap));
    }
}
