use super::{AgentInfo, Herdr, TaskDelivery, TaskRecipient, fake::FakeHerdr};

fn fixture(kind: &str, status: &str) -> (FakeHerdr, TaskRecipient) {
    let h = FakeHerdr::new(vec![]);
    h.agent_records.borrow_mut().push(AgentInfo {
        name: Some("owned".into()),
        agent: kind.into(),
        agent_status: status.into(),
        cwd: Some("/fixture/checkout".into()),
        pane_id: "w:p1".into(),
        workspace_id: Some("w".into()),
    });
    h.pane_sessions
        .borrow_mut()
        .insert("w:p1".into(), "session-1".into());
    let recipient = TaskRecipient {
        name: "owned".into(),
        harness: kind.into(),
        pane_id: "w:p1".into(),
        session_ref: "session-1".into(),
    };
    (h, recipient)
}

#[test]
fn delivers_next_task_to_each_idle_harness_without_launching() {
    for kind in ["codex", "omp", "claude", "pi"] {
        for status in ["idle", "done"] {
            let (h, recipient) = fixture(kind, status);
            assert_eq!(
                h.deliver_task(&recipient, "next task").unwrap(),
                TaskDelivery::Accepted
            );
            assert_eq!(*h.calls.borrow(), ["prompt_agent owned next task true"]);
            assert_eq!(h.agent_records.borrow().len(), 1);
            assert_eq!(*h.accepted_prompts.borrow(), ["owned"]);
        }
    }
}

#[test]
fn rejects_unverified_or_busy_recipients_before_input() {
    for change in [
        "missing",
        "name",
        "harness",
        "pane",
        "session",
        "missing-session",
        "blank-session",
        "busy",
        "blocked",
        "unknown",
        "unsupported",
        "empty-task",
    ] {
        let (h, mut recipient) = fixture("codex", "idle");
        let mut text = "next task";
        match change {
            "missing" => h.agent_records.borrow_mut().clear(),
            "name" => {
                let mut wrong = h.agent_records.borrow()[0].clone();
                wrong.name = Some("other".into());
                h.agent_get_results.borrow_mut().push_back(Some(wrong));
            }
            "harness" => h.agent_records.borrow_mut()[0].agent = "omp".into(),
            "pane" => h.agent_records.borrow_mut()[0].pane_id = "w:p2".into(),
            "session" => {
                h.pane_sessions
                    .borrow_mut()
                    .insert("w:p1".into(), "new-session".into());
            }
            "missing-session" => h.pane_sessions.borrow_mut().clear(),
            "blank-session" => recipient.session_ref.clear(),
            "busy" => h.agent_records.borrow_mut()[0].agent_status = "working".into(),
            "blocked" => h.agent_records.borrow_mut()[0].agent_status = "blocked".into(),
            "unknown" => h.agent_records.borrow_mut()[0].agent_status.clear(),
            "unsupported" => {
                recipient.harness = "other".into();
                h.agent_records.borrow_mut()[0].agent = "other".into();
            }
            "empty-task" => text = " \n ",
            _ => unreachable!(),
        }
        // A missing recipient is proven absent; all other cases are
        // rejected by preflight without changing the process or sending input.
        let outcome = h.deliver_task(&recipient, text).unwrap();
        assert!(
            matches!(
                outcome,
                TaskDelivery::Rejected { .. } | TaskDelivery::Dead { .. }
            ),
            "{change}: {outcome:?}"
        );
        assert!(h.calls.borrow().is_empty(), "{change}");
    }
}

#[test]
fn delivery_separates_acknowledgement_rejection_death_and_timeout() {
    use crate::error::ShepherdError;
    for code in [
        "agent_blocked",
        "agent_not_ready",
        "empty_agent_prompt",
        "agent_not_found",
        "not_found",
        "agent_target_not_found",
        "timeout",
        "agent_prompt_stalled",
        "agent_prompt_failed",
        "permission_denied",
    ] {
        let possible_acceptance: &[bool] = match code {
            "agent_blocked"
            | "agent_not_ready"
            | "empty_agent_prompt"
            | "agent_not_found"
            | "not_found"
            | "agent_target_not_found" => &[false],
            _ => &[false, true],
        };
        for &accepted in possible_acceptance {
            let (h, recipient) = fixture("omp", "done");
            h.prompt_results.borrow_mut().push_back((
                accepted,
                Err(ShepherdError::HerdrCommand {
                    code: code.into(),
                    message: "private prompt text".into(),
                }),
            ));
            let outcome = h.deliver_task(&recipient, "next task").unwrap();
            match code {
                "agent_blocked" | "agent_not_ready" | "empty_agent_prompt" => {
                    assert!(matches!(outcome, TaskDelivery::Rejected { .. }))
                }
                "agent_not_found" | "not_found" | "agent_target_not_found" => {
                    assert!(matches!(outcome, TaskDelivery::Dead { .. }))
                }
                _ => assert!(matches!(outcome, TaskDelivery::Ambiguous { .. })),
            }
            assert!(!format!("{outcome:?}").contains("private prompt text"));
            assert_eq!(
                h.calls.borrow().len(),
                1,
                "never retry an ambiguous submission"
            );
            assert_eq!(h.accepted_prompts.borrow().len(), usize::from(accepted));
        }
    }
    let (h, recipient) = fixture("pi", "idle");
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(ShepherdError::Runtime(crate::proc::DeadlineExceeded.into())),
    ));
    assert!(matches!(
        h.deliver_task(&recipient, "next task").unwrap(),
        TaskDelivery::Ambiguous { .. }
    ));
    assert_eq!(h.calls.borrow().len(), 1);
}
