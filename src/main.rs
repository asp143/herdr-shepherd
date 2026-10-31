mod cli_pr;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, error::ErrorKind};
use herdr_shepherd::{
    config,
    error::ShepherdError,
    herdr::{Herdr, cli::CliHerdr},
    output,
    probe::SystemProbe,
    proc,
    upkeep::{event, keys, prune, regroup},
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
    #[arg(
        long,
        global = true,
        env = "HERDR_PLUGIN_STATE_DIR",
        hide_env_values = true
    )]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Emit configured lifecycle hooks.
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    /// Remove a verified merged ticket worktree.
    Cleanup {
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force_discard: bool,
    },
    /// Poll linked-worktree PRs and report sidebar metadata.
    Watch {
        #[arg(long, conflicts_with = "detach")]
        once: bool,
        #[arg(long)]
        detach: bool,
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Install or run the per-worktree push guard.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
    /// Read and update GitHub pull requests through the gate.
    Pr {
        #[command(subcommand)]
        command: cli_pr::PrCommand,
    },
    /// Gate gh calls from agent panes.
    Shim {
        #[command(subcommand)]
        command: ShimCommand,
    },
    /// Print the shepherd version.
    Version,
    /// Relabel and pack linked worktree workspaces.
    Regroup,
    /// Handle a plugin event supplied by herdr.
    Event,
    /// Print suggested bindings without editing herdr configuration.
    Keys {
        #[arg(long)]
        herdr_config: Option<PathBuf>,
    },
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
    /// Inspect tickets for the invoking project.
    Ticket {
        #[arg(long, global = true)]
        project_root: Option<PathBuf>,
        #[command(subcommand)]
        command: TicketCommands,
    },
}

#[derive(Subcommand)]
enum TicketCommands {
    List,
    Show { id: String },
    Fetch { id: String },
}

#[derive(serde::Serialize)]
struct ValidatedConfig {
    #[serde(flatten)]
    config: config::Config,
    #[serde(flatten)]
    resolved: herdr_shepherd::roles::Resolved,
}

#[derive(Subcommand)]
enum ShimCommand {
    Gh {
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    Install {
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long)]
        branch: String,
    },
    PrePush {
        remote: Option<String>,
        url: Option<String>,
    },
}

#[derive(Subcommand)]
enum HooksCommand {
    Emit {
        event: String,
        #[arg(long)]
        json_file: PathBuf,
    },
}

fn state_dir(cli: &Cli) -> PathBuf {
    cli.state_dir
        .clone()
        .unwrap_or_else(|| home().join(".local/state/herdr/plugins/herdr-shepherd"))
}

