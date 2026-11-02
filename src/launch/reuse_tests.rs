use super::*;
use crate::{herdr::fake::FakeHerdr, store::Ticket};

const REPORT: &str = r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#;

fn fixture(role: &str) -> (tempfile::TempDir, Store, FakeHerdr, Config, AgentRecord) {
    let base = std::env::current_dir()
        .unwrap()
        .join("target/reuse-fixtures");
    std::fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                state: crate::store::TicketState::Building,
                worktree: Some(dir.path().into()),
                workspace_id: Some("w1".into()),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let h = FakeHerdr::new(vec![]);
    let cfg = config();
    let record = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), role)).unwrap();
    let pane = record.pane_id.clone().unwrap();
    h.pane_sessions
        .borrow_mut()
        .insert(pane, "owned-session".into());
    store
        .update(|data| {
            let rec = &mut data.agents[0];
            let mut observed: serde_json::Value =
                serde_json::from_str(rec.observed.as_deref().unwrap()).unwrap();
            observed["session_ref"] = "owned-session".into();
            rec.observed = Some(observed.to_string());
            Ok(())
        })
        .unwrap();
    let record = crate::flock::store_report_for_task(
        &store,
        &record.name,
        record.task_id.as_deref(),
        REPORT,
    )
    .unwrap();
    (dir, store, h, cfg, record)
}

fn request(root: &Path, role: &str) -> LaunchRequest {
    let resolved = crate::roles::resolve(&config()).unwrap();
    LaunchRequest {
        ticket: "T-1".into(),
        role: crate::roles::Role {
            name: role.into(),
            ..Default::default()
        },
        runners: vec![resolved.runners["codex-sol-high"].clone()],
        worktree: root.into(),
        workspace_id: "w1".into(),
        tab_label: role.into(),
        prompt: "next {{agent_name}} {{task_id}} {{subagent_hint}}".into(),
        state_dir: root.join("state"),
    }
}

fn reason(record: &AgentRecord) -> String {
    serde_json::to_value(record).unwrap()["dispatch_reason"]
        .as_str()
        .unwrap_or_default()
        .into()
}

#[test]
fn reported_roles_reuse_record_tab_and_process() {
    for role in ["coder", "reviewer", "qa"] {
        let (dir, store, h, cfg, old) = fixture(role);
        let starts = h.start_args.borrow().len();
        let tabs = h
            .calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with("create_tab "))
            .count();
        let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), role)).unwrap();
        assert_eq!(next.name, old.name, "{role}");
        assert_eq!(next.pane_id, old.pane_id);
        assert_eq!(next.tab_id, old.tab_id);
        assert_eq!(store.read().unwrap().agents.len(), 1);
        assert_eq!(h.start_args.borrow().len(), starts);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("create_tab "))
                .count(),
            tabs
        );
        assert_ne!(next.task_id, old.task_id);
        assert_ne!(next.attempt_id, old.attempt_id);
        assert_eq!(next.completed_tasks, old.completed_tasks);
        assert_eq!(next.status, AgentStatus::Working);
        assert!(next.report.is_none());
        assert!(!next.prompt_pending && !next.prompt_delivery_uncertain());
        assert!(reason(&next).starts_with("reused:"));
    }
}

