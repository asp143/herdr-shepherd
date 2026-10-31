mod cli_flock;
mod cli_pr;
mod cli_ticket;
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
    Agent {
        #[arg(long, global = true)]
        project_root: Option<PathBuf>,
        #[command(subcommand)]
        command: cli_flock::AgentCommand,
    },
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
    /// Summarize tickets and cached branch protection warnings.
    Status {
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
    Pr {
        id: String,
        #[arg(long)]
        summary_file: PathBuf,
        #[arg(long)]
        changes_file: PathBuf,
        #[arg(long)]
        testing_file: PathBuf,
    },
    Comments {
        id: String,
        #[arg(long)]
        new: bool,
    },
    Reply {
        id: String,
        #[arg(long)]
        comment: u64,
        #[arg(long)]
        body_file: PathBuf,
    },
    Revise {
        id: String,
        #[arg(long, value_delimiter = ',')]
        comments: Vec<u64>,
        #[arg(long)]
        task_file: PathBuf,
    },
    Ack {
        id: String,
        #[arg(long)]
        comment: u64,
        #[arg(long)]
        body_file: PathBuf,
    },
    Add {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body_file: PathBuf,
        #[arg(long)]
        acceptance_file: Option<PathBuf>,
        #[arg(long = "type", default_value = "feat")]
        kind: String,
        #[arg(long)]
        base: Option<String>,
    },
    Cancel {
        id: String,
    },
    Dispatch,
    Transition {
        id: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        reason: Option<String>,
    },
    List,
    Show {
        id: String,
    },
    Fetch {
        id: String,
    },
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
        Commands::Agent {
            project_root,
            command,
        } => {
            let root = resolve_project_root(project_root.as_deref())?;
            cli_flock::run(
                command,
                herdr_client()?.as_ref(),
                &root,
                &state_dir(cli),
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                cli.json,
            )?;
        }
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
            let cfg = herdr_shepherd::trusted::committed_config(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                &root,
                None,
                &state_dir(cli),
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
            let cfg = herdr_shepherd::trusted::committed_config(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                Path::new(&root),
                None,
                &state_dir(cli),
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
                herdr_shepherd::gate::hook::install_configured(
                    worktree,
                    branch,
                    &state_dir(cli),
                    &std::env::current_exe().map_err(anyhow::Error::from)?,
                    &config::resolve_config_dir(cli.config_dir.as_deref()),
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
                if let Err(error) = herdr_shepherd::gate::hook::pre_push_configured(
                    &std::env::current_dir().map_err(anyhow::Error::from)?,
                    &input,
                    &state_dir(cli),
                    &config::resolve_config_dir(cli.config_dir.as_deref()),
                ) {
                    eprintln!("shepherd: push blocked: {error}");
                    std::process::exit(1);
                }
            }
        },
        Commands::Pr { command } => cli_pr::run(
            command,
            &config::resolve_config_dir(cli.config_dir.as_deref()),
            &state_dir(cli),
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
                TicketCommands::Pr { .. }
                | TicketCommands::Comments { .. }
                | TicketCommands::Reply { .. }
                | TicketCommands::Revise { .. }
                | TicketCommands::Ack { .. } => {
                    let result = run_ticket_pr(command, &project, cli)?;
                    output::emit(cli.json, &result, || {
                        serde_json::to_string_pretty(&result).expect("serializable")
                    });
                }
                TicketCommands::Add {
                    id,
                    title,
                    body_file,
                    acceptance_file,
                    kind,
                    base,
                } => {
                    let store = herdr_shepherd::store::Store::open(&state_dir(cli), &project)?;
                    let cfg = herdr_shepherd::trusted::committed_config(
                        &config::resolve_config_dir(cli.config_dir.as_deref()),
                        &project,
                        None,
                        &state_dir(cli),
                    )?;
                    let resolved = herdr_shepherd::roles::resolve(&cfg)?;
                    let acceptance = acceptance_file
                        .as_deref()
                        .map(cli_pr::read_text)
                        .transpose()?
                        .map(|text| {
                            serde_json::from_str::<Vec<String>>(&text).unwrap_or_else(|_| {
                                text.lines()
                                    .filter(|s| !s.trim().is_empty())
                                    .map(str::to_owned)
                                    .collect()
                            })
                        })
                        .unwrap_or_default();
                    let ticket = herdr_shepherd::ticket::intake::add(
                        &store,
                        &herdr_shepherd::ticket::intake::IntakeRequest {
                            id: id.clone(),
                            title: title.clone(),
                            body: cli_pr::read_text(body_file)?,
                            acceptance,
                            kind: kind.clone(),
                            base: base.clone(),
                        },
                        &resolved.shepherd.integration_branch,
                        &herdr_shepherd::launch::now(),
                    )?;
                    herdr_shepherd::hooks::emit(
                        &cfg,
                        "ticket.queued",
                        &serde_json::json!({"ticket":ticket.id}),
                    );
                    output::emit(cli.json, &ticket, || {
                        serde_json::to_string_pretty(&ticket).expect("serializable")
                    });
                }
                TicketCommands::Cancel { id } => {
                    let store = herdr_shepherd::store::Store::open(&state_dir(cli), &project)?;
                    let cfg = herdr_shepherd::trusted::committed_config(
                        &config::resolve_config_dir(cli.config_dir.as_deref()),
                        &project,
                        None,
                        &state_dir(cli),
                    )?;
                    let ticket = cli_ticket::cancel(herdr_client()?.as_ref(), &store, &cfg, id)?;
                    output::emit(cli.json, &ticket, || {
                        serde_json::to_string_pretty(&ticket).expect("serializable")
                    });
                }
                TicketCommands::Dispatch => {
                    let store = herdr_shepherd::store::Store::open(&state_dir(cli), &project)?;
                    let dir = config::resolve_config_dir(cli.config_dir.as_deref());
                    let cfg = herdr_shepherd::trusted::committed_config(
                        &dir,
                        &project,
                        None,
                        &state_dir(cli),
                    )?;
                    let result = cli_ticket::dispatch_with_backend(
                        herdr_client()?.as_ref(),
                        github_client()?.as_ref(),
                        &store,
                        &cfg,
                        &state_dir(cli),
                        &dir,
                    )?;
                    output::emit(cli.json, &result, || {
                        serde_json::to_string_pretty(&result).expect("serializable")
                    });
                }
                TicketCommands::Transition { id, to, reason } => {
                    let store = herdr_shepherd::store::Store::open(&state_dir(cli), &project)?;
                    let cfg = herdr_shepherd::trusted::committed_config(
                        &config::resolve_config_dir(cli.config_dir.as_deref()),
                        &project,
                        None,
                        &state_dir(cli),
                    )?;
                    let to = serde_json::from_value(serde_json::json!(to))
                        .map_err(|_| ShepherdError::Usage("unknown ticket state".into()))?;
                    let reason = reason
                        .as_ref()
                        .map(|r| serde_json::from_value(serde_json::json!(r)))
                        .transpose()
                        .map_err(|_| ShepherdError::Usage("unknown blocked reason".into()))?;
                    let ticket = cli_ticket::transition(&store, &cfg, id, to, reason)?;
                    output::emit(cli.json, &ticket, || {
                        serde_json::to_string_pretty(&ticket).expect("serializable")
                    });
                }
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
                    let cfg = herdr_shepherd::trusted::committed_config(
                        &config::resolve_config_dir(cli.config_dir.as_deref()),
                        &project,
                        None,
                        &state_dir(cli),
                    )?;
                    let ticket = herdr_shepherd::ticket::source::fetch(&cfg, id)?;
                    output::emit(cli.json, &ticket, || {
                        serde_json::to_string_pretty(&ticket).expect("ticket is serializable")
                    });
                }
            }
        }
        Commands::Status { project_root } => {
            let project = resolve_project_root(project_root.as_deref())?;
            let store = herdr_shepherd::store::Store::open(&state_dir(cli), &project)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs());
            let data = herdr_shepherd::github::protection::refresh_status(
                &store,
                github_client()?.as_ref(),
                now,
            )?;
            let warnings = herdr_shepherd::github::protection::status_warnings(&data);
            let value = serde_json::json!({"tickets":data.tickets, "protection_warnings":warnings});
            output::emit(cli.json, &value, || {
                let mut lines: Vec<String> = data
                    .tickets
                    .iter()
                    .map(|ticket| format!("{}: {:?}", ticket.id, ticket.state))
                    .collect();
                lines.extend(warnings);
                lines.join("\n")
            });
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

