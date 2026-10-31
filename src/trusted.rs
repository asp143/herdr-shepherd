//! Resolve security-sensitive project inputs outside editable ticket worktrees.
use crate::{
    config::{self, Config},
    error::ShepherdError,
    proc,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

/// Shared fallback for helpers that are not invoked through the CLI parser.
pub fn default_state_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local/state/herdr/plugins/herdr-shepherd")
        })
}

fn git(cwd: &Path, args: &[&str]) -> Result<Option<String>, ShepherdError> {
    let out = proc::capture(
        Command::new("git")
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .args(args),
        Duration::from_secs(20),
        None,
    )?;
    Ok(out.status.success().then(|| out.stdout.trim().to_owned()))
}

pub fn main_checkout(worktree: &Path) -> Result<PathBuf, ShepherdError> {
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?
    .ok_or_else(|| ShepherdError::Usage("cannot resolve repository common directory".into()))?;
    let common = PathBuf::from(common)
        .canonicalize()
        .map_err(anyhow::Error::from)?;
    let trees = git(
        worktree,
        &[
            "--git-dir",
            common
                .to_str()
                .ok_or_else(|| ShepherdError::Usage("git path is not UTF-8".into()))?,
            "worktree",
            "list",
            "--porcelain",
            "-z",
        ],
    )?
    .ok_or_else(|| ShepherdError::Usage("cannot list repository worktrees".into()))?;
    let path = trees
        .split('\0')
        .find_map(|field| field.strip_prefix("worktree "))
        .ok_or_else(|| ShepherdError::Usage("main checkout not found".into()))?;
    PathBuf::from(path)
        .canonicalize()
        .map_err(|e| anyhow::Error::from(e).into())
}

fn fallback_default(main: &Path) -> Result<Option<String>, ShepherdError> {
    let remote = git(
        main,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )?
    .and_then(|s| s.strip_prefix("origin/").map(str::to_owned));
    let Some(remote) = remote else {
        return Ok(None);
    };
    // Remote-tracking refs are shared by every worktree, so reading this ref
    // from MAIN alone does not establish its provenance. The primary checkout
    // is the fallback anchor when the provider cannot confirm a default.
    Ok(git(main, &["symbolic-ref", "--short", "HEAD"])?
        .map(|local| if local == remote { remote } else { local }))
}

fn github_origin(origin: &str) -> bool {
    origin.starts_with("git@github.com:")
        || origin.starts_with("ssh://git@github.com/")
        || origin.starts_with("https://github.com/")
        || origin.starts_with("http://github.com/")
}

pub fn default_branch(worktree: &Path, state_dir: &Path) -> Result<Option<String>, ShepherdError> {
    let main = main_checkout(worktree)?;
    let origin = git(&main, &["remote", "get-url", "origin"])?.unwrap_or_default();
    if !github_origin(&origin) {
        return fallback_default(&main);
    }
    default_branch_using(worktree, state_dir, Path::new("gh"))
}

/// Explicit executable seam for the authoritative provider lookup.
pub fn default_branch_using(
    worktree: &Path,
    state_dir: &Path,
    gh: &Path,
) -> Result<Option<String>, ShepherdError> {
    let main = main_checkout(worktree)?;
    let origin = git(&main, &["remote", "get-url", "origin"])?.unwrap_or_default();
    let root = state_dir
        .join("projects")
        .join(crate::store::project_hash(&main));
    fs::create_dir_all(&root).map_err(anyhow::Error::from)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("default-branch.lock"))
        .map_err(anyhow::Error::from)?;
    lock.lock().map_err(anyhow::Error::from)?;
    let cache = root.join("default-branch.json");
    if let Ok(bytes) = fs::read(&cache) {
        let saved: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(anyhow::Error::from)?;
        if saved["origin"].as_str() == Some(&origin)
            && let Some(branch) = saved["branch"].as_str()
        {
            if git(&main, &["check-ref-format", "--branch", branch])?.is_none() {
                return Err(ShepherdError::Usage("invalid cached default branch".into()));
            }
            return Ok(Some(branch.into()));
        }
    }
    let out = proc::capture(
        Command::new(gh)
            .env_remove("GH_REPO")
            .env_remove("GH_HOST")
            .current_dir(&main)
            .args(["repo", "view", "--json", "defaultBranchRef"]),
        Duration::from_secs(20),
        None,
    );
    if let Ok(out) = out
        && out.status.success()
    {
        let value: serde_json::Value =
            serde_json::from_str(&out.stdout).map_err(anyhow::Error::from)?;
        if let Some(branch) = value
            .pointer("/defaultBranchRef/name")
            .and_then(serde_json::Value::as_str)
            && git(&main, &["check-ref-format", "--branch", branch])?.is_some()
        {
            let temporary = root.join("default-branch.json.tmp");
            fs::write(
                &temporary,
                serde_json::to_vec(&serde_json::json!({"origin":origin,"branch":branch}))
                    .expect("serializable"),
            )
            .map_err(anyhow::Error::from)?;
            fs::rename(temporary, cache).map_err(anyhow::Error::from)?;
            return Ok(Some(branch.into()));
        }
    }
    fallback_default(&main)
}

/// Project settings and hooks are pinned to the trusted MAIN base, never a
/// ticket's mutable checkout or a caller-selected feature branch.
pub fn committed_config(
    config_dir: &Path,
    worktree: &Path,
    _base: Option<&str>,
    state_dir: &Path,
) -> Result<Config, ShepherdError> {
    if git(worktree, &["rev-parse", "--git-dir"])?.is_none() {
        // A plain non-repository directory may still invoke global lifecycle
        // hooks. An existing but malformed repository marker is never ignored.
        if worktree
            .ancestors()
            .any(|path| fs::symlink_metadata(path.join(".git")).is_ok())
        {
            return Err(ShepherdError::Usage(
                "cannot resolve repository for trusted configuration".into(),
            ));
        }
        return config::load_committed(config_dir, &worktree.join(".shepherd.toml"), None);
    }
    let main = main_checkout(worktree)?;
    let default = default_branch(&main, state_dir)?;
    let base = match default {
        Some(branch) => {
            let local = format!("refs/heads/{branch}");
            if git(&main, &["rev-parse", "--verify", &local])?.is_some() {
                local
            } else {
                format!("refs/remotes/origin/{branch}")
            }
        }
        None => "HEAD".into(),
    };
    let oid = git(
        &main,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base}^{{commit}}"),
        ],
    )?
    .ok_or_else(|| ShepherdError::Usage("trusted base commit is missing".into()))?;
    let entry = git(&main, &["ls-tree", &oid, "--", ".shepherd.toml"])?
        .ok_or_else(|| ShepherdError::Usage("cannot inspect trusted project config".into()))?;
    let contents = if entry.is_empty() {
        None
    } else {
        if !entry.starts_with("100644 blob ") && !entry.starts_with("100755 blob ") {
            return Err(ShepherdError::Usage(
                "trusted project config must be a regular file".into(),
            ));
        }
        Some(
            git(&main, &["show", &format!("{oid}:.shepherd.toml")])?
                .ok_or_else(|| ShepherdError::Usage("cannot read trusted project config".into()))?,
        )
    };
    config::load_committed(
        config_dir,
        &main.join(".shepherd.toml"),
        contents.as_deref(),
    )
}