#[test]
fn admission_explains_each_ineligible_candidate() {
    for case in [
        "retired",
        "cancelled",
        "legacy",
        "profile",
        "unit",
        "checkout",
        "session",
        "pane",
        "dead",
        "busy",
        "uncertain",
        "claim",
        "cap",
        "unresponsive",
    ] {
        let (dir, store, h, cfg, old) = fixture("reviewer");
        store
            .update(|data| {
                let rec = &mut data.agents[0];
                match case {
                    "retired" | "cancelled" => rec.status = AgentStatus::Retired,
                    "legacy" => rec.task_id = None,
                    "profile" => rec
                        .launch_profile
                        .as_mut()
                        .unwrap()
                        .args
                        .push("changed".into()),
                    "unit" => rec.unit = Some(99),
                    "checkout" => rec.worktree = dir.path().join("other"),
                    "uncertain" => rec.set_prompt_delivery_uncertain(true),
                    "claim" => rec.set_prompt_claim(Some(crate::store::PromptClaim {
                        task_id: rec.task_id.clone(),
                        attempt_id: rec.attempt_id.clone(),
                        kind: "old".into(),
                        text: "old".into(),
                        queued_ping: None,
                    })),
                    "cap" => rec.completed_tasks = vec![rec.completed_tasks[0].clone(); 5],
                    _ => {}
                }
                Ok(())
            })
            .unwrap();
        match case {
            "session" => {
                h.pane_sessions.borrow_mut().clear();
            }
            "pane" => {
                h.agent_records.borrow_mut()[0].pane_id = "replacement".into();
            }
            "dead" => {
                h.agent_records.borrow_mut().clear();
            }
            "busy" => {
                h.agent_records.borrow_mut()[0].agent_status = "working".into();
            }
            "unresponsive" => {
                h.read_errors
                    .borrow_mut()
                    .insert(old.name.clone(), "unavailable".into());
            }
            _ => {}
        }
        let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "reviewer")).unwrap();
        assert_ne!(next.name, old.name, "{case}");
        let expected = match case {
            "claim" => "claim",
            "uncertain" => "uncertain",
            "cap" => "cap reached",
            "session" | "pane" => "ownership differs",
            _ => case,
        };
        assert!(
            reason(&next).contains(expected),
            "{case}: {}",
            reason(&next)
        );
        assert!(
            reason(&next).starts_with("fresh:"),
            "{case}: {}",
            reason(&next)
        );
    }
}

#[test]
fn active_writer_cannot_share_checkout() {
    let (dir, store, h, cfg, old) = fixture("coder");
    store
        .update(|data| {
            data.agents[0].status = AgentStatus::Working;
            data.agents[0].report = None;
            Ok(())
        })
        .unwrap();
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder"));
    assert!(next.is_err(), "second writer was admitted for {}", old.name);
    assert_eq!(h.start_args.borrow().len(), 1);
}

#[test]
fn reused_timeout_preserves_claim_without_replacement() {
    let (dir, store, h, cfg, old) = fixture("coder");
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(crate::error::ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "accepted but lost ack".into(),
        }),
    ));
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_eq!(next.name, old.name);
    assert_eq!(next.status, AgentStatus::Blocked);
    assert!(next.prompt_delivery_uncertain());
    let claim = next.prompt_claim().unwrap();
    assert_eq!(claim.task_id, next.task_id);
    assert_eq!(claim.attempt_id, next.attempt_id);
    assert_eq!(claim.text, next.task);
    assert_eq!(h.start_args.borrow().len(), 1);
    assert!(crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).is_err());
}

fn config() -> Config {
    Config {
        schema_version: 1,
        enabled: true,
        upkeep: Default::default(),
        source: crate::config::ConfigSource::Default,
        warnings: vec![],
        raw: toml::Table::new(),
    }
}

#[test]
fn cap_does_not_hide_a_still_busy_writer() {
    let (dir, store, h, cfg, _) = fixture("coder");
    store
        .update(|data| {
            let rec = &mut data.agents[0];
            rec.completed_tasks = vec![rec.completed_tasks[0].clone(); 5];
            Ok(())
        })
        .unwrap();
    h.agent_records.borrow_mut()[0].agent_status = "working".into();
    assert!(crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).is_err());
    assert_eq!(h.start_args.borrow().len(), 1);
}

#[test]
fn initial_launch_records_available_session_for_reuse() {
    let (dir, store, h, cfg, old) = fixture("reviewer");
    store
        .update(|data| {
            data.agents[0].status = AgentStatus::Retired;
            Ok(())
        })
        .unwrap();
    for number in 1..100 {
        h.pane_sessions
            .borrow_mut()
            .insert(format!("w1:p{number}"), "session-at-start".into());
    }
    let fresh = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "reviewer")).unwrap();
    assert_ne!(fresh.name, old.name);
    let observation: serde_json::Value =
        serde_json::from_str(fresh.observed.as_deref().unwrap()).unwrap();
    assert_eq!(observation["session_ref"], "session-at-start");
    crate::flock::store_report_for_task(&store, &fresh.name, fresh.task_id.as_deref(), REPORT)
        .unwrap();
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "reviewer")).unwrap();
    assert_eq!(next.name, fresh.name);
}

