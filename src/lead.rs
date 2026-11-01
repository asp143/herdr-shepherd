//! Lead-agent lifecycle and public status reporting.
use crate::{
    config::Config,
    error::ShepherdError,
    herdr::Herdr,
    launch::{self, LaunchRequest},
    roles::{self, Role},
    store::{AgentRecord, AgentStatus, LeadRecord, Store, Ticket},
};
use serde::Serialize;
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

pub struct LeadContext<'a> {
    pub bin: &'a Path,
    pub state_dir: &'a Path,
    pub workspace_id: &'a str,
}

pub fn start(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    ctx: &LeadContext<'_>,
) -> Result<AgentRecord, ShepherdError> {
    if !cfg.enabled {
        return Err(ShepherdError::Usage("shepherd is disabled".into()));
    }
    crate::flock::reconcile(h, store)?;
    if store
        .read()?
        .tickets
        .iter()
        .any(|ticket| crate::ticket::state::is_active(ticket.state))
    {
        return Err(ShepherdError::Usage(
            "tickets are active; use resume instead".into(),
        ));
    }
    launch_lead(h, store, cfg, ctx)
}

fn launch_lead(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    ctx: &LeadContext<'_>,
) -> Result<AgentRecord, ShepherdError> {
    if ctx.workspace_id.is_empty() {
        return Err(ShepherdError::Usage(
            "invoking workspace is required".into(),
        ));
    }
    store.heal_pending_pings()?;
    let data = store.read()?;
    if crate::trusted::main_checkout(&data.project_root).is_ok() {
        crate::trusted::ensure_snapshot(ctx.state_dir, &data.project_root)?;
    }
    let resolved = roles::resolve(cfg)?;
    let role = Role {
        name: "lead".into(),
        description: "Coordinates the flock and speaks to the user".into(),
        runners: vec![resolved.shepherd.runner.clone()],
        ..Default::default()
    };
    let prompt = crate::prompts::lead_prompt(ctx.bin, &data.project_root, &resolved, &data);
    let rec = launch::launch_configured(
        h,
        store,
        LaunchRequest {
            ticket: String::new(),
            role,
            runners: vec![resolved.runners[&resolved.shepherd.runner].clone()],
            worktree: data.project_root,
            workspace_id: ctx.workspace_id.into(),
            tab_label: "shepherd".into(),
            prompt,
            state_dir: ctx.state_dir.into(),
        },
        cfg,
    )?;
    if rec.status == AgentStatus::LaunchFailed {
        return Err(anyhow::anyhow!("lead runner failed to launch").into());
    }
    remember_lead(store, &rec, ctx.workspace_id)?;
    h.focus_agent(&rec.name)?;
    Ok(rec)
}

fn remember_lead(store: &Store, rec: &AgentRecord, workspace: &str) -> Result<(), ShepherdError> {
    store.update(|data| {
        data.lead = Some(LeadRecord {
            agent_name: rec.name.clone(),
            tab_id: rec.tab_id.clone(),
            pane_id: rec.pane_id.clone(),
            started_at: launch::now(),
            workspace_id: Some(workspace.into()),
        });
        Ok(())
    })
}

pub fn resume(
    h: &dyn Herdr,
    store: &Store,
    cfg: &Config,
    ctx: &LeadContext<'_>,
) -> Result<AgentRecord, ShepherdError> {
    if !cfg.enabled {
        return Err(ShepherdError::Usage("shepherd is disabled".into()));
    }
    crate::flock::reconcile(h, store)?;
    let data = store.read()?;
    if let Some(rec) = data.agents.iter().rev().find(|rec| {
        rec.role == "lead"
            && matches!(
                rec.status,
                AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
            )
    }) && h.agent_get(&rec.name)?.is_some()
    {
        remember_lead(store, rec, ctx.workspace_id)?;
        h.focus_agent(&rec.name)?;
        flush_pings(h, store)?;
        return Ok(rec.clone());
    }
    launch_lead(h, store, cfg, ctx)
}

pub fn ping_lead(h: &dyn Herdr, store: &Store, text: &str) {
    crate::flock::ping(h, store, text);
}

