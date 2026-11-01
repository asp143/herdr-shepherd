use herdr_shepherd::{
    error::ShepherdError,
    herdr::{AgentInfo, fake::FakeHerdr},
    lead::{flush_pings, ping_lead},
    store::{AgentRecord, AgentStatus, LeadRecord, Store, StoreData},
};

fn fixture(root: &std::path::Path, status: Option<AgentStatus>) -> (Store, FakeHerdr) {
    let store = Store::open(&root.join("state"), root).unwrap();
    let h = FakeHerdr::new(vec![]);
    if let Some(status) = status {
        store
            .update(|data| {
                data.lead = Some(LeadRecord {
                    agent_name: "lead".into(),
                    pane_id: Some("w1:p1".into()),
                    tab_id: Some("w1:t1".into()),
                    workspace_id: Some("w1".into()),
                    started_at: "0".into(),
                });
                data.agents.push(AgentRecord {
                    name: "lead".into(),
                    ticket: String::new(),
                    role: "lead".into(),
                    runner: "omp-astra".into(),
                    status,
                    pane_id: Some("w1:p1".into()),
                    tab_id: Some("w1:t1".into()),
                    worktree: root.into(),
                    task: "coordinate".into(),
                    report: None,
                    observed: None,
                    last_progress_at: "0".into(),
                    nudges: 0,
                    restarts: 0,
                    prompt_pending: false,
                });
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut().push(AgentInfo {
            name: Some("lead".into()),
            agent: "omp".into(),
            agent_status: "idle".into(),
            cwd: Some(root.into()),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        });
    }
    (store, h)
}

#[test]
fn repeated_notifications_deduplicate_without_a_running_lead() {
    for status in [None, Some(AgentStatus::Retired)] {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = fixture(dir.path(), status);
        for _ in 0..440 {
            ping_lead(&h, &store, "T-1: waiting for review");
        }
        assert_eq!(
            store.read().unwrap().pending_pings,
            ["T-1: waiting for review"]
        );
        assert!(h.accepted_prompts.borrow().is_empty());
    }
}

#[test]
fn distinct_notifications_stop_at_capacity_without_a_running_lead() {
    for status in [None, Some(AgentStatus::Retired)] {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = fixture(dir.path(), status);
        for index in 0..440 {
            ping_lead(&h, &store, &format!("T-{index}: waiting for review"));
        }
        let pending = store.read().unwrap().pending_pings;
        assert_eq!(pending.len(), StoreData::MAX_PENDING_PINGS);
        assert_eq!(pending[0], "T-0: waiting for review");
        assert_eq!(pending[255], "T-255: waiting for review");
    }
}

#[test]
fn oversized_legacy_queue_is_preserved_and_cannot_grow() {
    let dir = tempfile::tempdir().unwrap();
    let (store, h) = fixture(dir.path(), Some(AgentStatus::Retired));
    let legacy: Vec<_> = (0..440)
        .map(|index| format!("legacy request {index}"))
        .collect();
    store
        .update(|data| {
            data.pending_pings = legacy.clone();
            Ok(())
        })
        .unwrap();
    ping_lead(&h, &store, "another notification");
    assert_eq!(store.read().unwrap().pending_pings, legacy);
}

#[test]
fn explicit_requests_preserve_duplicates_and_reject_a_full_queue_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = fixture(dir.path(), None);
    store
        .update(|data| {
            data.enqueue_lead_request("repeat this request")?;
            data.enqueue_lead_request("repeat this request")?;
            for index in 2..StoreData::MAX_PENDING_PINGS {
                data.enqueue_lead_request(&format!("request {index}"))?;
            }
            Ok(())
        })
        .unwrap();
    let before = store.read().unwrap();
    assert_eq!(
        before.pending_pings[..2],
        ["repeat this request", "repeat this request"]
    );
    assert!(
        store
            .update(|data| data.enqueue_lead_request("new explicit request"))
            .unwrap_err()
            .to_string()
            .contains("queue is full")
    );
    assert_eq!(store.read().unwrap(), before);
}

fn fill_requests(store: &Store) {
    store
        .update(|data| {
            for index in 0..StoreData::MAX_PENDING_PINGS {
                data.enqueue_lead_request(&format!("request {index}"))?;
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn in_flight_notification_reserves_capacity_and_rejection_restores_the_head() {
    for code in ["agent_blocked", "timeout"] {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = fixture(dir.path(), Some(AgentStatus::Working));
        fill_requests(&store);
        let original = store.read().unwrap().pending_pings;
        let callback_store = store.clone();
        *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
            let before = callback_store.read().unwrap();
            assert_eq!(before.pending_pings.len(), StoreData::MAX_PENDING_PINGS - 1);
            assert_eq!(before.agents[0].prompt_claim().unwrap().text, "request 0");
            callback_store
                .update(|data| {
                    assert!(data.enqueue_lead_request("concurrent request").is_err());
                    data.enqueue_lead_ping("concurrent notice");
                    data.enqueue_lead_ping("request 0");
                    Ok(())
                })
                .unwrap();
            assert_eq!(callback_store.read().unwrap(), before);
        }));
        h.prompt_results.borrow_mut().push_back((
            false,
            Err(ShepherdError::HerdrCommand {
                code: code.into(),
                message: "transport rejection".into(),
            }),
        ));
        assert!(flush_pings(&h, &store).is_err());
        let data = store.read().unwrap();
        if code == "agent_blocked" {
            assert_eq!(data.pending_pings, original);
            assert!(data.agents[0].prompt_claim().is_none());
        } else {
            assert_eq!(data.pending_pings, original[1..]);
            assert!(data.agents[0].prompt_delivery_uncertain());
        }
    }
}

#[test]
fn concurrent_report_restoration_preserves_requests_with_one_bounded_overflow() {
    let dir = tempfile::tempdir().unwrap();
    let (store, h) = fixture(dir.path(), Some(AgentStatus::Working));
    fill_requests(&store);
    let original = store.read().unwrap().pending_pings;
    let callback_store = store.clone();
    *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
        herdr_shepherd::flock::store_report(
            &callback_store,
            "lead",
            r#"{"status":"completed","summary":"Reported during input","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#,
        )
        .unwrap();
        callback_store
            .update(|data| {
                let has_room = data.pending_pings.len() < StoreData::MAX_PENDING_PINGS;
                assert_eq!(
                    data.enqueue_lead_request("concurrent request").is_ok(),
                    has_room
                );
                Ok(())
            })
            .unwrap();
    }));
    h.prompt_results.borrow_mut().push_back((
        false,
        Err(ShepherdError::HerdrCommand {
            code: "agent_blocked".into(),
            message: "transport rejection".into(),
        }),
    ));
    assert!(flush_pings(&h, &store).is_err());
    let before = store.read().unwrap();
    assert_eq!(before.pending_pings.len(), StoreData::MAX_PENDING_PINGS + 1);
    assert_eq!(before.pending_pings[..original.len()], original);
    assert_eq!(before.pending_pings.last().unwrap(), "concurrent request");
    assert_eq!(before.agents[0].status, AgentStatus::Reported);
    // A resumed recipient can retry the original head. Once the preserved
    // overflow exists, releasing a claim cannot admit another request.
    for _ in 0..3 {
        store
            .update(|data| {
                data.agents[0].status = AgentStatus::Working;
                data.agents[0].report = None;
                Ok(())
            })
            .unwrap();
        h.prompt_results.borrow_mut().push_back((
            false,
            Err(ShepherdError::HerdrCommand {
                code: "agent_blocked".into(),
                message: "transport rejection".into(),
            }),
        ));
        assert!(flush_pings(&h, &store).is_err());
        assert_eq!(store.read().unwrap().pending_pings, before.pending_pings);
    }
    for index in 0..440 {
        ping_lead(&h, &store, &format!("another notice {index}"));
    }
    assert_eq!(store.read().unwrap(), before);
}

#[test]
fn reconcile_failure_notifications_respect_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let (store, h) = fixture(dir.path(), Some(AgentStatus::Starting));
    h.agent_records.borrow_mut().clear();
    fill_requests(&store);
    let original = store.read().unwrap().pending_pings;
    let result = herdr_shepherd::flock::reconcile(&h, &store).unwrap();
    assert_eq!(result.failed, 1);
    assert_eq!(store.read().unwrap().pending_pings, original);
}
