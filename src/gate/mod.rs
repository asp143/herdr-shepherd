pub mod args;
pub mod policy;
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
                let branch = proc::run(
                    Command::new("git")
                        .current_dir(&self.worktree)
                        .args(["branch", "--show-current"]),
                    Duration::from_secs(20),
                )?
                .unwrap_or_default();
                if branch.trim() != self.ticket_branch {
                    return Err(ShepherdError::PolicyDenied(
                        "HEAD is not the ticket branch".into(),
                    ));
                }
                *refspec = self.ticket_branch.clone();
            }
        }
        check_push(&target, &self.ticket_branch, &self.policy)?;
        if target.remote != "origin" {
            return Err(ShepherdError::PolicyDenied(
                "gated pushes require origin".into(),
            ));
        }
        // Never execute the parsed command verbatim: destination-less refs let
        // remote push mappings replace the destination (and add force).
        let main = crate::trusted::main_checkout(&self.worktree)?;
        let url = trusted_push_url(&main)?;
        let state = &self.state_dir;
        let hooks = hook::prepare_configured(
            state,
            &std::env::current_exe().map_err(anyhow::Error::from)?,
            &self.config_dir,
        )?;
        let reference = format!("refs/heads/{}", self.ticket_branch);
        Self::execute(
            Command::new("git")
                .current_dir(&main)
                .args([
                    "-c",
                    "push.followTags=false",
                    "-c",
                    "remote.origin.mirror=false",
                    "-c",
                    "push.recurseSubmodules=no",
                ])
                .arg("-c")
                .arg(format!("remote.origin.push={reference}:{reference}"))
                .arg("-c")
                .arg(format!("core.hooksPath={}", hooks.display()))
                .arg("-c")
                .arg(format!("shepherd.ticketBranch={}", self.ticket_branch))
                .arg("push")
                .arg("--porcelain")
                .arg("--")
                .arg(url)
                .arg(format!("{reference}:{reference}")),
            Duration::from_secs(120),
            None,
        )
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
        check_gh(args)?;
        Self::execute(
            Command::new("gh").current_dir(cwd).args(args),
            Duration::from_secs(60),
            input,
        )
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
    pub fn exec_gh(args: &[&str], path: &std::ffi::OsStr) -> Result<(), ShepherdError> {
        use std::os::unix::process::CommandExt;
        check_gh(args)?;
        let error = Command::new("gh").args(args).env("PATH", path).exec();
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
        check_gh(args)?;
        Self::execute(
            Command::new(executable).current_dir(cwd).args(args),
            timeout,
            None,
        )
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
        let reference = format!("refs/heads/{branch}");
        let main = crate::trusted::main_checkout(&self.worktree)?;
        let url = trusted_push_url(&main)?;
        let remote = Self::execute(
            Command::new("git")
                .current_dir(&main)
                .args(["ls-remote", "--heads", &url, &reference]),
            Duration::from_secs(60),
            None,
        )?;
        if remote.trim().is_empty() {
            return Ok(());
        }
        let mut rows = remote.lines();
        let first = rows
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>();
        if first.as_slice() != [proof.tip(), reference.as_str()] || rows.next().is_some() {
            return Err(ShepherdError::PolicyDenied(
                "remote branch changed since merge proof".into(),
            ));
        }
        let mut command = Command::new("git");
        command.current_dir(&main).args([
            "-c",
            "push.followTags=false",
            "-c",
            "remote.origin.mirror=false",
            "-c",
            "push.recurseSubmodules=no",
            "push",
            &format!("--force-with-lease={reference}:{}", proof.tip()),
        ]);
        // A trusted MAIN receive-pack setting remains part of the transport;
        // using an explicit URL otherwise drops that remote-specific setting.
        if let Some(receivepack) = proc::run(
            Command::new("git").current_dir(&main).args([
                "config",
                "--get",
                "remote.origin.receivepack",
            ]),
            Duration::from_secs(20),
        )? {
            command.arg(format!("--receive-pack={}", receivepack.trim()));
        }
        command.arg("--").arg(&url).arg(format!(":{reference}"));
        Self::execute(&mut command, Duration::from_secs(120), None)?;
        Ok(())
    }
}

/// Keep push authorization independent of worktree-editable remote settings.
fn trusted_push_url(main: &Path) -> Result<String, ShepherdError> {
    let url = Gate::execute(
        Command::new("git")
            .current_dir(main)
            .args(["remote", "get-url", "--push", "--all", "origin"]),
        Duration::from_secs(20),
        None,
    )?;
    let urls: Vec<_> = url.lines().filter(|s| !s.is_empty()).collect();
    if urls.len() != 1 || urls[0].starts_with('-') {
        return Err(ShepherdError::PolicyDenied(
            "origin must have one explicit push URL".into(),
        ));
    }
    Ok(urls[0].into())
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
