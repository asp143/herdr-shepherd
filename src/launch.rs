use crate::{
    config::Config,
    error::ShepherdError,
    herdr::{AgentStart, Herdr},
    roles::{Role, Runner},
    store::{AgentRecord, AgentStatus, Store},
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub struct LaunchRequest {
    pub ticket: String,
    pub role: Role,
    pub runners: Vec<Runner>,
    pub worktree: PathBuf,
    pub workspace_id: String,
    pub tab_label: String,
    pub prompt: String,
    pub state_dir: PathBuf,
}

pub fn launch(
    h: &dyn Herdr,
    store: &Store,
    req: LaunchRequest,
) -> Result<AgentRecord, ShepherdError> {
    let config = Config {
        schema_version: 1,
        enabled: true,
        upkeep: Default::default(),
        source: crate::config::ConfigSource::Default,
        warnings: vec![],
        raw: toml::Table::new(),
    };
    launch_configured(h, store, req, &config)
}

/// Derive a stable ticket/role identity while retaining the unique suffix.
pub fn agent_name(project_root: &Path, ticket: &str, role: &str, number: usize) -> String {
    let mut hash = Sha256::new();
    hash.update(project_root.as_os_str().as_encoded_bytes());
    hash.update(ticket.as_bytes());
    let hash = format!("{:x}", hash.finalize());
    if role == "lead" {
        return format!("sh-{}-lead", &hash[..6]);
    }
    let suffix = format!("-{number}");
    let room = 32usize.saturating_sub(10 + suffix.len());
    let role: String = role
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .take(room)
        .collect();
    format!("sh-{}-{role}{suffix}", &hash[..6])
}

pub fn launch_configured(
    h: &dyn Herdr,
    store: &Store,
    req: LaunchRequest,
    cfg: &Config,
) -> Result<AgentRecord, ShepherdError> {
    let resolved = crate::roles::resolve(cfg)?;
    let first = req
        .runners
        .first()
        .ok_or_else(|| ShepherdError::Usage("launch requires at least one runner".into()))?;
    // Validate adapters before reserving state or creating a pane.
    for runner in &req.runners {
        crate::harness::harness_for(&runner.harness)?;
    }
    let mut record = store.update(|data| {
        if req.role.name != "lead"
            && data
                .agents
                .iter()
                .filter(|record| {
                    record.role != "lead"
                        && matches!(
                            record.status,
                            AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                        )
                })
                .count()
                >= resolved.shepherd.max_parallel_agents
        {
            return Err(ShepherdError::Capacity);
        }
        if req.role.name == "lead"
            && data.agents.iter().any(|record| {
                record.role == "lead"
                    && matches!(
                        record.status,
                        AgentStatus::Starting | AgentStatus::Working | AgentStatus::Blocked
                    )
            })
        {
            return Err(ShepherdError::Usage(
                "lead is already running; resume it".into(),
            ));
        }
        if req.role.name != "lead"
            && let Some(ticket) = data.tickets.iter().find(|t| t.id == req.ticket)
            && !crate::ticket::state::is_active(ticket.state)
        {
            return Err(ShepherdError::Usage("ticket is not active".into()));
        }
        let mut number = data
            .agents
            .iter()
            .filter(|record| record.ticket == req.ticket && record.role == req.role.name)
            .count()
            + 1;
        let mut name = agent_name(&data.project_root, &req.ticket, &req.role.name, number);
        while req.role.name != "lead" && data.agents.iter().any(|record| record.name == name) {
            number += 1;
            name = agent_name(&data.project_root, &req.ticket, &req.role.name, number);
        }
        // A retired lead has the same stable name; retain one current record.
        if req.role.name == "lead" {
            data.agents.retain(|record| record.name != name);
        }
        let record = AgentRecord {
            task: req.prompt.replace("{{agent_name}}", &name),
            name,
            ticket: req.ticket.clone(),
            role: req.role.name.clone(),
            runner: first.name.clone(),
            status: AgentStatus::Starting,
            pane_id: None,
            tab_id: None,
            worktree: req.worktree.clone(),
            report: None,
            observed: None,
            last_progress_at: now(),
            nudges: 0,
            restarts: 0,
            prompt_pending: true,
        };
        data.agents.push(record.clone());
        Ok(record)
    })?;
    let result = launch_reserved(h, store, &req, cfg, &mut record);
    if result.is_err() {
        // A CLI timeout can follow a successful admission. Retain Starting
        // until reconcile proves whether a live agent needs to be adopted.
        save(store, &record)?;
    }
    result?;
    store
        .read()?
        .agents
        .into_iter()
        .find(|stored| stored.name == record.name)
        .ok_or_else(|| ShepherdError::Usage("launch record disappeared".into()))
}

fn save(store: &Store, record: &AgentRecord) -> Result<(), ShepherdError> {
    store.update(|data| {
        let stored = data
            .agents
            .iter_mut()
            .find(|stored| stored.name == record.name)
            .ok_or_else(|| ShepherdError::Usage("launch record disappeared".into()))?;
        // An immediately finishing agent may report while launch waits for
        // its working signal. Keep that report rather than overwriting it.
        if stored.status == AgentStatus::Retired {
            return Ok(());
        }
        let report = stored.report.clone();
        let status = stored.status;
        *stored = record.clone();
        if report.is_some() {
            stored.report = report;
            stored.status = status;
            stored.prompt_pending = false;
        }
        Ok(())
    })
}

fn launch_reserved(
    h: &dyn Herdr,
    store: &Store,
    req: &LaunchRequest,
    cfg: &Config,
    record: &mut AgentRecord,
) -> Result<(), ShepherdError> {
    let binary = std::env::current_exe().map_err(anyhow::Error::from)?;
    crate::gate::shim::install_shim(&req.state_dir, &binary)?;
    // Pane shells inherit their server's environment. Explicitly export only
    // needed non-secret settings; start_agent unsets all credential variables.
    let mut base = HashMap::new();
    for key in [
        "PATH",
        "LANG",
        "LC_ALL",
        "HERDR_SOCKET_PATH",
        "HERDR_BIN_PATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            base.insert(key.to_owned(), value);
        }
    }
    let mut env = crate::gate::env::agent_env(&base, &req.state_dir);
    env.insert(
        "HERDR_PLUGIN_STATE_DIR".into(),
        req.state_dir.to_string_lossy().into_owned(),
    );
    env.insert(
        "HERDR_SHEPHERD_PROJECT_ROOT".into(),
        store.read()?.project_root.to_string_lossy().into_owned(),
    );
    if let Ok(value) = std::env::var("HERDR_PLUGIN_CONFIG_DIR") {
        env.insert("HERDR_PLUGIN_CONFIG_DIR".into(), value);
    }
    for runner in &req.runners {
        let harness = crate::harness::harness_for(&runner.harness)?;
        record.runner = runner.name.clone();
        record.status = AgentStatus::Starting;
        record.prompt_pending = true;
        let (tab, pane) = h.create_tab(&req.workspace_id, &req.tab_label, &req.worktree)?;
        record.tab_id = Some(tab.clone());
        record.pane_id = Some(pane.clone());
        save(store, record)?;
        let start = h.start_agent(
            &record.name,
            harness.kind(),
            &pane,
            &harness.launch_args(runner),
            &env,
        )?;
        if start == AgentStart::NotReady {
            let screen = h.read_visible(&record.name)?;
            if harness.provider_failure(&screen) {
                h.send_keys(&record.name, &["esc".into()])?;
                h.close_tab(&tab)?;
                record.status = AgentStatus::LaunchFailed;
                record.observed = Some("provider_failure".into());
                save(store, record)?;
                continue;
            }
            record.status = AgentStatus::Blocked;
            save(store, record)?;
            report_badges(h, cfg, &pane, runner, &req.role.name)?;
            crate::hooks::emit(
                cfg,
                "agent.spawned",
                &serde_json::json!({"ticket":record.ticket,"agent":record.name,"role":record.role,"runner":record.runner}),
            );
            return Ok(());
        }
        match h.prompt_agent(&record.name, &record.task, true) {
            Ok(()) => {
                record.prompt_pending = false;
                record.status = AgentStatus::Working;
            }
            Err(error) if error.to_string().contains("agent_blocked") => {
                record.status = AgentStatus::Blocked;
            }
            Err(error)
                if error.to_string().contains("agent_prompt_stalled")
                    || error.to_string().contains("timeout") =>
            {
                // Accepted submissions must never be sent a second time solely
                // because the observation deadline expired.
                record.prompt_pending = false;
                record.status = AgentStatus::Working;
                record.observed = Some("prompt_delivery_uncertain".into());
            }
            Err(error) => return Err(error),
        }
        save(store, record)?;
        report_badges(h, cfg, &pane, runner, &req.role.name)?;
        crate::hooks::emit(
            cfg,
            "agent.spawned",
            &serde_json::json!({"ticket": record.ticket, "agent": record.name, "role": record.role, "runner": record.runner}),
        );
        return Ok(());
    }
    record.status = AgentStatus::LaunchFailed;
    save(store, record)
}

