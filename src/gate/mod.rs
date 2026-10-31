pub mod args;
pub mod policy;
pub mod transport;
pub use args::{PushTarget, check_gh, check_push, parse_push_args};
pub use policy::BranchPolicy;

use crate::{error::ShepherdError, proc};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub struct Gate {
    pub policy: BranchPolicy,
    pub ticket_branch: String,
    pub worktree: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Gate {
    pub fn git_push(&self, args: &[&str]) -> Result<String, ShepherdError> {
        let mut target = parse_push_args(args);
        for refspec in &mut target.refspecs {
            if refspec == "HEAD" {
                *refspec = self.ticket_branch.clone();
            }
        }
        check_push(&target, &self.ticket_branch, &self.policy)?;
        if target.remote != "origin" {
            return Err(ShepherdError::PolicyDenied(
                "gated pushes require origin".into(),
            ));
        }
        transport::PushTransport::open(&self.state_dir, &self.worktree)?
            .push_ticket(&self.ticket_branch)
    }

    pub fn gh(&self, args: &[&str], cwd: &Path) -> Result<String, ShepherdError> {
        self.gh_input(args, cwd, None)
    }

    pub fn gh_with_input(
        &self,
        args: &[&str],
        cwd: &Path,
        input: &[u8],
    ) -> Result<String, ShepherdError> {
        self.gh_input(args, cwd, Some(input))
    }

    fn gh_input(
        &self,
        args: &[&str],
        cwd: &Path,
        input: Option<&[u8]>,
    ) -> Result<String, ShepherdError> {
        let _ = cwd;
        Self::execute(
            &mut self.github_command(Path::new("gh"), args)?,
            Duration::from_secs(60),
            input,
        )
    }

    /// Resolve GitHub identity only from the authorized project snapshot.
    pub fn github_repository(&self) -> Result<String, ShepherdError> {
        let trust = crate::trusted::load_snapshot(&self.state_dir, &self.worktree)?;
        let url = trust.origin_url.trim_end_matches('/');
        let path = [
            "https://github.com/",
            "http://github.com/",
            "ssh://git@github.com/",
            "git@github.com:",
        ]
        .iter()
        .find_map(|prefix| url.strip_prefix(prefix))
        .ok_or_else(|| {
            ShepherdError::PolicyDenied("trusted origin is not a GitHub repository".into())
        })?;
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() != 2
            || parts.iter().any(|part| {
                part.is_empty()
                    || matches!(*part, "." | "..")
                    || !part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            })
        {
            return Err(ShepherdError::PolicyDenied(
                "invalid snapshotted GitHub repository".into(),
            ));
        }
        Ok(path.into())
    }

    fn github_command(&self, executable: &Path, args: &[&str]) -> Result<Command, ShepherdError> {
        self.github_command_scoped(executable, args, true)
    }

    fn github_command_scoped(
        &self,
        executable: &Path,
        args: &[&str],
        restore_auth: bool,
    ) -> Result<Command, ShepherdError> {
        check_gh(args)?;
        let repository = self.github_repository()?;
        if args.iter().any(|arg| {
            *arg == "--repo"
                || arg.starts_with("--repo=")
                || arg.starts_with("-R")
                || *arg == "--hostname"
                || arg.starts_with("--hostname=")
        }) {
            return Err(ShepherdError::PolicyDenied(
                "repository and host overrides are forbidden".into(),
            ));
        }
        let mut scoped: Vec<String> = args.iter().map(|arg| (*arg).into()).collect();
        if args.first() == Some(&"api") {
            for arg in &mut scoped {
                if arg.starts_with("http://") || arg.starts_with("https://") {
                    return Err(ShepherdError::PolicyDenied(
                        "absolute API URLs are forbidden".into(),
                    ));
                }
                *arg = arg.replace("repos/{owner}/{repo}/", &format!("repos/{repository}/"));
                if arg.starts_with("repos/") && !arg.starts_with(&format!("repos/{repository}/")) {
                    return Err(ShepherdError::PolicyDenied(
                        "API repository differs from trust snapshot".into(),
                    ));
                }
            }
        } else if args.starts_with(&["repo", "view"]) {
            // gh repo view takes a positional repository, not -R.
            scoped.insert(2, repository.clone());
        } else if args.first() == Some(&"repo") {
            return Err(ShepherdError::PolicyDenied(
                "scoped repository forwarding supports only repo view".into(),
            ));
        } else {
            scoped.extend(["-R".into(), repository.clone()]);
        }
        // Policy checks still apply to the final command; scoping never bypasses them.
        check_gh(&scoped.iter().map(String::as_str).collect::<Vec<_>>())?;
        std::fs::create_dir_all(&self.state_dir).map_err(anyhow::Error::from)?;
        let mut command = Command::new(executable);
        command.current_dir(&self.state_dir).args(scoped);
        env::prepare_provider_command(&mut command, &self.state_dir, restore_auth)?;
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_")
                || matches!(key.to_str(), Some("GH_REPO" | "GH_HOST"))
            {
                command.env_remove(key);
            }
        }
        command
            .env("GH_REPO", repository)
            .env("GH_HOST", "github.com")
            .env("GIT_DIR", self.state_dir.join("github-no-repository.git"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        Ok(command)
    }

    fn execute(
        cmd: &mut Command,
        timeout: Duration,
        input: Option<&[u8]>,
    ) -> Result<String, ShepherdError> {
        let result = proc::capture(cmd, timeout, input)?;
        if !result.status.success() {
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "command failed ({}): {}",
                result.status,
                result.stderr.trim()
            )));
        }
        Ok(result.stdout)
    }
}
pub mod env;
pub mod shim;

