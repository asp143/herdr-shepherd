mod reuse;
#[cfg(test)]
mod reuse_tests;

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

/// Opaque assignment token, generated once before persisting a launch intent.
/// Native resume and provider fallback keep the current token.
pub fn new_task_id(name: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(name.as_bytes());
    hash.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    format!("{:x}", hash.finalize())[..24].to_owned()
}

pub fn launch_configured(
    h: &dyn Herdr,
    store: &Store,
    req: LaunchRequest,
    cfg: &Config,
) -> Result<AgentRecord, ShepherdError> {
    launch_with_reservation(h, store, req, cfg, &|_, _| Ok(()))
}

/// Persist the caller's assignment reference in the same transaction as the
/// launch/reuse intent, before any input can reach the recipient.
pub type ReservationHook<'a> =
    dyn Fn(&mut crate::store::StoreData, &AgentRecord) -> Result<(), ShepherdError> + 'a;

pub fn launch_with_reservation(
    h: &dyn Herdr,
    store: &Store,
    req: LaunchRequest,
    cfg: &Config,
    reserved: &ReservationHook<'_>,
) -> Result<AgentRecord, ShepherdError> {
    let _hooks = crate::hooks::defer();
    let _ticket_operation = crate::ticket::operation::shared(store, &req.ticket)?;
    let first = req
        .runners
        .first()
        .ok_or_else(|| ShepherdError::Usage("launch requires at least one runner".into()))?;
    // Validate adapters before reserving state or creating a pane.
    for runner in &req.runners {
        crate::harness::harness_for(&runner.harness)?;
    }
    // Fail a conflicting writer before waiting on placement. The same check
    // runs again under placement and in the reservation transaction.
    reuse::check_admission(&store.read()?, &req, cfg)?;
    let _placement = reuse::placement(store)?;
    let admission = reuse::admit(h, store, &req, cfg, reserved)?;
    let (reason, transfer) = match admission {
        reuse::Admission::Reused(record) => return Ok(*record),
        reuse::Admission::Fresh { reason, transfer } => (reason, transfer),
    };
    let (mut record, _operation, needs_launch) = store.update(|data| {
        if let Some(transfer) = &transfer {
            let current = data
                .agents
                .iter_mut()
                .find(|a| a.name == transfer.reserved.name)
                .ok_or_else(|| ShepherdError::Usage("reuse recipient disappeared".into()))?;
            if current.task_id != transfer.reserved.task_id
                || current.report.is_some()
                || current.status == AgentStatus::Retired
            {
                return Ok((current.clone(), None, false));
            }
            if current != &transfer.reserved {
                return Err(ShepherdError::Usage(
                    "reuse intent changed; retry dispatch".into(),
                ));
            }
            *current = transfer.original.clone();
            current.dispatch_reason = Some(format!("unavailable: {reason}"));
        }
        reuse::check_admission(data, &req, cfg)?;
        let cwd = req
            .worktree
            .canonicalize()
            .unwrap_or_else(|_| req.worktree.clone());
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
        // Nonblocking acquisition inside the reservation transaction avoids a
        // state/operation lock inversion with recovery.
        let operation = crate::flock::try_agent_operation(store, &name)?
            .ok_or_else(|| ShepherdError::Usage("agent launch is already running".into()))?;
        // A retired lead has the same stable name; retain one current record.
        if req.role.name == "lead" {
            data.agents.retain(|record| record.name != name);
        }
        let task_id = transfer
            .as_ref()
            .and_then(|t| t.reserved.task_id.clone())
            .unwrap_or_else(|| new_task_id(&name));
        let attempt = if transfer.is_some() { 2 } else { 1 };
        let unit = data
            .tickets
            .iter()
            .find(|ticket| ticket.id == req.ticket)
            .and_then(|ticket| {
                ticket.children.iter().find(|child| {
                    child
                        .path
                        .canonicalize()
                        .unwrap_or_else(|_| child.path.clone())
                        == cwd
                })
            })
            .map(|child| child.unit);
        let record = AgentRecord {
            task: req
                .prompt
                .replace("{{agent_name}}", &name)
                .replace("{{task_id}}", &task_id),
            name,
            ticket: req.ticket.clone(),
            role: req.role.name.clone(),
            runner: first.name.clone(),
            status: AgentStatus::Starting,
            pane_id: None,
            tab_id: None,
            worktree: req.worktree.clone(),
            report: None,
            observed: Some(
                serde_json::json!({
                    "tab_workspace_id": req.workspace_id,
                })
                .to_string(),
            ),
            last_progress_at: now(),
            nudges: 0,
            restarts: 0,
            prompt_pending: true,
            attempt_id: Some(format!("{task_id}-{attempt}")),
            task_id: Some(task_id),
            unit,
            launch_profile: Some(first.into()),
            completed_tasks: vec![],
            dispatch_reason: Some(reason.clone()),
        };
        data.agents.push(record.clone());
        reserved(data, &record)?;
        Ok((record, Some(operation), true))
    })?;
    if !needs_launch {
        return Ok(record);
    }
    let result = launch_reserved(h, store, &req, cfg, &mut record);
    if result.is_err() {
        // A CLI timeout can follow a successful admission. Retain Starting
        // until reconcile proves whether a live agent needs to be adopted.
        save(store, &record)?;
    }
    result?;
    drop(_operation);
    drop(transfer);
    drop(_placement);
    drop(_ticket_operation);
    drop(_hooks);
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
        // Reports can land after the launch reload. Preserve their immutable
        // history and resolved claims along with the accepted result.
        if stored.status == AgentStatus::Retired || stored.report.is_some() {
            return Ok(());
        }
        *stored = record.clone();
        Ok(())
    })
}

