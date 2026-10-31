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
        AgentCommand::Report { agent, report_file } => serde_json::to_value(flock::report(
            h,
            &store,
            &cfg,
            agent,
            &crate::cli_pr::read_text(report_file)?,
        )?)
        .expect("serializable"),
        AgentCommand::Cancel { name, force } => {
            serde_json::to_value(flock::cancel(h, &store, name, *force)?).expect("serializable")
        }
        AgentCommand::Spawn {
            ticket,
            role,
            task_file,
            unit,
        } => {
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
            if child_mode && parallel {
                let unit = unit.expect("checked unit");
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
                    child.clone()
                } else {
                    let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
                    let child = herdr_shepherd::ticket::children::create_child(
                        h, &ticket, unit, state, &bin,
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
                    herdr_shepherd::gate::hook::install_configured(
                        &child.path,
                        &child.branch,
                        state,
                        &bin,
                        config_dir,
                    )?;
                    child
                };
                ticket.worktree = Some(child.path);
                ticket.workspace_id = Some(child.workspace_id);
                ticket.branch = child.branch;
                label = Some(format!("{}-{unit}", role.name));
            }
            serde_json::to_value(spawn_task_request(
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
            )?)
            .expect("serializable")
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
    let prompt = format!(
        "Ticket {}: {}\n{}\nAcceptance: {:?}\nTask: {task}\nStay on branch {} in worktree {}. Never switch branches or write outside it. Never read .env*, Credentials/, *.pem, *.key or other secret files. Never push, open PRs, or call GitHub. Label findings High (Blocker) | Mid | Low | Wish. Write JSON report inside the assigned worktree, run {} agent report --agent {{{{agent_name}}}} --report-file <path>, then remove the report file after success. JSON: {{\"status\":\"completed|blocked|failed\",\"summary\":\"text\",\"tests\":[],\"changed_paths\":[],\"blockers\":[],\"findings\":[],\"commit_sha\":null}}",
        ticket.id,
        ticket.title,
        ticket.body,
        ticket.acceptance,
        ticket.branch,
        worktree.display(),
        bin.display()
    );
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