pub fn model_badge(runner: &Runner) -> String {
    let model = runner
        .args
        .windows(2)
        .find(|pair| matches!(pair[0].as_str(), "-m" | "--model"))
        .map(|pair| pair[1].as_str());
    match (runner.harness.as_str(), model) {
        ("omp", Some(model)) => {
            let model = model.rsplit('/').next().unwrap_or(model);
            model
                .rsplit_once(':')
                .map(|(model, effort)| format!("{model}/{effort}"))
                .unwrap_or_else(|| model.into())
        }
        (_, Some(model)) => {
            let effort = runner
                .args
                .iter()
                .find_map(|arg| arg.strip_prefix("model_reasoning_effort="));
            effort
                .map(|effort| format!("{model}/{effort}"))
                .unwrap_or_else(|| model.into())
        }
        _ => runner.harness.clone(),
    }
}

fn report_badges(
    h: &dyn Herdr,
    cfg: &Config,
    pane: &str,
    runner: &Runner,
    stage: &str,
) -> Result<(), ShepherdError> {
    if cfg.upkeep.model_badge {
        h.report_pane_tokens(
            pane,
            &[
                (
                    crate::metadata::token(&cfg.upkeep.token_prefix, "model")?,
                    model_badge(runner),
                ),
                (
                    crate::metadata::token(&cfg.upkeep.token_prefix, "stage")?,
                    stage.into(),
                ),
            ],
            &[],
        )?;
    }
    Ok(())
}

