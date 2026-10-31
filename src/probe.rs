use std::{path::Path, process::Command, time::Duration};

use crate::{
    gate::{BranchPolicy, Gate},
    proc,
};

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const SIZE_TIMEOUT: Duration = Duration::from_secs(120);

// Scans share one probe across scoped workers; implementations must permit
// concurrent independent reads.
pub trait Probe: Sync {
    fn branch(&self, p: &Path) -> Option<String>;
    fn dirty(&self, p: &Path) -> bool;
    fn pr_state(&self, p: &Path, branch: &str) -> Option<String>;
    fn size(&self, p: &Path) -> Option<String>;
}

pub struct SystemProbe;

fn output(cmd: &mut Command, timeout: Duration) -> Option<String> {
    proc::run(cmd, timeout)
        .ok()
        .flatten()
        .map(|s| s.trim().to_owned())
}

fn pr_state_with(bin: &Path, p: &Path, branch: &str, timeout: Duration) -> Option<String> {
    let gate = Gate {
        policy: BranchPolicy::new(&[], None, None).ok()?,
        ticket_branch: branch.into(),
        worktree: p.into(),
    };
    gate.gh_using(
        bin,
        &["pr", "view", branch, "--json", "state", "-q", ".state"],
        p,
        timeout,
    )
    .ok()
    .map(|value| value.trim().to_owned())
}

impl Probe for SystemProbe {
    fn branch(&self, p: &Path) -> Option<String> {
        output(
            Command::new("git")
                .current_dir(p)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .args(["-c", "core.fsmonitor=false", "branch", "--show-current"]),
            PROBE_TIMEOUT,
        )
        .filter(|s| !s.is_empty())
    }

    fn dirty(&self, p: &Path) -> bool {
        output(
            Command::new("git")
                .current_dir(p)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .args(["-c", "core.fsmonitor=false", "status", "--porcelain"]),
            PROBE_TIMEOUT,
        )
        .is_some_and(|s| !s.is_empty())
    }

    fn pr_state(&self, p: &Path, branch: &str) -> Option<String> {
        pr_state_with(Path::new("gh"), p, branch, PROBE_TIMEOUT)
    }

    fn size(&self, p: &Path) -> Option<String> {
        output(Command::new("du").args(["-sh", "--"]).arg(p), SIZE_TIMEOUT)?
            .split_whitespace()
            .next()
            .map(str::to_owned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Instant};

    fn script(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("fake-gh");
        fs::write(&p, text).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        p
    }

    #[test]
    fn missing_gh_returns_no_pr_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            pr_state_with(
                &dir.path().join("missing-gh"),
                dir.path(),
                "feat/example",
                PROBE_TIMEOUT
            ),
            None
        );
    }

    #[test]
    fn non_github_remote_returns_no_pr_state() {
        let dir = tempfile::tempdir().unwrap();
        let gh = script(
            dir.path(),
            "#!/bin/sh\nprintf '%s\\n' 'none of the git remotes point to a known GitHub host' >&2\nexit 1\n",
        );
        assert_eq!(
            pr_state_with(&gh, dir.path(), "feat/example", PROBE_TIMEOUT),
            None
        );
    }

    #[test]
    fn slow_gh_times_out_and_returns_no_pr_state() {
        let dir = tempfile::tempdir().unwrap();
        let gh = script(dir.path(), "#!/bin/sh\nexec sleep 1\n");
        let start = Instant::now();
        assert_eq!(
            pr_state_with(&gh, dir.path(), "feat/example", Duration::from_millis(40)),
            None
        );
        assert!(start.elapsed() < Duration::from_millis(500));
        assert_eq!(PROBE_TIMEOUT, Duration::from_secs(20));
    }
    #[test]
    fn never_writes_real_index_or_runs_fsmonitor() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let result = Command::new("git")
                .current_dir(dir.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        git(&["config", "user.name", "Fixture"]);
        fs::write(dir.path().join("tracked"), "fixture\n").unwrap();
        git(&["add", "tracked"]);
        git(&["commit", "-m", "fixture"]);
        let marker = dir.path().join("fsmonitor-ran");
        let hook = script(
            dir.path(),
            &format!(
                "#!/bin/sh\nprintf marker > '{}'\nprintf 'token\\0'\n",
                marker.display()
            ),
        );
        git(&["config", "core.fsmonitor", hook.to_str().unwrap()]);
        let index = dir.path().join(".git/index");
        let before = (
            fs::read(&index).unwrap(),
            fs::metadata(&index).unwrap().modified().unwrap(),
        );
        assert_eq!(SystemProbe.branch(dir.path()).as_deref(), Some("main"));
        SystemProbe.dirty(dir.path());
        assert!(
            !marker.exists(),
            "read-only probes must not run configured fsmonitor commands"
        );
        assert_eq!(
            (
                fs::read(&index).unwrap(),
                fs::metadata(&index).unwrap().modified().unwrap()
            ),
            before
        );
    }
}