#[test]
fn rejected_or_dead_delivery_transfers_reserved_task() {
    for code in ["agent_not_ready", "agent_not_found"] {
        let (dir, store, h, cfg, old) = fixture("coder");
        let observed = store.clone();
        let task = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = task.clone();
        *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
            let rec = observed
                .read()
                .unwrap()
                .agents
                .into_iter()
                .find(|a| a.name == name)
                .unwrap();
            if seen.lock().unwrap().is_none() {
                *seen.lock().unwrap() = Some((rec.task_id.clone(), rec.attempt_id.clone()));
            }
        }));
        h.prompt_results.borrow_mut().push_back((
            false,
            Err(ShepherdError::HerdrCommand {
                code: code.into(),
                message: "rejected before input".into(),
            }),
        ));
        let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
        assert_ne!(next.name, old.name);
        assert_eq!(h.start_args.borrow().len(), 2);
        let first = task.lock().unwrap().clone().unwrap();
        assert_eq!(next.task_id, first.0);
        assert_ne!(next.attempt_id, first.1);
        assert_eq!(next.status, AgentStatus::Working);
        assert!(reason(&next).contains("delivery failed"));
        let previous = store.read().unwrap().agents[0].clone();
        assert_eq!(previous.completed_tasks, old.completed_tasks);
        assert_eq!(previous.report, old.report);
        assert_eq!(previous.status, AgentStatus::Reported);
        assert!(reason(&previous).starts_with("unavailable:"));
    }
}

#[test]
fn replacement_reservation_rebinds_caller_before_input() {
    use crate::ticket::revise::{AssignmentRef, CiRecovery};

    for code in ["agent_not_ready", "agent_not_found"] {
        let (dir, store, h, cfg, old) = fixture("coder");
        let callback_store = store.clone();
        let submitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = submitted.clone();
        *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
            let data = callback_store.read().unwrap();
            let recipient = data.agents.iter().find(|a| a.name == name).unwrap();
            let recovery = data.tickets[0].ci_recovery.as_ref().unwrap();
            assert!(
                recovery.assignment.matches(recipient),
                "input reached a recipient before its caller reference was durable"
            );
            assert!(recipient.report.is_none());
            seen.lock().unwrap().push(recovery.assignment.clone());
        }));
        h.prompt_results.borrow_mut().push_back((
            false,
            Err(ShepherdError::HerdrCommand {
                code: code.into(),
                message: "rejected before input".into(),
            }),
        ));
        let next = crate::flock::spawn_with_reservation(
            &h,
            &store,
            &cfg,
            request(dir.path(), "coder"),
            &|data, agent| {
                data.tickets[0].ci_recovery = Some(CiRecovery {
                    generation: "pending-ci-generation".into(),
                    assignment: AssignmentRef::from_agent(agent),
                });
                Ok(())
            },
        )
        .unwrap();
        let inputs = submitted.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].agent, old.name);
        assert_eq!(inputs[1].agent, next.name);
        assert_ne!(inputs[0].agent, inputs[1].agent);
        assert_eq!(inputs[0].task_id, inputs[1].task_id);
        assert_ne!(inputs[0].attempt_id, inputs[1].attempt_id);
        let data = store.read().unwrap();
        assert!(
            data.tickets[0]
                .ci_recovery
                .as_ref()
                .unwrap()
                .assignment
                .matches(&next)
        );
        assert_eq!(next.status, AgentStatus::Working);
    }
}

