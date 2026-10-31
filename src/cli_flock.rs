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
                .cloned()
                .ok_or_else(|| ShepherdError::Usage(format!("unknown role: {role}")))?;
            let data = store.read()?;
            let ticket = data
                .tickets
                .iter()
                .find(|t| &t.id == ticket)
                .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {ticket}")))?;
            let task = crate::cli_pr::read_text(task_file)?;
            let worktree = ticket
                .worktree
                .clone()
                .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
            let workspace_id = ticket
                .workspace_id
                .clone()
                .ok_or_else(|| ShepherdError::Usage("ticket has no workspace".into()))?;
            if unit.is_some() {
                return Err(ShepherdError::Usage(
                    "child worktrees are not wired yet".into(),
                ));
            }
            let bin = std::env::current_exe().map_err(anyhow::Error::from)?;
            let prompt = format!(
                "Ticket {}: {}\n{}\nTask: {task}\nStay on branch {} in worktree {}. Never switch branches or write outside it. Never read .env*, Credentials/, *.pem, *.key or other secret files. Never push, open PRs, or call GitHub. Label findings High (Blocker) | Mid | Low | Wish. Report using: {} agent report --agent {{{{agent_name}}}} --report-file <path>\nJSON: {{\"status\":\"completed|blocked|failed\",\"summary\":\"text\",\"tests\":[],\"changed_paths\":[],\"blockers\":[],\"findings\":[],\"commit_sha\":null}}",
                ticket.id,
                ticket.title,
                ticket.body,
                ticket.branch,
                worktree.display(),
                bin.display()
            );
            let runners = role
                .runners
                .iter()
                .map(|name| resolved.runners[name].clone())
                .collect();
            let tab_label = role.name.clone();
            serde_json::to_value(flock::spawn(
                h,
                &store,
                &cfg,
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
            )?)
            .expect("serializable")
        }
    };
    output::emit(json, &data, || {
        serde_json::to_string_pretty(&data).expect("serializable")
    });
    Ok(())
}