fn herdr_client() -> Result<Box<dyn Herdr>, ShepherdError> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("SHEPHERD_FAKE_HERDR") {
        let text = cli_pr::read_text(Path::new(&path))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ShepherdError::Usage(format!("invalid fake herdr: {e}")))?;
        let workspaces = serde_json::from_value(
            value
                .get("workspaces")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )
        .map_err(anyhow::Error::from)?;
        let h = herdr_shepherd::herdr::fake::FakeHerdr::new(workspaces);
        *h.agent_records.borrow_mut() = serde_json::from_value(
            value
                .get("agents")
                .cloned()
                .unwrap_or(serde_json::json!([])),
        )
        .map_err(anyhow::Error::from)?;
        return Ok(Box::new(h));
    }
    Ok(Box::new(CliHerdr::from_env()))
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
    if let Some(path) = std::env::var_os("HERDR_SHEPHERD_PROJECT_ROOT") {
        return Ok(path.into());
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

fn github_client() -> Result<Box<dyn herdr_shepherd::github::PrBackend>, ShepherdError> {
    #[cfg(debug_assertions)]
    if let Some(directory) = std::env::var_os("SHEPHERD_FAKE_GITHUB") {
        return Ok(Box::new(herdr_shepherd::github::fake::DiskBackend::open(
            Path::new(&directory),
        )?));
    }
    Ok(Box::new(herdr_shepherd::github::gh::GhBackend))
}

fn run_ticket_pr(
    command: &TicketCommands,
    project: &Path,
    cli: &Cli,
) -> Result<serde_json::Value, ShepherdError> {
    use herdr_shepherd::ticket::{pr, pr_body};
    let state = state_dir(cli);
    let store = herdr_shepherd::store::Store::open(&state, project)?;
    let id = match command {
        TicketCommands::Pr { id, .. }
        | TicketCommands::Comments { id, .. }
        | TicketCommands::Reply { id, .. }
        | TicketCommands::Revise { id, .. }
        | TicketCommands::Ack { id, .. } => id,
        _ => unreachable!("PR command"),
    };
    // Validate human-facing text before trusted config can query GitHub.
    let body = match command {
        TicketCommands::Reply { body_file, .. } | TicketCommands::Ack { body_file, .. } => {
            let text = cli_pr::read_text(body_file)?;
            pr::validate_body(&text)?;
            Some(text)
        }
        _ => None,
    };
    let data = store.read()?;
    let ticket = data
        .tickets
        .iter()
        .find(|t| t.id == *id)
        .ok_or_else(|| ShepherdError::Usage(format!("unknown ticket: {id}")))?;
    let content = if let TicketCommands::Pr {
        summary_file,
        changes_file,
        testing_file,
        ..
    } = command
    {
        let lines = |path: &Path| -> Result<Vec<String>, ShepherdError> {
            let text = cli_pr::read_text(path)?;
            Ok(
                serde_json::from_str::<Vec<String>>(&text).unwrap_or_else(|_| {
                    text.lines()
                        .filter(|s| !s.trim().is_empty())
                        .map(str::to_owned)
                        .collect()
                }),
            )
        };
        let summary = cli_pr::read_text(summary_file)?;
        let changes = lines(changes_file)?;
        let testing = lines(testing_file)?;
        pr_body::check_attribution(&pr_body::build_title(ticket))?;
        pr::validate_body(&pr_body::build_body(ticket, &summary, &changes, &testing))?;
        Some((summary, changes, testing))
    } else {
        None
    };
    let dir = config::resolve_config_dir(cli.config_dir.as_deref());
    let cfg = herdr_shepherd::trusted::committed_config(&dir, project, None, &state)?;
    if let TicketCommands::Revise {
        comments,
        task_file,
        ..
    } = command
    {
        let t = pr::revise(
            &store,
            id,
            comments,
            &cli_pr::read_text(task_file)?,
            &herdr_shepherd::launch::now(),
            herdr_shepherd::roles::resolve(&cfg)?
                .shepherd
                .max_revision_rounds,
        )?;
        return serde_json::to_value(t).map_err(|e| anyhow::Error::from(e).into());
    }
    let gh = github_client()?;
    if let TicketCommands::Comments { new, .. } = command {
        return serde_json::to_value(pr::comments(&store, gh.as_ref(), id, *new)?)
            .map_err(|e| anyhow::Error::from(e).into());
    }
    let path = ticket
        .worktree
        .as_deref()
        .ok_or_else(|| ShepherdError::Usage("ticket has no worktree".into()))?;
    let mut gate = cli_pr::gate(path, &cfg, Some(&ticket.base), Some(&ticket.branch), &state)?;
    gate.config_dir = dir;
    let t = match command {
        TicketCommands::Pr { .. } => {
            let (summary, changes, testing) = content.expect("read PR content");
            pr::open(
                &store,
                gh.as_ref(),
                &gate,
                &cfg,
                id,
                &summary,
                &changes,
                &testing,
                &herdr_shepherd::launch::now(),
            )?
        }
        TicketCommands::Reply { comment, .. } => pr::reply(
            &store,
            gh.as_ref(),
            &gate,
            id,
            *comment,
            body.as_deref().expect("read body"),
        )?,
        TicketCommands::Ack { comment, .. } => pr::ack(
            &store,
            gh.as_ref(),
            &gate,
            id,
            *comment,
            body.as_deref().expect("read body"),
        )?,
        _ => unreachable!("handled PR command"),
    };
    serde_json::to_value(t).map_err(|e| anyhow::Error::from(e).into())
}
