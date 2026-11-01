use clap::Subcommand;
use herdr_shepherd::{
    error::ShepherdError, flock, herdr::Herdr, launch::LaunchRequest, output, roles, store::Store,
};
use std::path::Path;

#[derive(Subcommand)]
pub enum AgentCommand {
    Spawn {
        #[arg(long)]
        ticket: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        task_file: std::path::PathBuf,
        #[arg(long)]
        unit: Option<u32>,
    },
    Report {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        report_file: std::path::PathBuf,
    },
    List {
        #[arg(long)]
        ticket: Option<String>,
    },
    Status {
        name: String,
    },
    Cancel {
        name: String,
        #[arg(long)]
        force: bool,
    },
}

pub fn run(
    command: &AgentCommand,
    h: &dyn Herdr,
    root: &Path,
    state: &Path,
    config_dir: &Path,
    json: bool,
) -> Result<(), ShepherdError> {
    let store = Store::open(state, root)?;
    let cfg = herdr_shepherd::trusted::committed_config(config_dir, root, None, state)?;
    let data = match command {
        AgentCommand::List { ticket } => serde_json::to_value(
            store
                .read()?
                .agents
                .into_iter()
                .filter(|a| ticket.as_ref().is_none_or(|t| t == &a.ticket))
                .collect::<Vec<_>>(),
        )
        .expect("serializable"),
        AgentCommand::Status { name } => serde_json::to_value(
            store
                .read()?
                .agents
                .into_iter()
                .find(|a| &a.name == name)
                .ok_or_else(|| ShepherdError::Usage(format!("unknown agent: {name}")))?,
        )
        .expect("serializable"),
        AgentCommand::Report { agent, report_file } => {
            let text = crate::cli_pr::read_text(report_file)?;
            flock::validate_report(&text)?;
            if let Ok(caller) = std::env::var("HERDR_PANE_ID") {
                let data = store.read()?;
                let rec = data
                    .agents
                    .iter()
                    .find(|a| &a.name == agent)
                    .ok_or_else(|| ShepherdError::Usage(format!("unknown agent: {agent}")))?;
                if rec.pane_id.as_deref() != Some(caller.as_str()) {
                    return Err(ShepherdError::PolicyDenied(
                        "report caller pane does not own this agent".into(),
                    ));
                }
            }
            serde_json::to_value(flock::report(h, &store, &cfg, agent, &text)?)
                .expect("serializable")
        }
        AgentCommand::Cancel { name, force } => {
            serde_json::to_value(flock::cancel(h, &store, name, *force)?).expect("serializable")
        }
        AgentCommand::Spawn {
            ticket,
            role,
            task_file,
            unit,
        } => {
            let _hooks = herdr_shepherd::hooks::defer();
            let _ticket_operation = herdr_shepherd::ticket::operation::shared(&store, ticket)?;
            let _placement = spawn_placement_lock(&store)?;
            let resolved = roles::resolve(&cfg)?;
            let role = resolved
                .roles
                .iter()
                .find(|r| &r.name == role)
                .ok_or_else(|| ShepherdError::Usage(format!("unknown role: {role}")))?;
            let data = store.read()?;
            let mut ticket = data
                .tickets
                .iter()
                .find(|t| &t.id == ticket)
                .cloned()
                .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {ticket}")))?;
            if !herdr_shepherd::ticket::state::is_active(ticket.state) {
                return Err(ShepherdError::Usage("ticket is not active".into()));
            }
            let parallel = data.agents.iter().any(|a| {
                a.ticket == ticket.id
                    && a.role == role.name
                    && matches!(
                        a.status,
                        herdr_shepherd::store::AgentStatus::Starting
                            | herdr_shepherd::store::AgentStatus::Working
                            | herdr_shepherd::store::AgentStatus::Blocked
                    )
            });
            let child_mode = role.worktree == herdr_shepherd::roles::WorktreeMode::Child;
            if parallel && child_mode && unit.is_none() {
                return Err(ShepherdError::Usage(
                    "parallel coders require --unit".into(),
                ));
            }
            if unit.is_some() && !child_mode {
                return Err(ShepherdError::Usage(
                    "--unit requires a child-worktree role".into(),
                ));
            }
            let task = crate::cli_pr::read_text(task_file)?;
            let mut label = None;
            if child_mode && unit.is_some() {
                let unit = unit.expect("checked unit");
                if ticket
                    .child_cleanup
                    .iter()
                    .any(|receipt| receipt.child.unit == unit)
                {
                    return Err(ShepherdError::Usage(
                        "child unit has outstanding cleanup; finish cleanup before spawning it again".into(),
                    ));
                }
                if data
                    .agents
                    .iter()
                    .filter(|a| {
                        a.role != "lead"
                            && matches!(
                                a.status,
                                herdr_shepherd::store::AgentStatus::Starting
                                    | herdr_shepherd::store::AgentStatus::Working
                                    | herdr_shepherd::store::AgentStatus::Blocked
                            )
                    })
                    .count()
                    >= resolved.shepherd.max_parallel_agents
                {
                    return Err(ShepherdError::Capacity);
                }
                let child = if let Some(child) = ticket.children.iter().find(|c| c.unit == unit) {
                    if data.agents.iter().any(|a| {
                        a.worktree == child.path
                            && matches!(
                                a.status,
                                herdr_shepherd::store::AgentStatus::Starting
                                    | herdr_shepherd::store::AgentStatus::Working
                                    | herdr_shepherd::store::AgentStatus::Blocked
                            )
                    }) {
                        return Err(ShepherdError::Usage(
                            "child unit already has an active agent".into(),
                        ));
                    }
                    child.clone()
                } else {
                    let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
                    let child = herdr_shepherd::ticket::children::create_child_configured(
                        h, &ticket, unit, state, &bin, config_dir,
                    )?;
                    store.update(|data| {
                        let t = data
                            .tickets
                            .iter_mut()
                            .find(|t| t.id == ticket.id)
                            .ok_or_else(|| ShepherdError::Usage("ticket disappeared".into()))?;
                        t.children.push(child.clone());
                        Ok(())
                    })?;
                    child
                };
                ticket.worktree = Some(child.path);
                ticket.branch = child.branch;
                label = Some(format!("{}-{unit}", role.name));
            }
            let rec = spawn_task_request(
                h,
                &store,
                &cfg,
                TaskRequest {
                    state,
                    ticket: &ticket,
                    role_name: &role.name,
                    task: &task,
                    tab_label: label.as_deref(),
                },
            )?;
            drop(_placement);
            drop(_ticket_operation);
            drop(_hooks);
            // A synchronous spawn hook may have reported or retired the agent.
            let rec = store
                .read()?
                .agents
                .into_iter()
                .find(|agent| agent.name == rec.name)
                .ok_or_else(|| ShepherdError::Usage("launch record disappeared".into()))?;
            serde_json::to_value(rec).expect("serializable")
        }
    };
    output::emit(json, &data, || {
        serde_json::to_string_pretty(&data).expect("serializable")
    });
    Ok(())
}