#[test]
fn matching_report_wins_even_when_ack_times_out() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let observed = store.clone();
    *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
        let rec = observed
            .read()
            .unwrap()
            .agents
            .into_iter()
            .find(|a| a.name == name)
            .unwrap();
        assert!(rec.report.is_none());
        assert!(rec.prompt_delivery_uncertain());
        assert_eq!(rec.prompt_claim().unwrap().text, rec.task);
        crate::flock::store_report_for_task(&observed, name, rec.task_id.as_deref(), REPORT)
            .unwrap();
    }));
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "lost ack".into(),
        }),
    ));
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_eq!(next.name, old.name);
    assert_eq!(next.status, AgentStatus::Reported);
    assert!(!next.prompt_delivery_uncertain());
    assert_eq!(next.completed_tasks.len(), 2);
    assert_eq!(h.start_args.borrow().len(), 1);
}

#[test]
fn interruption_keeps_durable_assignment_and_excludes_second_writer() {
    let (dir, store, h, cfg, old) = fixture("coder");
    *h.on_prompt.borrow_mut() = Some(Box::new(|_| panic!("interrupted during delivery")));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder"))
    }));
    assert!(outcome.is_err());
    let rec = store.read().unwrap().agents[0].clone();
    assert_ne!(rec.task_id, old.task_id);
    assert!(rec.prompt_delivery_uncertain());
    assert_eq!(rec.prompt_claim().unwrap().task_id, rec.task_id);
    *h.on_prompt.borrow_mut() = None;
    assert!(crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).is_err());
    assert_eq!(h.start_args.borrow().len(), 1);
}

#[test]
fn concurrent_dispatch_cannot_admit_another_task_during_delivery() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let observed = store.clone();
    let root = dir.path().to_owned();
    *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
        let root = root.clone();
        let observed = observed.clone();
        // A second actual caller can neither reserve this recipient nor start
        // another writer while the first waited submission is in flight.
        std::thread::spawn(move || {
            let other = FakeHerdr::new(vec![]);
            let result = crate::flock::spawn(&other, &observed, &config(), request(&root, "coder"));
            assert!(result.unwrap_err().to_string().contains("checkout"));
            assert!(other.calls.borrow().is_empty());
        })
        .join()
        .unwrap();
    }));
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_eq!(next.name, old.name);
    assert_eq!(store.read().unwrap().agents.len(), 1);
    assert_eq!(h.accepted_prompts.borrow().len(), 2);
}

#[test]
fn stale_reports_and_old_worktree_file_do_not_complete_reassignment() {
    let (dir, store, h, cfg, old) = fixture("reviewer");
    let file = dir.path().join(format!(
        ".shepherd-report-{}-{}.json",
        old.name,
        old.task_id.as_deref().unwrap()
    ));
    std::fs::write(&file, REPORT).unwrap();
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "reviewer")).unwrap();
    assert_eq!(next.name, old.name);
    assert!(
        crate::flock::store_report_for_task(&store, &next.name, old.task_id.as_deref(), REPORT)
            .is_err()
    );
    assert!(crate::flock::store_report(&store, &next.name, REPORT).is_err());
    assert_eq!(store.read().unwrap().agents[0], next);
    assert!(file.exists());
}

#[test]
fn unchanged_configured_fallback_profile_is_reused() {
    let (dir, store, h, cfg, old) = fixture("reviewer");
    let resolved = crate::roles::resolve(&cfg).unwrap();
    let fallback = resolved.runners["omp-astra"].clone();
    store
        .update(|data| {
            data.agents[0].runner = fallback.name.clone();
            data.agents[0].launch_profile = Some((&fallback).into());
            Ok(())
        })
        .unwrap();
    h.agent_records.borrow_mut()[0].agent = fallback.harness.clone();
    let mut req = request(dir.path(), "reviewer");
    req.runners.push(fallback.clone());
    let next = crate::flock::spawn(&h, &store, &cfg, req).unwrap();
    assert_eq!(next.name, old.name);
    assert_eq!(next.launch_profile, Some((&fallback).into()));
    assert_eq!(h.start_args.borrow().len(), 1);
}

