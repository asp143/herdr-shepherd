use super::Harness;
use crate::roles::Runner;

pub struct Omp;

impl Harness for Omp {
    fn kind(&self) -> &'static str {
        "omp"
    }
    fn launch_args(&self, runner: &Runner) -> Vec<String> {
        runner.args.clone()
    }
    fn subagent_hint(&self) -> &'static str {
        "use one batch task call"
    }
    fn provider_failure(&self, screen: &str) -> bool {
        super::provider_failure(screen)
    }
}

/// OMP's large-paste menu consumes the task's submit key when paste and Enter
/// arrive as separate events. Its supported per-run overlay disables that menu.
/// Append after user overlays (before a literal `--`), retaining every other
/// setting and argument. Never read or modify the user's OMP configuration.
pub(super) fn managed_args(
    mut args: Vec<String>,
    state_dir: &std::path::Path,
) -> Result<Vec<String>, crate::error::ShepherdError> {
    use anyhow::Context;
    use std::{fs, io::Write, os::unix::fs::OpenOptionsExt};

    fs::create_dir_all(state_dir).context("create managed OMP overlay directory")?;
    let state = state_dir
        .canonicalize()
        .context("resolve managed OMP overlay directory")?;
    let target = state.join("omp-paste-v1.yml");
    let path = target.to_str().ok_or_else(|| {
        crate::error::ShepherdError::Usage("managed OMP overlay path must be UTF-8".into())
    })?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = state.join(format!(".omp-paste-{}-{nonce}.tmp", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .context("create temporary managed OMP overlay")?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(b"paste:\n  largeMenuThreshold: 0\n")?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.context("install managed OMP paste overlay")?;
    let index = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(index..index, ["--config".into(), path.into()]);
    Ok(args)
}