pub struct TaskRequest<'a> {
    pub state: &'a Path,
    pub ticket: &'a herdr_shepherd::store::Ticket,
    pub role_name: &'a str,
    pub task: &'a str,
    pub tab_label: Option<&'a str>,
}

pub fn spawn_task(
    h: &dyn Herdr,
    store: &Store,
    cfg: &herdr_shepherd::config::Config,
    state: &Path,
    ticket: &herdr_shepherd::store::Ticket,
    role_name: &str,
    task: &str,
) -> Result<herdr_shepherd::store::AgentRecord, ShepherdError> {
    spawn_task_request(
        h,
        store,
        cfg,
        TaskRequest {
            state,
            ticket,
            role_name,
            task,
            tab_label: None,
        },
    )
}

pub fn spawn_task_request(
    h: &dyn Herdr,
    store: &Store,
    cfg: &herdr_shepherd::config::Config,
    request: TaskRequest<'_>,
) -> Result<herdr_shepherd::store::AgentRecord, ShepherdError> {
    let TaskRequest {
        state,
        ticket,
        role_name,
        task,
        tab_label: requested_label,
    } = request;
    let resolved = roles::resolve(cfg)?;
    let role = resolved
        .roles
        .iter()
        .find(|r| r.name == role_name)
        .cloned()
        .ok_or_else(|| ShepherdError::Usage(format!("unknown role: {role_name}")))?;
    let worktree = ticket
        .worktree
        .clone()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    let workspace_id = ticket
        .workspace_id
        .clone()
        .ok_or_else(|| ShepherdError::Usage("ticket has no workspace".into()))?;
    let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
    let first = &resolved.runners[&role.runners[0]];
    let harness = herdr_shepherd::harness::harness_for(&first.harness)?;
    let prompt = herdr_shepherd::prompts::role_prompt(
        &bin,
        ticket,
        &role,
        task,
        "{{agent_name}}",
        harness.as_ref(),
    )
    .replace(harness.subagent_hint(), "{{subagent_hint}}");
    let runners = role
        .runners
        .iter()
        .map(|name| resolved.runners[name].clone())
        .collect();
    let tab_label = requested_label.unwrap_or(&role.name).to_owned();
    flock::spawn(
        h,
        store,
        cfg,
        LaunchRequest {
            ticket: ticket.id.clone(),
            role,
            runners,
            worktree,
            workspace_id,
            tab_label,
            prompt,
            state_dir: state.into(),
        },
    )
}

fn spawn_placement_lock(store: &Store) -> Result<std::fs::File, ShepherdError> {
    std::fs::create_dir_all(store.root()).map_err(anyhow::Error::from)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(store.root().join("spawn.lock"))
        .map_err(anyhow::Error::from)?;
    lock.lock().map_err(anyhow::Error::from)?;
    Ok(lock)
}