/// Deliver one queued notification per sweep without losing concurrent reports.
pub fn flush_pings(h: &dyn Herdr, store: &Store) -> Result<usize, ShepherdError> {
    // Healing never requires a live or idle lead and causes no transport input.
    store.heal_pending_pings()?;
    let data = store.read()?;
    let Some(lead) = data.lead else {
        return Ok(0);
    };
    let Some(_operation) = crate::flock::try_agent_operation(store, &lead.agent_name)? else {
        return Ok(0);
    };
    let data = store.read()?;
    if data.lead.as_ref() != Some(&lead) {
        return Ok(0);
    }
    let Some(rec) = data.agents.iter().find(|a| a.name == lead.agent_name) else {
        return Ok(0);
    };
    let Some(live) = crate::flock::automatic::recipient(h, store, rec, false)? else {
        return Ok(0);
    };
    if !matches!(live.agent_status.as_str(), "idle" | "done") {
        return Ok(0);
    }
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store.root().join("lead-pings.lock"))
        .map_err(anyhow::Error::from)?;
    match guard.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(0),
        Err(error) => return Err(anyhow::Error::from(error).into()),
    }
    store.heal_pending_pings()?;
    let Some(message) = store.read()?.pending_pings.first().cloned() else {
        return Ok(0);
    };
    crate::flock::automatic::dispatch(
        h,
        store,
        rec,
        crate::flock::automatic::Prompt::Followup {
            text: &message.text,
            kind: "notification",
            nudge_at: None,
            queued: true,
        },
    )
    .map(usize::from)
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub protection_warnings: Vec<String>,
    pub project_root: PathBuf,
    pub lead: Option<LeadRecord>,
    pub tickets: Vec<Ticket>,
    pub agents: Vec<AgentRecord>,
    pub pending_pings: usize,
}

