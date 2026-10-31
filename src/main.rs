use std::path::PathBuf;

use clap::{Parser, Subcommand, error::ErrorKind};
use herdr_shepherd::{
    config,
    error::ShepherdError,
    herdr::{Herdr, cli::CliHerdr},
    output,
    probe::SystemProbe,
    proc,
    upkeep::{event, prune, regroup},
};

#[derive(Parser)]
#[command(
    name = "herdr-shepherd",
    about = "Herdr upkeep and coding agent orchestration"
)]
struct Cli {
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the shepherd version.
    Version,
    /// Relabel and pack linked worktree workspaces.
    Regroup,
    /// Handle a plugin event supplied by herdr.
    Event,
    /// Report stale worktrees without changing them.
    Prune {
        #[arg(long)]
        no_size: bool,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Validate the resolved shepherd configuration.
    ValidateConfig {
        #[arg(long)]
        project_root: Option<PathBuf>,
    },
}

fn run(cli: &Cli) -> Result<(), ShepherdError> {
    match &cli.command {
        Commands::Version => {
            let version = env!("CARGO_PKG_VERSION");
            output::emit(cli.json, &serde_json::json!({ "version": version }), || {
                format!("herdr-shepherd {version}")
            });
        }
        Commands::ValidateConfig { project_root } => {
            let current = project_root_from_cwd()?;
            let cfg = config::load(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                Some(project_root.as_deref().unwrap_or(&current)),
            )?;
            output::emit(cli.json, &cfg, || {
                serde_json::to_string_pretty(&cfg).expect("config is serializable")
            });
        }
        Commands::Regroup => {
            let h = CliHerdr::from_env();
            let report = regroup::apply(&h, &regroup::plan_regroup(&h.workspaces()?))?;
            output::emit(cli.json, &report, || {
                format!(
                    "herdr-shepherd: {} relabeled, {} group(s) repacked",
                    report.relabeled, report.repacked
                )
            });
        }
        Commands::Event => {
            let name = std::env::var("HERDR_PLUGIN_EVENT")
                .map_err(|_| ShepherdError::Usage("HERDR_PLUGIN_EVENT is required".into()))?;
            let payload = std::env::var("HERDR_PLUGIN_EVENT_JSON")
                .map_err(|_| ShepherdError::Usage("HERDR_PLUGIN_EVENT_JSON is required".into()))?;
            let payload = serde_json::from_str(&payload).map_err(|err| {
                ShepherdError::Usage(format!("invalid HERDR_PLUGIN_EVENT_JSON: {err}"))
            })?;
            let cfg = config::load(&config::resolve_config_dir(cli.config_dir.as_deref()), None)?;
            let outcome = event::handle_event(&name, &payload, &cfg, &CliHerdr::from_env())?;
            output::emit(cli.json, &outcome, || {
                format!("herdr-shepherd: {outcome:?}")
            });
        }
        Commands::Prune { no_size, all, root } => {
            let root = root
                .clone()
                .unwrap_or_else(|| home().join(".herdr/worktrees"));
            let h = CliHerdr::from_env();
            let mut warnings = Vec::new();
            let ws = h.workspaces().unwrap_or_else(|err| {
                warnings.push(format!("herdr workspace information unavailable: {err}"));
                Vec::new()
            });
            let agents = h.agents().unwrap_or_else(|err| {
                warnings.push(format!("herdr agent information unavailable: {err}"));
                Vec::new()
            });
            let entries = prune::scan(&root, &ws, &agents, &SystemProbe, !no_size);
            let shown: Vec<_> = entries
                .iter()
                .filter(|entry| *all || entry.kind != prune::Kind::Open)
                .collect();
            output::emit(
                cli.json,
                &serde_json::json!({
                    "entries": shown,
                    "warnings": warnings,
                    "total": entries.len(),
                    "hidden_open": entries.len() - shown.len(),
                }),
                || {
                    let mut report = prune::human_report(&entries, *all);
                    for warning in &warnings {
                        report.push_str(&format!("\nwarning: {warning}"));
                    }
                    report
                },
            );
        }
    }
    Ok(())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn project_root_from_cwd() -> Result<PathBuf, ShepherdError> {
    let cwd = std::env::current_dir().map_err(anyhow::Error::from)?;
    let root = proc::run(
        std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]),
        std::time::Duration::from_secs(20),
    );
    Ok(root
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .map(|s| PathBuf::from(s.trim()))
        .unwrap_or(cwd))
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let json = args.iter().any(|arg| arg == "--json");
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                print!("{error}");
                return;
            }
            let error = ShepherdError::Usage(error.to_string());
            output::emit_error(json, &error);
            std::process::exit(error.exit_code());
        }
    };
    if let Err(error) = run(&cli) {
        output::emit_error(cli.json, &error);
        std::process::exit(error.exit_code());
    }
}