pub fn now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ConfigSource, Upkeep},
        herdr::{AgentStart, fake::FakeHerdr},
        store::AgentStatus,
    };

    fn cfg() -> Config {
        Config {
            schema_version: 1,
            enabled: true,
            upkeep: Upkeep::default(),
            source: ConfigSource::Default,
            warnings: vec![],
            raw: toml::Table::new(),
        }
    }
    fn req(root: &std::path::Path, state: &std::path::Path) -> LaunchRequest {
        let resolved = crate::roles::resolve(&cfg()).unwrap();
        LaunchRequest {
            ticket: "T-1".into(),
            role: resolved
                .roles
                .into_iter()
                .find(|role| role.name == "coder")
                .unwrap(),
            runners: vec![
                resolved.runners["codex-sol-high"].clone(),
                resolved.runners["omp-astra"].clone(),
            ],
            worktree: root.into(),
            workspace_id: "w1".into(),
            tab_label: "coder".into(),
            prompt: "Implement tiny ticket".into(),
            state_dir: state.into(),
        }
    }

    #[test]
    fn all_runners_fail_launch_failed() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        h.start_results
            .borrow_mut()
            .extend([AgentStart::NotReady, AgentStart::NotReady]);
        h.visible_screens
            .borrow_mut()
            .extend(["quota exhausted".into(), "unauthorized".into()]);
        let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
        assert_eq!(record.status, AgentStatus::LaunchFailed);
        assert_eq!(
            store.read().unwrap().agents[0].status,
            AgentStatus::LaunchFailed
        );
        assert!(h.agent_records.borrow().is_empty());
    }

    #[test]
    fn model_badge_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let resolved = crate::roles::resolve(&cfg()).unwrap();
        assert_eq!(
            model_badge(&resolved.runners["codex-sol-high"]),
            "gpt-6.1-sol/high"
        );
        assert_eq!(
            model_badge(&resolved.runners["omp-astra"]),
            "gpt-6-astra/medium"
        );
        let h = FakeHerdr::new(vec![]);
        let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
        assert!(!record.prompt_pending);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call.contains("shepherd_model")
                    && call.contains("gpt-6.1-sol/high")
                    && call.contains("shepherd_stage")
                    && call.contains("coder"))
        );
    }

    #[test]
    fn starting_recorded_before_start() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        h.panic_on_start.set(true);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            launch(&h, &store, req(dir.path(), &state))
        }));
        assert!(result.is_err());
        let record = &store.read().unwrap().agents[0];
        assert_eq!(record.status, AgentStatus::Starting);
        assert!(record.pane_id.is_some());
        assert!(record.prompt_pending);
    }

    #[test]
    fn blocked_launch_retains_pending_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        h.start_results.borrow_mut().push_back(AgentStart::NotReady);
        h.visible_screens
            .borrow_mut()
            .push_back("Trust this folder? Trust and continue".into());
        let mut request = req(dir.path(), &state);
        request.prompt = "Report as {{agent_name}}".into();
        let record = launch(&h, &store, request).unwrap();
        assert_eq!(record.status, AgentStatus::Blocked);
        assert!(record.prompt_pending);
        assert_eq!(record.task, format!("Report as {}", record.name));
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("prompt_agent"))
        );
        assert_eq!(
            h.start_envs.borrow()[0]["HERDR_SHEPHERD_PROJECT_ROOT"],
            dir.path().to_string_lossy()
        );
    }

    #[test]
    fn fallback_on_provider_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        h.start_results
            .borrow_mut()
            .extend([AgentStart::NotReady, AgentStart::Ready]);
        h.visible_screens
            .borrow_mut()
            .push_back("rate limit reached".into());
        let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
        assert_eq!(record.runner, "omp-astra");
        assert_eq!(record.status, AgentStatus::Working);
        assert_eq!(store.read().unwrap().agents.len(), 1);
        assert_eq!(store.read().unwrap().agents[0].runner, "omp-astra");
        let calls = h.calls.borrow();
        let close = calls
            .iter()
            .position(|call| call.starts_with("close_tab"))
            .unwrap();
        let starts: Vec<_> = calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.starts_with("start_agent"))
            .collect();
        assert_eq!(starts.len(), 2);
        assert!(starts[0].0 < close && close < starts[1].0);
    }
}
