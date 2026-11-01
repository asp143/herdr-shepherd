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
    let data = store.read()?;
    let Some(lead) = data.lead else {
        return Ok(0);
    };
    let Some(live) = h.agent_get(&lead.agent_name)? else {
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
    let Some(message) = store.read()?.pending_pings.first().cloned() else {
        return Ok(0);
    };
    h.prompt_agent(&lead.agent_name, &message, false)?;
    store.update(|data| {
        if data.pending_pings.first() == Some(&message) {
            data.pending_pings.remove(0);
        }
        Ok(())
    })?;
    Ok(1)
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
    fn ping_queued_when_lead_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
        let name = "sh-123abc-lead";
        store
            .update(|data| {
                data.lead = Some(LeadRecord {
                    agent_name: name.into(),
                    tab_id: Some("w1:t1".into()),
                    pane_id: Some("w1:p1".into()),
                    started_at: "1".into(),
                    workspace_id: Some("w1".into()),
                });
                Ok(())
            })
            .unwrap();
        let h = crate::herdr::fake::FakeHerdr::new(vec![]);
        h.agent_records.borrow_mut().push(crate::herdr::AgentInfo {
            name: Some(name.into()),
            agent: "codex".into(),
            agent_status: "blocked".into(),
            cwd: Some(dir.path().into()),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        });
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