pub fn status(h: &dyn Herdr, store: &Store, cfg: &Config) -> Result<StatusReport, ShepherdError> {
    let data = store.read()?;
    let summary = if data.tickets.is_empty() {
        "No tickets".into()
    } else {
        data.tickets
            .iter()
            .map(|ticket| {
                format!(
                    "{}: {:?}{}",
                    ticket.id,
                    ticket.state,
                    ticket
                        .blocked_reason
                        .map(|reason| format!(" ({reason:?})"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    crate::notify::Notifier::new(h, cfg.upkeep.notifications).send("Shepherd status", &summary);
    Ok(StatusReport {
        protection_warnings: crate::github::protection::status_warnings(&data),
        project_root: data.project_root,
        lead: data.lead,
        tickets: data.tickets,
        agents: data.agents,
        pending_pings: data.pending_pings.len(),
    })
}

pub const DEFAULT_CONFIG: &str = r#"schema_version = 1
enabled = true

[shepherd]
runner = "claude-opus"
max_concurrent_tickets = 2
max_parallel_agents = 6
max_revision_rounds = 3
pr_poll_seconds = 180
integration_branch = "auto"
protected_branches = ["main", "master", "development", "develop", "production", "release-*", "sync/*"]

[ticket_source]
command = []

[roles.planner]
description = "Breaks the ticket into independent units with dependencies, risks, and acceptance checks"
runners = ["codex-sol-high"]
worktree = "ticket"

[roles.researcher]
description = "Investigates the codebase, docs, and ticket before implementation"
runners = ["omp-astra"]
worktree = "ticket"

[roles.coder]
description = "Implements one unit and commits on its branch"
runners = ["codex-sol-high"]
worktree = "child"

[roles.reviewer]
description = "Reviews the full ticket diff; labels findings High/Mid/Low/Wish"
runners = ["omp-astra"]
worktree = "ticket"

[roles.qa]
description = "Runs tests and checks every acceptance criterion against the final code"
runners = ["codex-sol-high"]
worktree = "ticket"

[runners.claude-opus]
harness = "claude"
args = ["--model", "claude-opus-5-5", "--dangerously-skip-permissions"]

[runners.codex-sol-high]
harness = "codex"
args = ["--yolo", "-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high"]

[runners.omp-astra]
harness = "omp"
args = ["--auto-approve", "--model", "openai-codex/gpt-6-astra:medium", "--smol", "openai-codex/gpt-6-luna:max"]

[runners.omp-glm-infra]
harness = "omp"
args = ["--auto-approve", "--model", "ollama/glm-5.3-flash:cloud:medium"]
"#;

#[derive(Debug, Serialize)]
pub struct InitReport {
    pub path: PathBuf,
    pub recommendation: String,
}

pub fn init(project_root: &Path) -> Result<InitReport, ShepherdError> {
    init_with_state(project_root, &crate::trusted::default_state_dir())
}

pub fn init_with_state(project_root: &Path, state_dir: &Path) -> Result<InitReport, ShepherdError> {
    if !project_root.is_dir() {
        return Err(ShepherdError::Usage(
            "project root must be a directory".into(),
        ));
    }
    let path = project_root.join(".shepherd.toml");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ShepherdError::Usage(format!("{} already exists", path.display()))
            } else {
                anyhow::Error::from(error).into()
            }
        })?;
    file.write_all(DEFAULT_CONFIG.as_bytes())
        .map_err(anyhow::Error::from)?;
    if crate::trusted::main_checkout(project_root).is_ok() {
        crate::trusted::snapshot_initialized_config(state_dir, project_root, DEFAULT_CONFIG)?;
    }
    Ok(InitReport {
        path,
        recommendation:
            "For desktop notifications, add [ui.toast] delivery = \"system\" to your herdr config."
                .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_claude_lead_delivers_prompt_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        let store = Store::open(&state, dir.path()).unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        let ctx = LeadContext {
            bin: Path::new("/tmp/bin/herdr-shepherd"),
            state_dir: &state,
            workspace_id: "w1",
        };
        let expected_prompt = crate::prompts::lead_prompt(
            ctx.bin,
            dir.path(),
            &roles::resolve(&cfg).unwrap(),
            &store.read().unwrap(),
        );
        let record = start(&h, &store, &cfg, &ctx).unwrap();
        assert_eq!(record.runner, "claude-opus");
        assert_eq!(record.status, AgentStatus::Working);
        assert!(!record.prompt_pending);
        assert_eq!(record.task, expected_prompt);
        assert!(h.calls.borrow().contains(&format!(
            "start_agent {} claude {} -- {:?}",
            record.name,
            record.pane_id.as_ref().unwrap(),
            [
                "--model",
                "claude-opus-5-5",
                "--dangerously-skip-permissions"
            ]
        )));
        assert!(h.calls.borrow().contains(&format!(
            "prompt_agent {} {expected_prompt} true",
            record.name
        )));

        let resumed = resume(&h, &store, &cfg, &ctx).unwrap();
        assert_eq!(resumed.name, record.name);
        let report = status(&h, &store, &cfg).unwrap();
        assert_eq!(report.lead.unwrap().agent_name, record.name);
        assert_eq!(report.agents.len(), 1);
        assert_eq!(report.agents[0].runner, "claude-opus");
        for operation in ["start_agent", "prompt_agent"] {
            assert_eq!(
                h.calls
                    .borrow()
                    .iter()
                    .filter(|call| call.starts_with(operation))
                    .count(),
                1,
                "resuming must not repeat {operation}"
            );
        }
    }

    #[test]
    fn claude_lead_keeps_prompt_pending_until_first_run_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        let store = Store::open(&state, dir.path()).unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        h.start_results
            .borrow_mut()
            .push_back(crate::herdr::AgentStart::NotReady);
        let ctx = LeadContext {
            bin: Path::new("/tmp/bin/herdr-shepherd"),
            state_dir: &state,
            workspace_id: "w1",
        };
        let record = start(&h, &store, &cfg, &ctx).unwrap();
        assert_eq!(record.runner, "claude-opus");
        assert_eq!(record.status, AgentStatus::Blocked);
        assert!(record.prompt_pending);
        h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
        for (live_status, stored_status) in [
            ("blocked", AgentStatus::Blocked),
            ("initializing", AgentStatus::Starting),
        ] {
            h.agent_records.borrow_mut()[0].agent_status = live_status.into();
            let resumed = resume(&h, &store, &cfg, &ctx).unwrap();
            assert_eq!(resumed.status, stored_status);
            assert!(resumed.prompt_pending);
            assert!(
                !h.calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("prompt_agent"))
            );
        }

        h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        let resumed = resume(&h, &store, &cfg, &ctx).unwrap();
        assert_eq!(resumed.status, AgentStatus::Working);
        assert!(!resumed.prompt_pending);
        resume(&h, &store, &cfg, &ctx).unwrap();
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("prompt_agent"))
                .collect::<Vec<_>>(),
            [&format!(
                "prompt_agent {} {} true",
                record.name, record.task
            )]
        );
    }

    #[test]
    fn legacy_noise_heals_without_a_lead_and_preserves_requests() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        store
            .update(|data| {
                for index in 0..32 {
                    data.tickets.push(crate::store::Ticket {
                        id: format!("T-{index}"),
                        state: crate::store::TicketState::Closed,
                        ..Default::default()
                    });
                }
                Ok(())
            })
            .unwrap();
        // This models a legacy file on disk, independently of the new serializer.
        let path = store.root().join("state.json");
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        legacy["schema_version"] = 1.into();
        legacy["pending_pings"] = serde_json::json!(
            (0..369)
                .map(|index| format!(": sh-69c8fe-lead blocked: screen {index}"))
                .chain((0..32).map(|index| format!(
                    "T-{index}: coder sh-69c8fe-coder-1 reported: old report"
                )))
                .chain(std::iter::once("legacy text from the user".into()))
                .collect::<Vec<_>>()
        );
        legacy["pending_pings"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "text": "structured request from the user",
                "kind": "request",
                "ticket": "T-0",
                "agent": "sh-69c8fe-lead",
                "created_at": 0
            }));
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(store.read().unwrap().pending_pings.len(), 403);
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        let report = crate::supervise::sweep_projects(&h, &dir.path().join("state"), &cfg, 100);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(store.read().unwrap().pending_pings.len(), 2);
        assert_eq!(flush_pings(&h, &store).unwrap(), 0);
        let healed = store.read().unwrap();
        assert_eq!(
            healed.pending_pings,
            [
                "legacy text from the user",
                "structured request from the user"
            ]
        );
        assert!(h.calls.borrow().is_empty());
        assert_eq!(healed.schema_version, 2);
        let reloaded = Store::open(&dir.path().join("state"), dir.path())
            .unwrap()
            .read()
            .unwrap();
        assert_eq!(reloaded, healed);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            saved["pending_pings"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry.is_object())
        );
    }

    fn enqueue_live_notice(store: &Store) {
        store
            .update(|data| {
                data.tickets.push(crate::store::Ticket {
                    id: "T-live".into(),
                    state: crate::store::TicketState::Building,
                    ..Default::default()
                });
                data.enqueue_lead_notice(
                    "T-live: coder reported useful progress",
                    Some("T-live"),
                    Some("worker"),
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn lead_relaunch_retains_live_ticket_notices_and_delivers_them() {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = notification_fixture(dir.path());
        store
            .update(|data| {
                data.agents[0].status = AgentStatus::Retired;
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut().clear();
        enqueue_live_notice(&store);
        let before = store.read().unwrap().pending_pings;
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        resume(
            &h,
            &store,
            &cfg,
            &LeadContext {
                bin: Path::new("fixture"),
                state_dir: &dir.path().join("state"),
                workspace_id: "w1",
            },
        )
        .unwrap();
        assert_eq!(store.read().unwrap().pending_pings, before);
        assert_eq!(flush_pings(&h, &store).unwrap(), 1);
        assert!(store.read().unwrap().pending_pings.is_empty());
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call.contains("T-live: coder reported useful progress false"))
        );
    }

    #[test]
    fn notification_closing_during_recipient_check_is_dropped_before_input() {
        for close_on_read in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let (store, h) = notification_fixture(dir.path());
            enqueue_live_notice(&store);
            let callback_store = store.clone();
            let reads = std::sync::atomic::AtomicUsize::new(0);
            *h.on_get.borrow_mut() = Some(Box::new(move |_| {
                if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == close_on_read {
                    callback_store
                        .update(|data| {
                            data.tickets[0].state = crate::store::TicketState::Closed;
                            Ok(())
                        })
                        .unwrap();
                }
            }));
            assert_eq!(flush_pings(&h, &store).unwrap(), 0);
            assert!(store.read().unwrap().pending_pings.is_empty());
            assert!(h.accepted_prompts.borrow().is_empty());
        }
    }

    #[test]
    fn notification_rejection_preserves_request_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = notification_fixture(dir.path());
        let self_notice_text = ": sh-69c8fe-lead blocked: explicitly quoted by user";
        store
            .update(|data| data.enqueue_lead_request(self_notice_text))
            .unwrap();
        let before = store.read().unwrap().pending_pings;
        h.prompt_results.borrow_mut().push_back((
            false,
            Err(ShepherdError::HerdrCommand {
                code: "agent_blocked".into(),
                message: "before input".into(),
            }),
        ));
        assert!(flush_pings(&h, &store).is_err());
        assert_eq!(store.read().unwrap().pending_pings, before);
        assert_eq!(flush_pings(&h, &store).unwrap(), 1);
        assert_eq!(h.accepted_prompts.borrow().len(), 1);
    }

    #[test]
    fn healing_and_concurrent_enqueue_preserve_every_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        store
            .update(|data| {
                for index in 0..369 {
                    data.pending_pings
                        .push(format!(": sh-69c8fe-lead blocked: screen {index}").into());
                }
                Ok(())
            })
            .unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let healer = scope.spawn(|| {
                barrier.wait();
                for _ in 0..40 {
                    store.heal_pending_pings().unwrap();
                }
            });
            let writer = scope.spawn(|| {
                barrier.wait();
                for index in 0..40 {
                    store
                        .update(|data| {
                            // An explicit concurrent writer may persist while healing
                            // is waiting for the same state lock.
                            data.pending_pings.push(crate::store::PendingPing {
                                text: format!("request {index}"),
                                kind: crate::store::PingKind::Request,
                                ticket: None,
                                agent: None,
                                created_at: 0,
                            });
                            Ok(())
                        })
                        .unwrap();
                }
            });
            healer.join().unwrap();
            writer.join().unwrap();
        });
        store.heal_pending_pings().unwrap();
        let data = store.read().unwrap();
        assert_eq!(data.pending_pings.len(), 40);
        for (index, ping) in data.pending_pings.iter().enumerate() {
            assert_eq!(ping.text, format!("request {index}"));
        }
    }

    #[test]
    fn ping_queued_when_lead_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = notification_fixture(dir.path());
        h.agent_records.borrow_mut()[0].agent_status = "blocked".into();
        ping_lead(&h, &store, &format!("T-1:\n{}", "report ".repeat(40)));
        let pending = store.read().unwrap().pending_pings;
        assert_eq!(pending.len(), 1);
        assert!(pending[0].chars().count() <= 160);
        assert!(!pending[0].contains('\n'));
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("prompt_agent"))
        );
        assert_eq!(flush_pings(&h, &store).unwrap(), 0);
        h.agent_records.borrow_mut()[0].agent_status = "idle".into();
        assert_eq!(flush_pings(&h, &store).unwrap(), 1);
        assert!(store.read().unwrap().pending_pings.is_empty());
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("prompt_agent"))
        );
    }
    fn notification_fixture(root: &Path) -> (Store, crate::herdr::fake::FakeHerdr) {
        let store = Store::open(&root.join("state"), root).unwrap();
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
                    status: AgentStatus::Working,
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
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        h.agent_records.borrow_mut().push(crate::herdr::AgentInfo {
            name: Some("lead".into()),
            agent: "omp".into(),
            agent_status: "idle".into(),
            cwd: Some(root.into()),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        });
        (store, h)
    }

    #[test]
    fn automatic_notifications_preserve_ambiguous_payload_without_replay() {
        for direct_ping in [true, false] {
            for code in ["timeout", "agent_prompt_stalled"] {
                let dir = tempfile::tempdir().unwrap();
                let (store, h) = notification_fixture(dir.path());
                store
                    .update(|data| {
                        data.pending_pings.push("first".into());
                        Ok(())
                    })
                    .unwrap();
                let callback_store = store.clone();
                *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
                    let data = callback_store.read().unwrap();
                    assert!(data.agents[0].prompt_delivery_uncertain());
                    assert!(!data.pending_pings.iter().any(|ping| ping.text == "first"));
                    callback_store
                        .update(|data| {
                            data.pending_pings.push("concurrent".into());
                            Ok(())
                        })
                        .unwrap();
                }));
                h.prompt_results.borrow_mut().push_back((
                    true,
                    Err(ShepherdError::HerdrCommand {
                        code: code.into(),
                        message: "accepted without acknowledgement".into(),
                    }),
                ));
                if direct_ping {
                    ping_lead(&h, &store, "second");
                } else {
                    assert!(
                        flush_pings(&h, &store)
                            .unwrap_err()
                            .prompt_submission_uncertain()
                    );
                }
                *h.on_prompt.borrow_mut() = None;
                for _ in 0..2 {
                    assert_eq!(flush_pings(&h, &store).unwrap(), 0);
                }
                let data = store.read().unwrap();
                assert!(data.agents[0].prompt_delivery_uncertain());
                let observation: serde_json::Value =
                    serde_json::from_str(data.agents[0].observed.as_deref().unwrap()).unwrap();
                assert_eq!(observation["prompt_claim"]["text"], "first");
                assert_eq!(
                    data.pending_pings,
                    if direct_ping {
                        vec!["second", "concurrent"]
                    } else {
                        vec!["concurrent"]
                    }
                );
                assert_eq!(h.accepted_prompts.borrow().len(), 1);
            }
        }
    }

    #[test]
    fn automatic_notification_requires_latest_recipient_to_be_ready() {
        for status in ["working", "blocked", "unknown"] {
            let dir = tempfile::tempdir().unwrap();
            let (store, h) = notification_fixture(dir.path());
            let idle = h.agent_records.borrow()[0].clone();
            let mut busy = idle.clone();
            busy.agent_status = status.into();
            h.agent_get_results
                .borrow_mut()
                .extend([Some(idle), Some(busy)]);
            ping_lead(&h, &store, "first");
            assert!(h.accepted_prompts.borrow().is_empty(), "{status}");
            assert_eq!(store.read().unwrap().pending_pings, ["first"]);
        }
    }

    #[test]
    fn automatic_notification_rechecks_lead_ownership_after_transport() {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = notification_fixture(dir.path());
        let callback_store = store.clone();
        *h.on_get.borrow_mut() = Some(Box::new(move |_| {
            callback_store
                .update(|data| {
                    data.lead.as_mut().unwrap().workspace_id = Some("other".into());
                    Ok(())
                })
                .unwrap();
        }));
        ping_lead(&h, &store, "first");
        assert!(h.accepted_prompts.borrow().is_empty());
        assert_eq!(store.read().unwrap().pending_pings, ["first"]);
    }

    #[test]
    fn automatic_notification_crash_keeps_claim_and_concurrent_enqueue() {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = notification_fixture(dir.path());
        store
            .update(|data| {
                data.pending_pings.push("first".into());
                Ok(())
            })
            .unwrap();
        let callback_store = store.clone();
        *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
            callback_store
                .update(|data| {
                    data.pending_pings.push("concurrent".into());
                    Ok(())
                })
                .unwrap();
            panic!("fixture interruption during input");
        }));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| flush_pings(&h, &store)))
                .is_err()
        );
        *h.on_prompt.borrow_mut() = None;
        for _ in 0..2 {
            assert_eq!(flush_pings(&h, &store).unwrap(), 0);
        }
        let data = store.read().unwrap();
        assert_eq!(data.agents[0].prompt_claim().unwrap().text, "first");
        assert!(data.agents[0].prompt_delivery_uncertain());
        assert_eq!(data.pending_pings, ["concurrent"]);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|call| call.starts_with("prompt_agent "))
                .count(),
            1
        );
    }

    #[test]
    fn automatic_simultaneous_flushes_share_one_durable_claim() {
        for uncertain in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let (store, h) = notification_fixture(dir.path());
            store
                .update(|data| {
                    data.pending_pings.push("first".into());
                    Ok(())
                })
                .unwrap();
            let info = h.agent_records.borrow()[0].clone();
            let barrier = std::sync::Barrier::new(2);
            let calls = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..2)
                    .map(|_| {
                        let info = info.clone();
                        let store = &store;
                        let barrier = &barrier;
                        scope.spawn(move || {
                            let h = crate::herdr::fake::FakeHerdr::new(vec![]);
                            h.agent_records.borrow_mut().push(info);
                            if uncertain {
                                h.prompt_results.borrow_mut().push_back((
                                    true,
                                    Err(ShepherdError::HerdrCommand {
                                        code: "timeout".into(),
                                        message: "accepted without acknowledgement".into(),
                                    }),
                                ));
                            }
                            barrier.wait();
                            let result = flush_pings(&h, store);
                            if let Err(error) = result {
                                assert!(error.prompt_submission_uncertain());
                            }
                            h.accepted_prompts.borrow().len()
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| worker.join().unwrap())
                    .sum::<usize>()
            });
            assert_eq!(calls, 1);
            let data = store.read().unwrap();
            assert_eq!(data.agents[0].prompt_delivery_uncertain(), uncertain);
            assert!(data.pending_pings.is_empty());
        }
    }

    #[test]
    fn automatic_notification_rejection_and_success_preserve_queue_order() {
        for code in ["agent_blocked", "agent_not_ready"] {
            let dir = tempfile::tempdir().unwrap();
            let (store, h) = notification_fixture(dir.path());
            store
                .update(|data| {
                    data.pending_pings.extend(["first".into(), "second".into()]);
                    Ok(())
                })
                .unwrap();
            let callback_store = store.clone();
            *h.on_prompt.borrow_mut() = Some(Box::new(move |_| {
                callback_store
                    .update(|data| {
                        data.pending_pings.push("concurrent".into());
                        Ok(())
                    })
                    .unwrap();
            }));
            h.prompt_results.borrow_mut().push_back((
                false,
                Err(ShepherdError::HerdrCommand {
                    code: code.into(),
                    message: "before input".into(),
                }),
            ));
            assert!(
                !flush_pings(&h, &store)
                    .unwrap_err()
                    .prompt_submission_uncertain()
            );
            *h.on_prompt.borrow_mut() = None;
            let data = store.read().unwrap();
            assert_eq!(data.pending_pings, ["first", "second", "concurrent"]);
            assert!(!data.agents[0].prompt_delivery_uncertain());
            for expected in ["first", "second", "concurrent"] {
                assert_eq!(store.read().unwrap().pending_pings[0], expected);
                assert_eq!(flush_pings(&h, &store).unwrap(), 1);
            }
            assert_eq!(flush_pings(&h, &store).unwrap(), 0);
            assert_eq!(h.accepted_prompts.borrow().len(), 3);
        }
    }

    #[test]
    fn automatic_notifications_skip_busy_and_unowned_recipients() {
        for mismatch in [
            "busy",
            "pane",
            "session",
            "worktree",
            "workspace",
            "retired",
            "reported",
            "initial-pending",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (store, h) = notification_fixture(dir.path());
            let _operation = (mismatch == "busy").then(|| {
                crate::flock::try_agent_operation(&store, "lead")
                    .unwrap()
                    .unwrap()
            });
            match mismatch {
                "pane" => h.agent_records.borrow_mut()[0].pane_id = "other".into(),
                "worktree" => h.agent_records.borrow_mut()[0].cwd = None,
                "workspace" => h.agent_records.borrow_mut()[0].workspace_id = Some("other".into()),
                "session" => {
                    store
                        .update(|data| {
                            data.agents[0].observed = Some(r#"{"session_ref":"saved"}"#.into());
                            Ok(())
                        })
                        .unwrap();
                }
                "initial-pending" => {
                    store
                        .update(|data| {
                            data.agents[0].prompt_pending = true;
                            Ok(())
                        })
                        .unwrap();
                }
                "retired" | "reported" => {
                    store
                        .update(|data| {
                            data.agents[0].status = if mismatch == "retired" {
                                AgentStatus::Retired
                            } else {
                                AgentStatus::Reported
                            };
                            Ok(())
                        })
                        .unwrap();
                }
                _ => {}
            }
            ping_lead(&h, &store, "first");
            for _ in 0..2 {
                assert_eq!(flush_pings(&h, &store).unwrap(), 0);
            }
            assert_eq!(store.read().unwrap().pending_pings, ["first"]);
            assert!(h.accepted_prompts.borrow().is_empty());
        }
    }

    #[test]
    fn automatic_resume_keeps_a_missing_uncertain_lead() {
        let dir = tempfile::tempdir().unwrap();
        let (store, h) = notification_fixture(dir.path());
        store
            .update(|data| {
                data.agents[0].set_prompt_delivery_uncertain(true);
                Ok(())
            })
            .unwrap();
        h.agent_records.borrow_mut().clear();
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        assert!(
            resume(
                &h,
                &store,
                &cfg,
                &LeadContext {
                    bin: Path::new("fixture"),
                    state_dir: &dir.path().join("state"),
                    workspace_id: "w1"
                }
            )
            .is_err()
        );
        assert!(store.read().unwrap().agents[0].prompt_delivery_uncertain());
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("create_tab ")
                    || call.starts_with("start_agent ")
                    || call.starts_with("prompt_agent "))
        );
    }

    #[test]
    fn init_does_not_touch_herdr_config() {
        let Some(home) = std::env::var_os("SHEPHERD_INIT_TEST_HOME") else {
            let dir = tempfile::tempdir().unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .env("HOME", dir.path())
                .env("SHEPHERD_INIT_TEST_HOME", dir.path())
                .args([
                    "--exact",
                    "lead::tests::init_does_not_touch_herdr_config",
                    "--nocapture",
                ])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{} {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        };
        assert_eq!(std::env::var_os("HOME"), Some(home.clone()));
        let home = PathBuf::from(home);
        let project = home.join("project");
        fs::create_dir(&project).unwrap();
        let config = home.join(".config/herdr/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "sentinel = true\n").unwrap();
        let report = init(&project).unwrap();
        assert_eq!(fs::read_to_string(config).unwrap(), "sentinel = true\n");
        assert!(report.recommendation.contains("[ui.toast]"));
        assert!(report.recommendation.contains("delivery = \"system\""));
        let parsed: toml::Table = fs::read_to_string(&report.path).unwrap().parse().unwrap();
        assert_eq!(
            parsed["runners"]["codex-sol-high"]["args"],
            toml::Value::try_from(vec![
                "--yolo",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=high"
            ])
            .unwrap()
        );
        assert!(init(&project).is_err());
        assert!(!project.join(".config").exists());
    }
    #[test]
    fn start_refuses_with_active_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(&dir.path().join("config"), None).unwrap();
        let store = crate::store::Store::open(&dir.path().join("state"), dir.path()).unwrap();
        store
            .update(|data| {
                data.tickets.push(crate::store::Ticket {
                    id: "T-1".into(),
                    state: crate::store::TicketState::Building,
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        let ctx = LeadContext {
            bin: std::path::Path::new("/tmp/bin/herdr-shepherd"),
            state_dir: &dir.path().join("state"),
            workspace_id: "w1",
        };
        assert_eq!(start(&h, &store, &cfg, &ctx).unwrap_err().code(), "usage");
        assert!(!h.calls.borrow().iter().any(|c| c.starts_with("create_tab")));
    }
}