#[cfg(debug_assertions)]
fn forward_fixture_env(
    source: &HashMap<String, String>,
    state: &Path,
    worktree: &Path,
    env: &mut HashMap<String, String>,
) {
    let scratch = |path: &Path| {
        path.canonicalize()
            .is_ok_and(|path| path.starts_with("/tmp"))
    };
    let Some(fake) = source.get("SHEPHERD_FAKE_GITHUB") else {
        return;
    };
    if !scratch(state) || !scratch(worktree) || !scratch(Path::new(fake)) {
        return;
    }
    env.insert("SHEPHERD_FAKE_GITHUB".into(), fake.clone());
    if source
        .get("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN")
        .map(String::as_str)
        == Some("allow-local-in-tmp")
    {
        env.insert(
            "SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN".into(),
            "allow-local-in-tmp".into(),
        );
    }
}

/// The launch operation owns a newly created tab before task submission.
/// Reports remain independent of that operation and always win over fallback.
fn current_launch(store: &Store, record: &AgentRecord) -> Result<bool, ShepherdError> {
    Ok(store.read()?.agents.iter().any(|current| {
        current == record
            && current.report.is_none()
            && current.status != AgentStatus::Retired
            && !current.prompt_delivery_uncertain()
    }))
}

fn owned_launch(h: &dyn Herdr, store: &Store, record: &AgentRecord) -> Result<bool, ShepherdError> {
    let Some(info) = h.agent_get(&record.name)? else {
        return Ok(false);
    };
    Ok(crate::supervise::owned_terminal(record, &info)
        && crate::supervise::same_session(record, h.pane_session_ref(&info.pane_id)?.as_deref())
        && current_launch(store, record)?)
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
    #[cfg(debug_assertions)]
    {
        let fixture: HashMap<_, _> = ["SHEPHERD_FAKE_GITHUB", "SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.into(), value)))
            .collect();
        forward_fixture_env(&fixture, &req.state_dir, &req.worktree, &mut env);
    }
    for runner in &req.runners {
        if !current_launch(store, record)? {
            return Ok(());
        }
        let harness = crate::harness::harness_for(&runner.harness)?;
        record.task = req
            .prompt
            .replace("{{agent_name}}", &record.name)
            .replace("{{task_id}}", record.task_id.as_deref().unwrap_or_default())
            .replace("{{subagent_hint}}", harness.subagent_hint());
        record.runner = runner.name.clone();
        record.launch_profile = Some(runner.into());
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
            &harness.managed_args(harness.launch_args(runner), &req.state_dir)?,
            &env,
        )?;
        if !current_launch(store, record)? {
            return Ok(());
        }
        if let Some(reference) = h.pane_session_ref(&pane)?.filter(|s| !s.is_empty()) {
            let mut observation: serde_json::Value =
                serde_json::from_str(record.observed.as_deref().unwrap_or("{}"))
                    .unwrap_or_else(|_| serde_json::json!({}));
            observation["session_ref"] = reference.into();
            record.observed = Some(observation.to_string());
            save(store, record)?;
        }
        if start == AgentStart::NotReady {
            let screen = h.read_visible(&record.name)?;
            if harness.provider_failure(&screen) {
                // No task has crossed the prompt boundary, but verify the
                // reserved recipient again before each destructive effect.
                if !owned_launch(h, store, record)? {
                    return Ok(());
                }
                h.send_keys(&record.name, &["esc".into()])?;
                if !owned_launch(h, store, record)? {
                    return Ok(());
                }
                h.close_tab(&tab)?;
                if !current_launch(store, record)? {
                    return Ok(());
                }
                record.status = AgentStatus::LaunchFailed;
                record.observed = Some(
                    serde_json::json!({
                        "tab_workspace_id": req.workspace_id,
                        "condition": "provider_failure",
                    })
                    .to_string(),
                );
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
        let delivery = crate::flock::deliver_pending_prompt(h, store, &record.name);
        // Delivery owns the durable claim, including an ambiguous outcome.
        // Reload it before save so an error cannot restore a stale pending task.
        if let Some(current) = store
            .read()?
            .agents
            .into_iter()
            .find(|a| a.name == record.name)
        {
            *record = current;
        }
        match delivery {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.herdr_code(),
                    Some("agent_blocked" | "agent_not_ready")
                ) || error.prompt_submission_uncertain() => {}
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

    #[cfg(debug_assertions)]
    #[test]
    fn scratch_launch_forwards_fake_provider_only_in_tmp() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let fake = dir.path().join("fake");
        let state = dir.path().join("state");
        std::fs::create_dir(&fake).unwrap();
        std::fs::create_dir(&state).unwrap();
        let source = HashMap::from([
            (
                "SHEPHERD_FAKE_GITHUB".into(),
                fake.to_string_lossy().into_owned(),
            ),
            (
                "SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN".into(),
                "allow-local-in-tmp".into(),
            ),
        ]);
        let mut forwarded = HashMap::new();
        forward_fixture_env(&source, &state, dir.path(), &mut forwarded);
        assert_eq!(forwarded, source);
        forwarded.clear();
        forward_fixture_env(&source, Path::new("/home"), dir.path(), &mut forwarded);
        assert!(forwarded.is_empty());
        forward_fixture_env(&source, &state, Path::new("/home"), &mut forwarded);
        assert!(forwarded.is_empty());
    }

    #[test]
    fn managed_omp_initial_launch_preserves_pinned_args() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        let mut request = req(dir.path(), &state);
        request.runners.remove(0);
        request.runners[0].args.extend([
            "--config".into(),
            "custom.yml".into(),
            "--no-title".into(),
        ]);
        let pinned = request.runners[0].args.clone();
        let record = launch(&h, &store, request).unwrap();
        let args = h.start_args.borrow();
        assert_eq!(&args[0][..pinned.len()], pinned);
        assert_eq!(args[0].len(), pinned.len() + 2);
        assert_eq!(args[0][pinned.len()], "--config");
        assert_eq!(
            std::fs::read_to_string(&args[0][pinned.len() + 1]).unwrap(),
            "paste:\n  largeMenuThreshold: 0\n"
        );
        assert_eq!(record.runner, "omp-astra");
        assert_eq!(record.status, AgentStatus::Working);
        assert_eq!(h.accepted_prompts.borrow().len(), 1);
        assert_eq!(
            h.start_envs.borrow()[0]["HERDR_PLUGIN_STATE_DIR"],
            state.to_string_lossy()
        );
    }

    #[test]
    fn active_launch_is_not_reconciled_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        let observed = store.clone();
        *h.on_start.borrow_mut() = Some(Box::new(move |_| {
            let event = FakeHerdr::new(vec![]);
            let report = crate::flock::reconcile(&event, &observed).unwrap();
            assert_eq!(report.failed, 0, "a live launcher owns its starting intent");
        }));
        let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
        assert_eq!(record.status, AgentStatus::Working);
    }
    #[test]
    fn event_recovery_and_normal_launch_share_one_prompt_claim() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        let event_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = event_count.clone();
        let observed_store = store.clone();
        *h.on_start.borrow_mut() = Some(Box::new(move |name| {
            let rec = observed_store
                .read()
                .unwrap()
                .agents
                .into_iter()
                .find(|a| a.name == name)
                .unwrap();
            let event = FakeHerdr::new(vec![]);
            event
                .agent_records
                .borrow_mut()
                .push(crate::herdr::AgentInfo {
                    name: Some(name.into()),
                    agent: "codex".into(),
                    agent_status: "idle".into(),
                    cwd: Some(rec.worktree),
                    pane_id: rec.pane_id.unwrap(),
                    workspace_id: Some("w1".into()),
                });
            crate::flock::reconcile(&event, &observed_store).unwrap();
            count.fetch_add(
                event
                    .calls
                    .borrow()
                    .iter()
                    .filter(|c| c.starts_with("prompt_agent"))
                    .count(),
                std::sync::atomic::Ordering::SeqCst,
            );
        }));
        launch(&h, &store, req(dir.path(), &state)).unwrap();
        let normal = h
            .calls
            .borrow()
            .iter()
            .filter(|c| c.starts_with("prompt_agent"))
            .count();
        assert_eq!(
            event_count.load(std::sync::atomic::Ordering::SeqCst) + normal,
            1,
            "startup events must not duplicate the initial task"
        );
    }
    // Transport errors do not tell us whether the application accepted a task.
    // Exercise both runners through the same public launch and recovery flow.
    #[test]
    fn ambiguous_prompt_delivery_is_quarantined_without_duplicate_input() {
        for runner in ["omp-astra", "codex-sol-high"] {
            for accepted in [false, true] {
                for code in ["timeout", "agent_prompt_stalled", "local_deadline"] {
                    let dir = tempfile::tempdir().unwrap();
                    let state = dir.path().join("state");
                    let store = Store::open(&state, dir.path()).unwrap();
                    let h = FakeHerdr::new(vec![]);
                    let error = if code == "local_deadline" {
                        ShepherdError::Runtime(crate::proc::DeadlineExceeded.into())
                    } else {
                        ShepherdError::HerdrCommand {
                            code: code.into(),
                            message: "observation expired".into(),
                        }
                    };
                    h.prompt_results
                        .borrow_mut()
                        .push_back((accepted, Err(error)));
                    let mut request = req(dir.path(), &state);
                    request.runners =
                        vec![crate::roles::resolve(&cfg()).unwrap().runners[runner].clone()];
                    let record = launch(&h, &store, request).unwrap();
                    assert_eq!(record.status, AgentStatus::Blocked, "{runner}: {code}");
                    assert!(
                        !record.prompt_pending,
                        "ambiguous delivery cannot be replayed"
                    );
                    assert_eq!(record.runner, runner);
                    assert_eq!(record.worktree, dir.path());
                    assert!(
                        record
                            .observed
                            .as_deref()
                            .unwrap()
                            .contains("prompt_delivery_uncertain")
                    );
                    store
                        .update(|data| {
                            data.tickets.push(crate::store::Ticket {
                                id: record.ticket.clone(),
                                state: crate::store::TicketState::Building,
                                worktree: Some(record.worktree.clone()),
                                workspace_id: Some("w1".into()),
                                ..Default::default()
                            });
                            Ok(())
                        })
                        .unwrap();
                    h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
                    for now in [100, 1000, 2000] {
                        crate::flock::reconcile(&h, &store).unwrap();
                        let sweep =
                            crate::supervise::supervise_project(&h, &store, &cfg(), &state, now)
                                .unwrap();
                        assert!(sweep.errors.is_empty(), "{:?}", sweep.errors);
                        let current = store.read().unwrap().agents.remove(0);
                        assert_eq!(current.status, AgentStatus::Blocked);
                        assert!(!current.prompt_pending);
                    }
                    assert_eq!(h.accepted_prompts.borrow().len(), usize::from(accepted));
                    let calls = h.calls.borrow();
                    assert_eq!(
                        calls
                            .iter()
                            .filter(|c| c.starts_with("prompt_agent"))
                            .count(),
                        1
                    );
                    assert!(!calls.iter().any(|c| c.starts_with("send_keys")
                        || c.starts_with("close_tab")
                        || c.starts_with("resume_agent")));
                    drop(calls);
                    // Later independent working evidence can release quarantine,
                    // including a user who manually submitted already staged text.
                    h.agent_records.borrow_mut()[0].agent_status = "working".into();
                    let sweep =
                        crate::supervise::supervise_project(&h, &store, &cfg(), &state, 2001)
                            .unwrap();
                    assert!(sweep.errors.is_empty(), "{:?}", sweep.errors);
                    assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Working);
                    assert_eq!(
                        h.calls
                            .borrow()
                            .iter()
                            .filter(|c| c.starts_with("prompt_agent"))
                            .count(),
                        1
                    );
                }
            }
        }
    }

    #[test]
    fn proven_prompt_rejection_recovers_once_after_readiness() {
        for code in ["agent_blocked", "agent_not_ready"] {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state");
            let store = Store::open(&state, dir.path()).unwrap();
            store
                .update(|data| {
                    data.tickets.push(crate::store::Ticket {
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
            h.prompt_results.borrow_mut().push_back((
                false,
                Err(ShepherdError::HerdrCommand {
                    code: code.into(),
                    message: "rejected before input".into(),
                }),
            ));
            let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
            assert_eq!(record.status, AgentStatus::Blocked);
            assert!(record.prompt_pending);
            assert!(h.accepted_prompts.borrow().is_empty());
            h.agent_records.borrow_mut()[0].cwd = Some(dir.path().into());
            crate::flock::reconcile(&h, &store).unwrap();
            crate::flock::reconcile(&h, &store).unwrap();
            assert_eq!(h.accepted_prompts.borrow().len(), 1);
            assert!(!store.read().unwrap().agents[0].prompt_pending);
            assert_eq!(
                h.calls
                    .borrow()
                    .iter()
                    .filter(|c| c.starts_with("prompt_agent"))
                    .count(),
                2
            );
        }
    }

    #[test]
    fn launch_save_preserves_completed_assignment_history() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        let snapshot = launch(&h, &store, req(dir.path(), &state)).unwrap();
        let completed = crate::flock::store_report(&store, &snapshot.name,
            r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        // The report does not take the launch lock and may land after a reload.
        save(&store, &snapshot).unwrap();
        let saved = store.read().unwrap().agents.remove(0);
        assert_eq!(saved.completed_tasks, completed.completed_tasks);
        assert_eq!(saved.report, completed.report);
        assert!(saved.prompt_claim().is_none());
    }

    #[test]
    fn interrupted_prompt_claim_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        *h.on_prompt.borrow_mut() = Some(Box::new(|_| panic!("dispatch interrupted")));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            launch(&h, &store, req(dir.path(), &state))
        }));
        assert!(result.is_err());
        let record = store.read().unwrap().agents.remove(0);
        assert!(!record.prompt_pending);
        assert_eq!(record.status, AgentStatus::Blocked);
        // A missing projection does not prove nonacceptance either.
        h.agent_records.borrow_mut().clear();
        crate::flock::reconcile(&h, &store).unwrap();
        assert_eq!(store.read().unwrap().agents[0].status, AgentStatus::Blocked);
        assert!(crate::flock::store_report(&store, &record.name, r#"{"status":"completed","summary":"late report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).is_ok());
        assert_eq!(
            store.read().unwrap().agents[0].status,
            AgentStatus::Reported
        );
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("prompt_agent"))
                .count(),
            1
        );
    }

    #[test]
    fn real_report_during_ambiguous_delivery_keeps_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
        let h = FakeHerdr::new(vec![]);
        let observed = store.clone();
        *h.on_prompt.borrow_mut() = Some(Box::new(move |name| {
            crate::flock::store_report(&observed, name, r#"{"status":"completed","summary":"real report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
        }));
        h.prompt_results.borrow_mut().push_back((
            true,
            Err(ShepherdError::HerdrCommand {
                code: "timeout".into(),
                message: "late observation".into(),
            }),
        ));
        let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
        assert_eq!(record.status, AgentStatus::Reported);
        assert_eq!(record.report.as_ref().unwrap().summary, "real report");
        assert!(!record.prompt_pending);
        crate::flock::reconcile(&h, &store).unwrap();
        assert!(!crate::flock::deliver_pending_prompt(&h, &store, &record.name).unwrap());
        assert_eq!(h.accepted_prompts.borrow().len(), 1);
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
    fn cancellation_drains_direct_launch_before_retiring() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let store = Store::open(&state, dir.path()).unwrap();
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
        let h = FakeHerdr::new(vec![]);
        let observed = store.clone();
        let (early_tx, early_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        *h.on_start.borrow_mut() = Some(Box::new(move |_| {
            let observed = observed.clone();
            let done = done_tx.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result = crate::ticket::intake::cancel_agents(
                    &FakeHerdr::new(vec![]),
                    &observed,
                    "T-1",
                    "cancel",
                    3,
                );
                let _ = tx.send(());
                done.send(result).unwrap();
            });
            early_tx
                .send(
                    rx.recv_timeout(std::time::Duration::from_millis(200))
                        .is_ok(),
                )
                .unwrap();
        }));
        let launched = launch(&h, &store, req(dir.path(), &state));
        let cancelled = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            !early_rx.recv().unwrap(),
            "cancellation returned before direct launch completed"
        );
        launched.unwrap();
        cancelled.unwrap();
        let data = store.read().unwrap();
        assert_eq!(data.tickets[0].state, crate::store::TicketState::Closed);
        assert_eq!(data.agents[0].status, AgentStatus::Retired);
        assert!(!crate::flock::deliver_pending_prompt(&h, &store, &data.agents[0].name).unwrap());
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
    fn automatic_startup_fallback_preserves_report_and_retirement() {
        for outcome in ["report", "retire", "uncertain"] {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state");
            let store = Store::open(&state, dir.path()).unwrap();
            let h = FakeHerdr::new(vec![]);
            h.start_results.borrow_mut().push_back(AgentStart::NotReady);
            h.visible_screens
                .borrow_mut()
                .push_back("rate limit reached".into());
            let callback_store = store.clone();
            *h.on_start.borrow_mut() = Some(Box::new(move |name| match outcome {
                "report" => {
                    crate::flock::store_report(&callback_store, name, r#"{"status":"completed","summary":"real report","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#).unwrap();
                }
                "retire" => {
                    callback_store
                        .update(|data| {
                            data.agents[0].status = AgentStatus::Retired;
                            Ok(())
                        })
                        .unwrap();
                }
                _ => {
                    callback_store
                        .update(|data| {
                            data.agents[0].status = AgentStatus::Blocked;
                            data.agents[0].set_prompt_delivery_uncertain(true);
                            Ok(())
                        })
                        .unwrap();
                }
            }));
            let record = launch(&h, &store, req(dir.path(), &state)).unwrap();
            assert_eq!(record.runner, "codex-sol-high");
            match outcome {
                "report" => assert_eq!(record.report.unwrap().summary, "real report"),
                "retire" => assert_eq!(record.status, AgentStatus::Retired),
                _ => assert!(record.prompt_delivery_uncertain()),
            }
            assert_eq!(h.start_args.borrow().len(), 1);
            assert!(
                !h.calls
                    .borrow()
                    .iter()
                    .any(|call| call.starts_with("send_keys ")
                        || call.starts_with("close_tab ")
                        || call.starts_with("prompt_agent "))
            );
        }
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
        let child = dir.path().join("child");
        std::fs::create_dir(&child).unwrap();
        store
            .update(|data| {
                data.tickets.push(crate::store::Ticket {
                    id: "T-1".into(),
                    state: crate::store::TicketState::Building,
                    worktree: Some(dir.path().into()),
                    workspace_id: Some("w1".into()),
                    children: vec![crate::store::ChildWorktree {
                        unit: 1,
                        branch: "feat/t--1".into(),
                        path: child.clone(),
                        workspace_id: Some("w-legacy".into()),
                        base_sha: "fixture".into(),
                        integrated: false,
                    }],
                    ..Default::default()
                });
                Ok(())
            })
            .unwrap();
        let record = launch(&h, &store, req(&child, &state)).unwrap();
        assert_eq!(record.worktree, child);
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| **c == format!("create_tab w1 coder {}", child.display()))
                .count(),
            2
        );
        h.agent_records.borrow_mut()[0].workspace_id = Some("w1".into());
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
        drop(calls);
        assert!(crate::flock::retire_automatically(&h, &store, &record).unwrap());
        let args = h.start_args.borrow();
        assert!(!args[0].contains(&"--config".into()));
        assert_eq!(args[1][args[1].len() - 2], "--config");
        assert_eq!(
            std::fs::read_to_string(args[1].last().unwrap()).unwrap(),
            "paste:\n  largeMenuThreshold: 0\n"
        );
    }
}