#[test]
fn mismatched_runner_identity_starts_fresh() {
    let (dir, store, h, cfg, old) = fixture("reviewer");
    store
        .update(|data| {
            data.agents[0].runner = "different".into();
            Ok(())
        })
        .unwrap();
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "reviewer")).unwrap();
    assert_ne!(next.name, old.name);
    assert!(reason(&next).contains("profile"));
}

#[test]
fn ambiguous_delivery_transfers_only_after_exact_pane_stops() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let name = old.name.clone();
    let tab = old.tab_id.clone().unwrap();
    let reserved = store.clone();
    let task_id = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen = task_id.clone();
    *h.on_prompt_effect.borrow_mut() = Some(Box::new(move |h, target| {
        if target == name {
            *seen.lock().unwrap() = reserved.read().unwrap().agents[0].task_id.clone();
            h.close_tab(&tab).unwrap();
        }
    }));
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "accepted then exited".into(),
        }),
    ));
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_ne!(next.name, old.name);
    assert_eq!(next.task_id, *task_id.lock().unwrap());
    assert!(reason(&next).contains("exact pane stopped"));
    assert_eq!(h.start_args.borrow().len(), 2);
    assert_eq!(h.agent_records.borrow().len(), 1);
}

#[test]
fn missing_name_with_present_pane_does_not_replace_uncertain_writer() {
    let (dir, store, h, cfg, old) = fixture("coder");
    *h.on_prompt_effect.borrow_mut() = Some(Box::new(|h, _| h.agent_records.borrow_mut().clear()));
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "unknown recipient state".into(),
        }),
    ));
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_eq!(next.name, old.name);
    assert!(next.prompt_delivery_uncertain());
    assert_eq!(h.start_args.borrow().len(), 1);
}

#[test]
fn unit_coder_reuses_only_same_live_child_checkout() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let child = dir.path().join("child");
    let other = dir.path().join("other-child");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    store
        .update(|data| {
            data.tickets[0].children = vec![
                crate::store::ChildWorktree {
                    unit: 1,
                    branch: "feat/unit-1".into(),
                    path: child.clone(),
                    workspace_id: None,
                    base_sha: "abc".into(),
                    integrated: false,
                },
                crate::store::ChildWorktree {
                    unit: 2,
                    branch: "feat/unit-2".into(),
                    path: other.clone(),
                    workspace_id: None,
                    base_sha: "abc".into(),
                    integrated: false,
                },
            ];
            data.agents[0].unit = Some(1);
            data.agents[0].worktree = child.clone();
            Ok(())
        })
        .unwrap();
    h.agent_records.borrow_mut()[0].cwd = Some(child.clone());
    let mut req = request(dir.path(), "coder");
    req.worktree = child;
    let next = crate::flock::spawn(&h, &store, &cfg, req).unwrap();
    assert_eq!(next.name, old.name);
    assert_eq!(next.unit, Some(1));
    crate::flock::store_report_for_task(&store, &next.name, next.task_id.as_deref(), REPORT)
        .unwrap();
    let mut req = request(dir.path(), "coder");
    req.worktree = other;
    let fresh = crate::flock::spawn(&h, &store, &cfg, req).unwrap();
    assert_ne!(fresh.name, old.name);
    assert_eq!(fresh.unit, Some(2));
    assert!(reason(&fresh).contains("unit differs"));
}

#[test]
fn canonical_checkout_alias_reuses_the_owned_recipient() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
    let mut req = request(dir.path(), "coder");
    req.worktree = alias;
    let next = crate::flock::spawn(&h, &store, &cfg, req).unwrap();
    assert_eq!(next.name, old.name);
}

#[test]
fn configured_completed_assignment_cap_starts_fresh() {
    let (dir, store, h, mut cfg, old) = fixture("reviewer");
    cfg.raw = "[shepherd]\nmax_completed_assignments=1".parse().unwrap();
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "reviewer")).unwrap();
    assert_ne!(next.name, old.name);
    assert!(reason(&next).contains("cap reached"));
}