impl Gate {
    /// Execute the real gh for a credential-stripped agent, preserving its env.
    pub fn exec_gh(&self, args: &[&str], path: &std::ffi::OsStr) -> Result<(), ShepherdError> {
        use std::os::unix::process::CommandExt;
        let error = self
            .github_command_scoped(Path::new("gh"), args, false)?
            .env("PATH", path)
            .exec();
        Err(ShepherdError::Runtime(error.into()))
    }
}
pub mod hook;

impl Gate {
    /// Use a configured executable while preserving the same gh policy checks.
    pub fn gh_using(
        &self,
        executable: &Path,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
    ) -> Result<String, ShepherdError> {
        let _ = cwd;
        Self::execute(&mut self.github_command(executable, args)?, timeout, None)
    }
}

impl Gate {
    /// The only delete escape hatch: cleanup must provide a bound merge proof.
    pub fn delete_remote_branch(
        &self,
        branch: &str,
        proof: &crate::cleanup::MergeProof,
    ) -> Result<(), ShepherdError> {
        if branch != self.ticket_branch || self.policy.is_protected(branch) {
            return Err(ShepherdError::PolicyDenied(
                "cleanup may delete only the unprotected ticket branch".into(),
            ));
        }
        proof.authorize(self, branch)?;
        proof.transport()?.delete_branch(branch, proof.tip())
    }
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/state/herdr/plugins/herdr-shepherd")
        })
}

#[cfg(test)]
mod github_scope_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn gh_uses_snapshot_in_neutral_directory() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let worktree = dir.path().join("repo");
        std::fs::create_dir(&worktree).unwrap();
        let store = crate::store::Store::open(&state, &worktree).unwrap();
        store
            .update(|data| {
                data.trust = Some(crate::store::ProjectTrust {
                    project_root: worktree.clone(),
                    common_git_dir: worktree.join(".git"),
                    origin_url: "https://github.com/trusted/project.git".into(),
                    push_url: "https://github.com/trusted/project.git".into(),
                    default_branch: "main".into(),
                    config_commit: None,
                    config_blob: None,
                    config_text: None,
                    trusted_at: "fixture".into(),
                    global_config: None,
                    object_format: "sha1".into(),
                });
                Ok(())
            })
            .unwrap();
        let script = dir.path().join("gh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" \"$GH_REPO\" \"$GIT_CONFIG_GLOBAL\" \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let gate = Gate {
            policy: BranchPolicy::new(&[], None, None).unwrap(),
            ticket_branch: "feat/test".into(),
            worktree,
            state_dir: state.clone(),
            config_dir: dir.path().join("cfg"),
        };
        let result = gate
            .gh_using(
                &script,
                &["pr", "view", "feat/test"],
                &gate.worktree,
                Duration::from_secs(2),
            )
            .unwrap();
        let rows: Vec<_> = result.lines().collect();
        assert!(Path::new(rows[0]).starts_with(&state));
        assert_eq!(
            &rows[1..],
            [
                "trusted/project",
                "/dev/null",
                "pr",
                "view",
                "feat/test",
                "-R",
                "trusted/project"
            ]
        );
        let result = gate
            .gh_using(
                &script,
                &["api", "repos/{owner}/{repo}/pulls/7/comments"],
                &gate.worktree,
                Duration::from_secs(2),
            )
            .unwrap();
        assert!(result.contains("repos/trusted/project/pulls/7/comments"));
        assert!(
            gate.gh_using(
                &script,
                &["api", "repos/other/project/pulls/7/comments"],
                &gate.worktree,
                Duration::from_secs(2)
            )
            .is_err()
        );
        for args in [
            vec!["pr", "view", "feat/test", "--repo", "evil/project"],
            vec!["pr", "view", "feat/test", "-Revil/project"],
            vec!["api", "https://evil.invalid/repos/trusted/project/pulls/7"],
            vec![
                "api",
                "repos/trusted/project/pulls/7",
                "--hostname",
                "evil.invalid",
            ],
        ] {
            assert!(
                gate.gh_using(&script, &args, &gate.worktree, Duration::from_secs(2))
                    .is_err(),
                "{args:?}"
            );
        }
    }
}
