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
        Self::execute(
            Command::new("git")
                .current_dir(&self.worktree)
                .arg("push")
                .args(args),
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