#[test]
fn cleanup_receipt_blocks_reuse_and_new_launch() {
    let (dir, store, h, cfg, old) = fixture("coder");
    store
        .update(|data| {
            data.tickets[0]
                .child_cleanup
                .push(crate::store::ChildCleanup {
                    child: crate::store::ChildWorktree {
                        unit: 1,
                        branch: "feat/unit".into(),
                        path: dir.path().into(),
                        workspace_id: None,
                        base_sha: "abc".into(),
                        integrated: false,
                    },
                    path: dir.path().into(),
                    tip: "abc".into(),
                    retiring_agents: vec![old.clone()],
                });
            Ok(())
        })
        .unwrap();
    assert!(
        crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder"))
            .unwrap_err()
            .to_string()
            .contains("cleanup")
    );
    assert_eq!(h.start_args.borrow().len(), 1);
    assert_eq!(store.read().unwrap().agents[0], old);
}

#[test]
fn cancelled_uncertain_assignment_does_not_block_fresh_writer() {
    let (dir, store, h, cfg, old) = fixture("coder");
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "lost ack".into(),
        }),
    ));
    let pending = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert!(pending.prompt_delivery_uncertain());
    crate::flock::cancel(&h, &store, &old.name, true).unwrap();
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_ne!(next.name, old.name);
    assert_eq!(next.status, AgentStatus::Working);
    assert!(reason(&next).contains("cancelled"));
    assert_eq!(h.agent_records.borrow().len(), 1);
}

#[test]
fn recipient_becoming_busy_before_input_cannot_duplicate_writers() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let idle = h.agent_records.borrow()[0].clone();
    h.agent_records.borrow_mut()[0].agent_status = "working".into();
    // Admission sees idle; the waited delivery preflight sees the new busy
    // state and rejects before input. A replacement must stop that writer.
    h.agent_get_results
        .borrow_mut()
        .extend([Some(idle.clone()), Some(idle)]);
    let next = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_ne!(next.name, old.name);
    assert_eq!(
        h.agent_records.borrow().len(),
        1,
        "busy predecessor survived replacement"
    );
    assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Retired);
    assert_eq!(next.status, AgentStatus::Working);
}

#[test]
fn late_reuse_delivery_cannot_change_a_new_attempt() {
    let (dir, store, h, cfg, old) = fixture("coder");
    let callback_store = store.clone();
    *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
        callback_store
            .update(|data| {
                let current = data.agents.iter_mut().find(|a| a.name == name).unwrap();
                current.attempt_id = Some("new-recovery-attempt".into());
                current.dispatch_reason = Some("new attempt is authoritative".into());
                let mut claim = current.prompt_claim().unwrap();
                claim.attempt_id = current.attempt_id.clone();
                current.set_prompt_claim(Some(claim));
                Ok(())
            })
            .unwrap();
    }));
    h.prompt_results.borrow_mut().push_back((
        true,
        Err(ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "late timeout".into(),
        }),
    ));
    let current = crate::flock::spawn(&h, &store, &cfg, request(dir.path(), "coder")).unwrap();
    assert_eq!(current.name, old.name);
    assert_eq!(current.attempt_id.as_deref(), Some("new-recovery-attempt"));
    assert_eq!(
        current.dispatch_reason.as_deref(),
        Some("new attempt is authoritative")
    );
    assert_eq!(store.read().unwrap().agents.len(), 1);
}

#[test]
fn late_launch_save_cannot_restore_an_earlier_assignment() {
    for changed in ["task", "attempt"] {
        let (_dir, store, _h, _cfg, old) = fixture("coder");
        let current = store
            .update(|data| {
                let current = &mut data.agents[0];
                current.report = None;
                current.status = AgentStatus::Working;
                if changed == "task" {
                    current.task_id = Some("next-task".into());
                } else {
                    current.attempt_id = Some("next-attempt".into());
                }
                current.task = "current payload".into();
                Ok(current.clone())
            })
            .unwrap();
        save(&store, &old).unwrap();
        assert_eq!(store.read().unwrap().agents[0], current, "{changed}");
    }
}
