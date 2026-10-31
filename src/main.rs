use std::path::PathBuf;

use clap::{Parser, Subcommand, error::ErrorKind};
use herdr_shepherd::{config, error::ShepherdError, output};

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
            let current = std::env::current_dir().map_err(anyhow::Error::from)?;
            let cfg = config::load(
                &config::resolve_config_dir(cli.config_dir.as_deref()),
                Some(project_root.as_deref().unwrap_or(&current)),
            )?;
            output::emit(cli.json, &cfg, || {
                serde_json::to_string_pretty(&cfg).expect("config is serializable")
            });
        }
    }
    Ok(())
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
