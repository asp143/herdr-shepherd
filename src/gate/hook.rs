use crate::{
    config,
    error::ShepherdError,
    gate::{BranchPolicy, shim::shell_quote},
    proc,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, time::Duration};

fn git(worktree: &Path, args: &[&str]) -> Result<String, ShepherdError> {
    let output = proc::capture(
        Command::new("git").current_dir(worktree).args(args),
        Duration::from_secs(20),
        None,
    )?;
    if !output.status.success() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "git failed: {}",
            output.stderr.trim()
        )));
    }
    Ok(output.stdout.trim().into())
}

pub fn install(
    worktree: &Path,
    ticket_branch: &str,
    state_dir: &Path,
    shepherd_bin: &Path,
) -> Result<(), ShepherdError> {
    install_configured(
        worktree,
        ticket_branch,
        state_dir,
        shepherd_bin,
        &config::resolve_config_dir(None),
    )
}

pub fn install_configured(
    worktree: &Path,
    ticket_branch: &str,
    state_dir: &Path,
    shepherd_bin: &Path,
    config_dir: &Path,
) -> Result<(), ShepherdError> {
    let git_dir = git(worktree, &["rev-parse", "--absolute-git-dir"])?;
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    if git_dir == common {
        return Err(ShepherdError::Usage(
            "install hooks only in a linked worktree".into(),
        ));
    }
    git(worktree, &["check-ref-format", "--branch", ticket_branch])?;
    let hooks = prepare_configured(state_dir, shepherd_bin, config_dir)?;
    git(worktree, &["config", "extensions.worktreeConfig", "true"])?;
    git(
        worktree,
        &[
            "config",
            "--worktree",
            "core.hooksPath",
            hooks
                .to_str()
                .ok_or_else(|| ShepherdError::Usage("hook path is not UTF-8".into()))?,
        ],
    )?;
    git(
        worktree,
        &[
            "config",
            "--worktree",
            "shepherd.ticketBranch",
            ticket_branch,
        ],
    )?;
    Ok(())
}

/// Prepare a forced hook independently of mutable worktree configuration.
pub fn prepare(state_dir: &Path, shepherd_bin: &Path) -> Result<std::path::PathBuf, ShepherdError> {
    prepare_configured(state_dir, shepherd_bin, &config::resolve_config_dir(None))
}

pub fn prepare_configured(
    state_dir: &Path,
    shepherd_bin: &Path,
    config_dir: &Path,
) -> Result<std::path::PathBuf, ShepherdError> {
    let hooks = state_dir.join("hooks");
    fs::create_dir_all(&hooks).map_err(anyhow::Error::from)?;
    let hooks = hooks.canonicalize().map_err(anyhow::Error::from)?;
    let binary = shepherd_bin.canonicalize().map_err(anyhow::Error::from)?;
    let hook = hooks.join("pre-push");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nexec {} --state-dir {} --config-dir {} hook pre-push \"$@\"\n",
            shell_quote(&binary),
            shell_quote(&state_dir.canonicalize().map_err(anyhow::Error::from)?),
            shell_quote(&std::path::absolute(config_dir).map_err(anyhow::Error::from)?)
        ),
    )
    .map_err(anyhow::Error::from)?;
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).map_err(anyhow::Error::from)?;
    Ok(hooks)
}

fn zeros(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte == b'0')
}

pub fn pre_push(worktree: &Path, input: &str) -> Result<(), ShepherdError> {
    pre_push_configured(
        worktree,
        input,
        &super::state_dir(),
        &config::resolve_config_dir(None),
    )
}

pub fn pre_push_configured(
    worktree: &Path,
    input: &str,
    state: &Path,
    config_dir: &Path,
) -> Result<(), ShepherdError> {
    let branch = proc::capture(
        Command::new("git").current_dir(worktree).args([
            "config",
            "--get",
            "shepherd.ticketBranch",
        ]),
        Duration::from_secs(20),
        None,
    )?;
    if branch.status.code() == Some(1) && branch.stdout.trim().is_empty() {
        return Err(ShepherdError::PolicyDenied(
            "shepherd hook requires a ticket branch marker".into(),
        ));
    }
    if !branch.status.success() {
        return Err(ShepherdError::Runtime(anyhow::anyhow!(
            "cannot read ticket branch: {}",
            branch.stderr.trim()
        )));
    }
    let branch = branch.stdout.trim();
    if branch.is_empty() {
        return Err(ShepherdError::PolicyDenied("empty ticket branch".into()));
    }
    let cfg = crate::trusted::committed_config(config_dir, worktree, None, state)?;
    let extra = super::policy::extra_patterns(&cfg)?;
    let default = crate::trusted::default_branch(worktree, state)?;
    let policy = BranchPolicy::new(&extra, default.as_deref(), None)?;
    for line in input.lines() {
        let refs: Vec<_> = line.split_whitespace().collect();
        if refs.len() != 4 {
            return Err(ShepherdError::PolicyDenied(
                "malformed pre-push input".into(),
            ));
        }
        let [_, local_sha, remote_ref, remote_sha] = refs.as_slice() else {
            unreachable!()
        };
        if *remote_ref != format!("refs/heads/{branch}") || policy.is_protected(branch) {
            return Err(ShepherdError::PolicyDenied(
                "destination must be the unprotected ticket branch".into(),
            ));
        }
        if zeros(local_sha) {
            return Err(ShepherdError::PolicyDenied(
                "deleting branches is forbidden".into(),
            ));
        }
        if !zeros(remote_sha)
            && git(
                worktree,
                &["merge-base", "--is-ancestor", remote_sha, local_sha],
            )
            .is_err()
        {
            return Err(ShepherdError::PolicyDenied(
                "non-fast-forward push is forbidden".into(),
            ));
        }
    }
    Ok(())
}