fn run(cli: &Cli) -> Result<(), ShepherdError> {
    match &cli.command {
        Commands::Hooks {
            command: HooksCommand::Emit { event, json_file },
        } => {
            if !herdr_shepherd::hooks::known_event(event) {
                return Err(ShepherdError::Usage(format!(
                    "unknown lifecycle event: {event}"
                )));
            }
            let payload = serde_json::from_str(&cli_pr::read_text(json_file)?)
                .map_err(|error| ShepherdError::Usage(format!("invalid event JSON: {error}")))?;
            let root = resolve_project_root(None)?;
            let cfg = config::load(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                Some(&root),
            )?;
            herdr_shepherd::hooks::emit(&cfg, event, &payload);
            output::emit(cli.json, &serde_json::json!({"event":event}), || {
                format!("shepherd: emitted {event}")
            });
        }
        Commands::Cleanup {
            worktree,
            dry_run,
            force_discard,
        } => {
            let root = cli_pr::git(worktree, &["rev-parse", "--show-toplevel"])?;
            let cfg = config::load(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                Some(Path::new(&root)),
            )?;
            let h = CliHerdr::from_env();
            let preflight = herdr_shepherd::cleanup::preflight(
                &h,
                &herdr_shepherd::github::gh::GhBackend,
                &cfg,
                Path::new(&root),
                *force_discard,
                *dry_run,
            )?;
            let report = herdr_shepherd::cleanup::run(preflight, *dry_run)?;
            output::emit(cli.json, &report, || {
                serde_json::to_string_pretty(&report).expect("serializable")
            });
        }
        Commands::Watch {
            once,
            detach,
            interval,
        } => {
            let config_dir = config::resolve_config_dir(cli.config_dir.as_deref());
            let cfg = config::load(&config_dir, None)?;
            let outcome = if *detach {
                herdr_shepherd::watch::detach(&state_dir(cli), Some(&config_dir), *interval)?
            } else {
                herdr_shepherd::watch::run(
                    &state_dir(cli),
                    &CliHerdr::from_env(),
                    &herdr_shepherd::github::gh::GhBackend,
                    &cfg,
                    *once,
                    *interval,
                )?
            };
            output::emit(cli.json, &outcome, || outcome.status.clone());
        }
        Commands::Hook { command } => match command {
            HookCommand::Install { worktree, branch } => {
                herdr_shepherd::gate::hook::install(
                    worktree,
                    branch,
                    &state_dir(cli),
                    &std::env::current_exe().map_err(anyhow::Error::from)?,
                )?;
                output::emit(cli.json, &serde_json::json!({"installed":true}), || {
                    "shepherd: pre-push hook installed".into()
                });
            }
            HookCommand::PrePush { .. } => {
                use std::io::Read;
                let mut input = String::new();
                std::io::stdin()
                    .read_to_string(&mut input)
                    .map_err(anyhow::Error::from)?;
                if let Err(error) = herdr_shepherd::gate::hook::pre_push(
                    &std::env::current_dir().map_err(anyhow::Error::from)?,
                    &input,
                ) {
                    eprintln!("shepherd: push blocked: {error}");
                    std::process::exit(1);
                }
            }
        },
        Commands::Pr { command } => cli_pr::run(
            command,
            &config::resolve_config_dir(cli.config_dir.as_deref()),
            cli.json,
        )?,
        Commands::Shim {
            command: ShimCommand::Gh { args },
        } => {
            if let Err(error) = herdr_shepherd::gate::shim::run(args, &state_dir(cli)) {
                if matches!(error, ShepherdError::PolicyDenied(_)) {
                    eprintln!("shepherd: blocked: {error}");
                    std::process::exit(4);
                }
                return Err(error);
            }
        }
        Commands::Ticket {
            project_root,
            command,
        } => {
            let project = resolve_project_root(project_root.as_deref())?;
            let read_store = || {
                let state_dir = cli.state_dir.as_deref().ok_or_else(|| {
                    ShepherdError::Usage("--state-dir or HERDR_PLUGIN_STATE_DIR is required".into())
                })?;
                herdr_shepherd::store::Store::open(state_dir, &project)?.read()
            };
            match command {
                TicketCommands::List => {
                    let data = read_store()?;
                    output::emit(cli.json, &data.tickets, || {
                        serde_json::to_string_pretty(&data.tickets)
                            .expect("tickets are serializable")
                    });
                }
                TicketCommands::Show { id } => {
                    let data = read_store()?;
                    let ticket = data
                        .tickets
                        .iter()
                        .find(|t| t.id == *id)
                        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
                    output::emit(cli.json, ticket, || {
                        serde_json::to_string_pretty(ticket).expect("ticket is serializable")
                    });
                }
                TicketCommands::Fetch { id } => {
                    let cfg = config::load(
                        &config::resolve_config_dir(cli.config_dir.as_deref()),
                        Some(&project),
                    )?;
                    let ticket = herdr_shepherd::ticket::source::fetch(&cfg, id)?;
                    output::emit(cli.json, &ticket, || {
                        serde_json::to_string_pretty(&ticket).expect("ticket is serializable")
                    });
                }
            }
        }
        Commands::Version => {
            let version = env!("CARGO_PKG_VERSION");
            output::emit(cli.json, &serde_json::json!({ "version": version }), || {
                format!("herdr-shepherd {version}")
            });
        }
        Commands::ValidateConfig { project_root } => {
            let current = resolve_project_root(project_root.as_deref())?;
            let cfg = config::load(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                Some(&current),
            )?;
            let validated = ValidatedConfig {
                resolved: herdr_shepherd::roles::resolve(&cfg)?,
                config: cfg,
            };
            output::emit(cli.json, &validated, || {
                serde_json::to_string_pretty(&validated).expect("config is serializable")
            });
        }
        Commands::Keys { herdr_config } => {
            let cfg = config::load(&config::resolve_config_dir(cli.config_dir.as_deref()), None)?;
            let path = herdr_config
                .clone()
                .unwrap_or_else(|| home().join(".config/herdr/config.toml"));
            let invalid = |message: String| ShepherdError::Config {
                path: path.clone(),
                message,
            };
            let text = if cfg.upkeep.keybindings {
                match std::fs::read_to_string(&path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(err) => return Err(invalid(err.to_string())),
                }
            } else {
                String::new()
            };
            text.parse::<toml::Table>()
                .map_err(|err| invalid(err.to_string()))?;
            let snippet = keys::keys_snippet(&text, cfg.upkeep.keybindings);
            output::emit(cli.json, &snippet, || {
                let mut text = snippet.toml.clone();
                for key in &snippet.skipped {
                    text.push_str(&format!("# skipped existing user binding: {key}\n"));
                }
                text
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
            let outcome = event::handle_event_in_state(
                &name,
                &payload,
                &cfg,
                &CliHerdr::from_env(),
                &state_dir(cli),
            )?;
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

fn resolve_project_root(explicit: Option<&Path>) -> Result<PathBuf, ShepherdError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    let context = std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .map(|text| serde_json::from_str::<serde_json::Value>(&text))
        .transpose()
        .map_err(|err| ShepherdError::Usage(format!("invalid HERDR_PLUGIN_CONTEXT_JSON: {err}")))?;
    let cwd = context
        .as_ref()
        .and_then(|context| {
            [
                context.pointer("/worktree/checkout_path"),
                context.get("focused_pane_cwd"),
                context.get("workspace_cwd"),
            ]
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .find(|path| !path.is_empty())
            .map(PathBuf::from)
        })
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(anyhow::Error::from)?;
    let root = proc::run(
        std::process::Command::new("git")
            .current_dir(&cwd)
            .args(["rev-parse", "--show-toplevel"]),
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
