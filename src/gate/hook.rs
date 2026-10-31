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
    let hooks = state_dir.join("hooks");
    fs::create_dir_all(&hooks).map_err(anyhow::Error::from)?;
    let hooks = hooks.canonicalize().map_err(anyhow::Error::from)?;
    let binary = shepherd_bin.canonicalize().map_err(anyhow::Error::from)?;
    let hook = hooks.join("pre-push");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nexec {} hook pre-push \"$@\"\n",
            shell_quote(&binary)
        ),
    )
    .map_err(anyhow::Error::from)?;
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700)).map_err(anyhow::Error::from)?;
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

fn zeros(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte == b'0')
}

pub fn pre_push(worktree: &Path, input: &str) -> Result<(), ShepherdError> {
    let branch = proc::capture(
        Command::new("git").current_dir(worktree).args([
            "config",
            "--worktree",
            "--get",
            "shepherd.ticketBranch",
        ]),
        Duration::from_secs(20),
        None,
    )?;
    if branch.status.code() == Some(1) && branch.stdout.trim().is_empty() {
        return Ok(());
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
    let root = git(worktree, &["rev-parse", "--show-toplevel"])?;
    let cfg = config::load(&config::resolve_config_dir(None), Some(Path::new(&root)))?;
    let extra = cfg
        .raw
        .get("shepherd")
        .and_then(|value| value.get("protected_branches"))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let default = git(
        worktree,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok();
    let policy = BranchPolicy::new(
        &extra,
        default
            .as_deref()
            .and_then(|value| value.strip_prefix("origin/")),
        None,
    )?;
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
