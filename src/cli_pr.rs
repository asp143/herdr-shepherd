use clap::Subcommand;
use herdr_shepherd::{
    config::{self, Config},
    error::ShepherdError,
    gate::{BranchPolicy, Gate},
    github::{PrBackend, gh::GhBackend},
    output, proc,
};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[derive(Subcommand)]
pub enum PrCommand {
    View {
        #[arg(long)]
        worktree: PathBuf,
    },
    Push {
        #[arg(long)]
        worktree: PathBuf,
    },
    Create {
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long)]
        base: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        body_file: PathBuf,
    },
    Reply {
        #[arg(long)]
        worktree: PathBuf,
        #[arg(long)]
        number: u64,
        #[arg(long)]
        comment: Option<u64>,
        #[arg(long)]
        body_file: PathBuf,
    },
}

pub fn git(worktree: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let result = proc::capture(
        Command::new("git").current_dir(worktree).args(args),
        Duration::from_secs(20),
        None,
    )?;
    if !result.status.success() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "git: {}",
            result.stderr.trim()
        )));
    }
    Ok(result.stdout.trim().to_owned())
}

pub fn gate(worktree: &Path, cfg: &Config, base: Option<&str>) -> Result<Gate, ShepherdError> {
    let branch = git(worktree, &["branch", "--show-current"])?;
    if branch.is_empty() {
        return Err(ShepherdError::Usage("worktree must be on a branch".into()));
    }
    let declared = git(
        worktree,
        &["config", "--worktree", "--get", "shepherd.ticketBranch"],
    )
    .unwrap_or_else(|_| branch.clone());
    if branch != declared {
        return Err(ShepherdError::PolicyDenied(
            "worktree is not on its declared ticket branch".into(),
        ));
    }
    let default = git(
        worktree,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok();
    let extra = match cfg
        .raw
        .get("shepherd")
        .and_then(|value| value.get("protected_branches"))
    {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .and_then(|values| {
                values
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                ShepherdError::Usage("protected_branches must be an array of globs".into())
            })?,
    };
    Ok(Gate {
        policy: BranchPolicy::new(
            &extra,
            default.as_deref().and_then(|s| s.strip_prefix("origin/")),
            base,
        )?,
        ticket_branch: declared,
        worktree: worktree.into(),
    })
}

pub fn read_text(path: &Path) -> Result<String, ShepherdError> {
    let actual = path.canonicalize().map_err(anyhow::Error::from)?;
    for part in actual.components() {
        let name = part.as_os_str().to_string_lossy();
        if name.starts_with(".env")
            || name == "Credentials"
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || matches!(name.as_ref(), "id_rsa" | "id_ed25519" | "credentials")
        {
            return Err(ShepherdError::Usage("secret files cannot be read".into()));
        }
    }
    std::fs::read_to_string(actual).map_err(|e| ShepherdError::Runtime(e.into()))
}

pub fn run(command: &PrCommand, config_dir: &Path, json: bool) -> Result<(), ShepherdError> {
    let worktree = match command {
        PrCommand::View { worktree }
        | PrCommand::Push { worktree }
        | PrCommand::Create { worktree, .. }
        | PrCommand::Reply { worktree, .. } => worktree,
    };
    let root = git(worktree, &["rev-parse", "--show-toplevel"])?;
    let cfg = config::load(config_dir, Some(Path::new(&root)))?;
    let base = match command {
        PrCommand::Create { base, .. } => Some(base.as_str()),
        _ => None,
    };
    let mut gate = gate(worktree, &cfg, base)?;
    let backend = GhBackend;
    let data = match command {
        PrCommand::View { .. } => {
            serde_json::to_value(backend.view(worktree, &gate.ticket_branch)?)
                .map_err(anyhow::Error::from)?
        }
        PrCommand::Push { .. } => {
            herdr_shepherd::gate::check_push(
                &herdr_shepherd::gate::parse_push_args(&["origin", &gate.ticket_branch]),
                &gate.ticket_branch,
                &gate.policy,
            )?;
            if let Some(status) = backend.view(worktree, &gate.ticket_branch)? {
                gate = self::gate(worktree, &cfg, Some(&status.base))?;
            }
            backend.push(&gate)?;
            serde_json::json!({"pushed":true, "branch":gate.ticket_branch})
        }
        PrCommand::Create {
            base,
            title,
            body_file,
            ..
        } => {
            herdr_shepherd::gate::check_push(
                &herdr_shepherd::gate::parse_push_args(&["origin", &gate.ticket_branch]),
                &gate.ticket_branch,
                &gate.policy,
            )?;
            let status = backend.create(&gate, base, title, &read_text(body_file)?)?;
            serde_json::to_value(status).map_err(anyhow::Error::from)?
        }
        PrCommand::Reply {
            number,
            comment,
            body_file,
            ..
        } => {
            backend.reply(&gate, *number, *comment, &read_text(body_file)?)?;
            serde_json::json!({"replied":true, "number":number, "comment":comment})
        }
    };
    output::emit(json, &data, || {
        serde_json::to_string_pretty(&data).expect("serializable")
    });
    Ok(())
}
