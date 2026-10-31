use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::Config,
    error::ShepherdError,
    github::PrBackend,
    herdr::Herdr,
    metadata::{pr_token_value, token},
    notify::Notifier,
    proc,
};

#[derive(Default, Debug, Deserialize, Serialize)]
pub struct WatchState {
    pub last: HashMap<String, String>,
    /// Exact token names successfully written by this watcher.
    /// Older state files cannot prove ownership of any particular name.
    #[serde(default)]
    pub owned_tokens: HashMap<String, String>,
}

impl WatchState {
    pub fn load(state_dir: &Path) -> Result<Self, ShepherdError> {
        match fs::read(state_dir.join("watch-state.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|error| anyhow::Error::from(error).into())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(anyhow::Error::from(error).into()),
        }
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), ShepherdError> {
        fs::create_dir_all(state_dir).map_err(anyhow::Error::from)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(anyhow::Error::from)?
            .as_nanos();
        let temporary = state_dir.join(format!(".watch-state-{}-{nonce}.tmp", std::process::id()));
        let result = (|| -> anyhow::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec(self)?)?;
            file.sync_all()?;
            fs::rename(&temporary, state_dir.join("watch-state.json"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(ShepherdError::Runtime)
    }
}

#[derive(Default, Debug, Serialize)]
pub struct TickReport {
    pub polled: usize,
    pub supervised: usize,
    pub updated: usize,
    pub cleared: usize,
    pub errors: Vec<String>,
}

pub fn tick(
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    cfg: &Config,
    notifier: &Notifier<'_>,
    st: &mut WatchState,
) -> TickReport {
    let mut report = TickReport::default();
    if !cfg.enabled || !cfg.upkeep.pr_status {
        return report;
    }
    let key = match token(&cfg.upkeep.token_prefix, "pr") {
        Ok(key) => key,
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    let workspaces = match h.workspaces() {
        Ok(workspaces) => workspaces,
        Err(error) => {
            report.errors.push(error.to_string());
            return report;
        }
    };
    for ws in workspaces {
        let Some(worktree) = ws.worktree.as_ref().filter(|w| w.is_linked_worktree) else {
            continue;
        };
        let Some(path) = worktree.checkout_path.as_deref() else {
            continue;
        };
        let branch = match proc::run(
            Command::new("git")
                .current_dir(path)
                .args(["branch", "--show-current"]),
            Duration::from_secs(20),
        ) {
            Ok(Some(branch)) if !branch.trim().is_empty() => branch.trim().to_owned(),
            other => {
                report.errors.push(format!(
                    "{}: cannot read branch: {other:?}",
                    ws.workspace_id
                ));
                continue;
            }
        };
        report.polled += 1;
        match gh.view(path, &branch) {
            Ok(Some(status)) => {
                let value = pr_token_value(&status);
                let value_changed = st.last.get(&ws.workspace_id).is_none_or(|old| old != value);
                let owned = st.owned_tokens.get(&ws.workspace_id);
                if !value_changed && owned == Some(&key) {
                    continue;
                }
                let clear: Vec<String> = owned
                    .filter(|old| *old != &key)
                    .cloned()
                    .into_iter()
                    .collect();
                match h.report_workspace_tokens(
                    &ws.workspace_id,
                    &[(key.clone(), value.into())],
                    &clear,
                ) {
                    Ok(()) => {
                        st.last.insert(ws.workspace_id.clone(), value.into());
                        st.owned_tokens.insert(ws.workspace_id.clone(), key.clone());
                        report.updated += 1;
                        report.cleared += clear.len();
                        if value_changed
                            && matches!(
                                value,
                                "merged" | "closed" | "ci-failing" | "changes-requested"
                            )
                        {
                            notifier.send(&format!("{}: PR {value}", ws.label), &status.url);
                        }
                    }
                    Err(error) => report.errors.push(format!("{}: {error}", ws.workspace_id)),
                }
            }
            Ok(None) => {
                if let Some(owned) = st.owned_tokens.get(&ws.workspace_id) {
                    match h.report_workspace_tokens(
                        &ws.workspace_id,
                        &[],
                        std::slice::from_ref(owned),
                    ) {
                        Ok(()) => {
                            st.last.remove(&ws.workspace_id);
                            st.owned_tokens.remove(&ws.workspace_id);
                            report.cleared += 1;
                        }
                        Err(error) => report.errors.push(format!("{}: {error}", ws.workspace_id)),
                    }
                } else {
                    // A legacy value alone does not identify a token we own.
                    st.last.remove(&ws.workspace_id);
                }
            }
            Err(error) => report.errors.push(format!("{}: {error}", ws.workspace_id)),
        }
    }
    report
}

#[derive(Debug, Serialize)]
pub struct WatchOutcome {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<TickReport>,
}

fn already_running() -> WatchOutcome {
    WatchOutcome {
        status: "already_running".into(),
        pid: None,
        report: None,
    }
}

pub fn run(
    state_dir: &Path,
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    cfg: &Config,
    once: bool,
    interval: Option<u64>,
) -> Result<WatchOutcome, ShepherdError> {
    run_with_config_dir(state_dir, h, gh, cfg, once, interval, None)
}

pub fn run_with_config_dir(
    state_dir: &Path,
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    cfg: &Config,
    once: bool,
    interval: Option<u64>,
    config_dir: Option<&Path>,
) -> Result<WatchOutcome, ShepherdError> {
    if std::env::var_os("SHEPHERD_WATCH_PR_ONLY").is_some_and(|value| value == "1") {
        return run_pr_once(state_dir, h, gh, cfg);
    }
    let guard = if std::env::var_os("SHEPHERD_WATCH_DETACHED").is_some_and(|value| value == "1") {
        lock::acquire_child(state_dir)?
    } else {
        lock::acquire(state_dir)?
    };
    let Some(_guard) = guard else {
        return Ok(already_running());
    };
    let interval = interval
        .or_else(|| {
            cfg.raw
                .get("shepherd")?
                .get("pr_poll_seconds")?
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
        })
        .unwrap_or(180);
    if interval == 0 {
        return Err(ShepherdError::Usage(
            "watch interval must be positive".into(),
        ));
    }
    let mut worker = PrWorker::default();
    let mut next_pr = Instant::now();
    loop {
        let mut report = TickReport::default();
        if once {
            let mut state = WatchState::load(state_dir)?;
            report = tick(
                h,
                gh,
                cfg,
                &Notifier::new(h, cfg.upkeep.notifications),
                &mut state,
            );
            if let Err(error) = state.save(state_dir) {
                report.errors.push(error.to_string());
            }
        } else {
            if let Err(error) = worker.poll() {
                report.errors.push(error.to_string());
            }
            if cfg.enabled && cfg.upkeep.pr_status && Instant::now() >= next_pr && !worker.running()
            {
                let selected =
                    config_dir
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| match &cfg.source {
                            crate::config::ConfigSource::Global(path) => {
                                path.parent().map(Path::to_path_buf).unwrap_or_default()
                            }
                            _ => crate::config::resolve_config_dir(None),
                        });
                if let Err(error) = worker.start(state_dir, &selected) {
                    report.errors.push(error.to_string());
                }
                next_pr = Instant::now() + Duration::from_secs(interval);
            }
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let supervision = crate::supervise::sweep_projects(h, state_dir, cfg, now);
        report.supervised = supervision.observed;
        report.errors.extend(supervision.errors);
        for error in &report.errors {
            eprintln!("shepherd: watch: {error}");
        }
        if once {
            return Ok(WatchOutcome {
                status: "completed".into(),
                pid: None,
                report: Some(report),
            });
        }
        thread::sleep(Duration::from_secs(1));
    }
}

const PR_WORKER_SECONDS: u64 = 180;

#[derive(Default)]
struct PrWorker {
    child: Option<Child>,
    started: Option<Instant>,
}
impl PrWorker {
    fn running(&self) -> bool {
        self.child.is_some()
    }
    fn start(&mut self, state: &Path, config: &Path) -> Result<(), ShepherdError> {
        let child = Command::new(std::env::current_exe().map_err(anyhow::Error::from)?)
            .arg("--state-dir")
            .arg(state)
            .arg("--config-dir")
            .arg(config)
            .args(["watch", "--once"])
            .env("SHEPHERD_WATCH_PR_ONLY", "1")
            .env("SHEPHERD_WATCH_PARENT_PID", std::process::id().to_string())
            .env_remove("SHEPHERD_WATCH_DETACHED")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn()
            .map_err(anyhow::Error::from)?;
        self.child = Some(child);
        self.started = Some(Instant::now());
        Ok(())
    }
    fn poll(&mut self) -> Result<(), ShepherdError> {
        if self
            .started
            .is_some_and(|started| started.elapsed() >= Duration::from_secs(PR_WORKER_SECONDS))
        {
            self.stop();
            return Err(anyhow::anyhow!("PR poll exceeded its worker deadline").into());
        }
        if let Some(child) = self.child.as_mut()
            && let Some(status) = child.try_wait().map_err(anyhow::Error::from)?
        {
            self.child = None;
            self.started = None;
            if !status.success() {
                return Err(anyhow::anyhow!("PR poll worker exited with {status}").into());
            }
        }
        Ok(())
    }
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = child.kill();
            let _ = child.wait();
        }
        self.started = None;
    }
}
impl Drop for PrWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn parent_monitor() {
    let Some(parent) = std::env::var("SHEPHERD_WATCH_PARENT_PID")
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
    else {
        return;
    };
    // This worker has no authority to outlive its resident supervisor. Check
    // parent liveness even while a provider/CLI call is blocked, with a hard
    // wall-clock bound independent of the number of workspaces.
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(PR_WORKER_SECONDS);
        loop {
            thread::sleep(Duration::from_millis(250));
            if Instant::now() >= deadline
                || nix::sys::signal::kill(nix::unistd::Pid::from_raw(parent), None).is_err()
            {
                std::process::exit(0);
            }
        }
    });
}

pub fn run_pr_once(
    state_dir: &Path,
    h: &dyn Herdr,
    gh: &dyn PrBackend,
    cfg: &Config,
) -> Result<WatchOutcome, ShepherdError> {
    let Some(_guard) = lock::acquire_named(state_dir, "watch-pr.lock")? else {
        return Ok(already_running());
    };
    parent_monitor();
    let mut state = WatchState::load(state_dir)?;
    let report = tick(
        h,
        gh,
        cfg,
        &Notifier::new(h, cfg.upkeep.notifications),
        &mut state,
    );
    state.save(state_dir)?;
    for error in &report.errors {
        eprintln!("shepherd: PR poll: {error}");
    }
    Ok(WatchOutcome {
        status: "completed".into(),
        pid: None,
        report: Some(report),
    })
}

/// Serialize detached launches, then wait for the re-exec to own the poll lock.
/// This handshake avoids releasing the lock and reporting success before a
/// second launcher can see the watcher. Foreground starts use the same poll lock.
pub fn detach(
    state_dir: &Path,
    config_dir: Option<&Path>,
    interval: Option<u64>,
) -> Result<WatchOutcome, ShepherdError> {
    let Some(_launch) = lock::acquire_named(state_dir, "watch-launch.lock")? else {
        return Ok(already_running());
    };
    let Some(check) = lock::acquire(state_dir)? else {
        return Ok(already_running());
    };
    drop(check);
    if interval == Some(0) {
        return Err(ShepherdError::Usage(
            "watch interval must be positive".into(),
        ));
    }
    // Launchers serialize this rotation with watch-launch.lock. Keep one
    // previous log and start a fresh file for the new detached process.
    let log_path = state_dir.join("watch.log");
    match fs::metadata(&log_path) {
        Ok(metadata) if metadata.len() > 5 * 1024 * 1024 => {
            fs::rename(&log_path, state_dir.join("watch.log.1")).map_err(anyhow::Error::from)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(anyhow::Error::from(error).into()),
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .map_err(anyhow::Error::from)?;
    let mut command = Command::new("setsid");
    command
        .arg(std::env::current_exe().map_err(anyhow::Error::from)?)
        .arg("--state-dir")
        .arg(state_dir);
    if let Some(path) = config_dir {
        command.arg("--config-dir").arg(path);
    }
    command.arg("watch");
    if let Some(seconds) = interval {
        command.arg("--interval").arg(seconds.to_string());
    }
    let mut child = command
        .env("SHEPHERD_WATCH_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(anyhow::Error::from)?)
        .stderr(log)
        .spawn()
        .map_err(anyhow::Error::from)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if lock::acquire(state_dir)?.is_none() {
            return Ok(WatchOutcome {
                status: "started".into(),
                pid: Some(child.id()),
                report: None,
            });
        }
        if let Some(status) = child.try_wait().map_err(anyhow::Error::from)? {
            if status.success() {
                return Ok(already_running());
            }
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "watcher exited before acquiring lock: {status}"
            )));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ShepherdError::Runtime(anyhow::anyhow!(
                "watcher did not acquire lock within 5 seconds"
            )));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub mod lock;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        github::{Checks, PrState, PrStatus, ReviewDecision, fake::FakeBackend},
        herdr::{Workspace, WorktreeInfo, fake::FakeHerdr},
        notify::Notifier,
    };
    use std::process::Command;

    fn setup() -> (tempfile::TempDir, FakeHerdr, Config) {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(["init", "--quiet", "--initial-branch", "feat/test"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let h = FakeHerdr::new(vec![Workspace {
            workspace_id: "w1".into(),
            label: "task".into(),
            worktree: Some(WorktreeInfo {
                repo_key: "fixture".into(),
                repo_root: None,
                checkout_path: Some(dir.path().into()),
                is_linked_worktree: true,
            }),
        }]);
        let cfg = config::load(&dir.path().join("config"), None).unwrap();
        (dir, h, cfg)
    }

    fn status(state: PrState) -> PrStatus {
        PrStatus {
            number: 1,
            url: "https://github.com/example/repo/pull/1".into(),
            state,
            merged_at: Some("2026-10-31T00:00:00Z".into()),
            base: "main".into(),
            head: "feat/test".into(),
            head_oid: None,
            checks: Checks::Passing,
            review: ReviewDecision::None,
            comment_ids: vec![],
        }
    }

    #[test]
    fn tick_sets_and_notifies_on_merge() {
        let (_dir, h, cfg) = setup();
        let gh = FakeBackend::new(vec![
            Ok(Some(status(PrState::Open))),
            Ok(Some(status(PrState::Merged))),
        ]);
        let notifier = Notifier::new(&h, true);
        let mut st = WatchState::default();
        assert_eq!(tick(&h, &gh, &cfg, &notifier, &mut st).updated, 1);
        assert_eq!(st.last["w1"], "open");
        assert!(!h.calls.borrow().iter().any(|c| c.starts_with("notify ")));
        assert_eq!(tick(&h, &gh, &cfg, &notifier, &mut st).updated, 1);
        assert_eq!(st.last["w1"], "merged");
        assert_eq!(
            h.calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("notify "))
                .count(),
            1
        );
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c.contains("shepherd_pr") && c.contains("merged"))
        );
    }

    #[test]
    fn tick_keeps_last_on_github_error() {
        let (_dir, h, cfg) = setup();
        let gh = FakeBackend::new(vec![Err(ShepherdError::GithubUnavailable(
            "rate limit".into(),
        ))]);
        let mut st = WatchState::default();
        st.last.insert("w1".into(), "open".into());
        let report = tick(&h, &gh, &cfg, &Notifier::new(&h, true), &mut st);
        assert_eq!(st.last["w1"], "open");
        assert_eq!(report.errors.len(), 1);
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("workspace_tokens") || c.starts_with("notify "))
        );
    }

    #[test]
    fn tick_clears_when_no_pr() {
        let (_dir, h, cfg) = setup();
        let gh = FakeBackend::new(vec![Ok(Some(status(PrState::Open))), Ok(None)]);
        let mut st = WatchState::default();
        assert_eq!(
            tick(&h, &gh, &cfg, &Notifier::new(&h, true), &mut st).updated,
            1
        );
        assert_eq!(
            tick(&h, &gh, &cfg, &Notifier::new(&h, true), &mut st).cleared,
            1
        );
        assert!(!st.last.contains_key("w1"));
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|c| c == "workspace_tokens w1 [] [\"shepherd_pr\"]")
        );
    }

    #[test]
    fn state_roundtrip_and_failed_save_preserve_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut st = WatchState::default();
        st.last.insert("w1".into(), "open".into());
        st.owned_tokens.insert("w1".into(), "shepherd_pr".into());
        st.save(dir.path()).unwrap();
        let loaded = WatchState::load(dir.path()).unwrap();
        assert_eq!(loaded.last, st.last);
        assert_eq!(loaded.owned_tokens, st.owned_tokens);
        st.last.insert("w1".into(), "merged".into());
        st.save(dir.path()).unwrap();
        assert_eq!(WatchState::load(dir.path()).unwrap().last, st.last);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        let blocked = tempfile::tempdir().unwrap();
        fs::create_dir(blocked.path().join("watch-state.json")).unwrap();
        fs::write(
            blocked.path().join("watch-state.json/preserved.txt"),
            "keep",
        )
        .unwrap();
        assert!(matches!(
            st.save(blocked.path()),
            Err(ShepherdError::Runtime(_))
        ));
        assert_eq!(
            fs::read_to_string(blocked.path().join("watch-state.json/preserved.txt")).unwrap(),
            "keep"
        );
        assert_eq!(fs::read_dir(blocked.path()).unwrap().count(), 1);
        assert_eq!(st.last["w1"], "merged");
    }

    #[test]
    fn corrupt_state_is_runtime_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("watch-state.json"), "{broken").unwrap();
        assert!(matches!(
            WatchState::load(dir.path()),
            Err(ShepherdError::Runtime(_))
        ));
    }

    #[test]
    fn no_pr_never_clears_unowned_token() {
        let (_dir, h, cfg) = setup();
        let gh = FakeBackend::new(vec![Ok(None)]);
        let report = tick(
            &h,
            &gh,
            &cfg,
            &Notifier::new(&h, true),
            &mut WatchState::default(),
        );
        assert_eq!(report.cleared, 0);
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("workspace_tokens"))
        );
    }

    #[test]
    fn pr_status_disabled_noop() {
        let (_dir, h, mut cfg) = setup();
        cfg.upkeep.pr_status = false;
        let gh = FakeBackend::new(vec![Ok(Some(status(PrState::Open)))]);
        tick(
            &h,
            &gh,
            &cfg,
            &Notifier::new(&h, true),
            &mut WatchState::default(),
        );
        assert!(h.calls.borrow().is_empty());
        assert!(gh.calls.borrow().is_empty());
    }
    #[test]
    fn prefix_change_replaces_only_owned_token() {
        let (_dir, h, mut cfg) = setup();
        let gh = FakeBackend::new(vec![
            Ok(Some(status(PrState::Open))),
            Ok(Some(status(PrState::Open))),
        ]);
        let notifier = Notifier::new(&h, true);
        let mut st = WatchState::default();
        assert_eq!(tick(&h, &gh, &cfg, &notifier, &mut st).updated, 1);
        h.calls.borrow_mut().clear();
        cfg.upkeep.token_prefix = "custom_".into();
        let report = tick(&h, &gh, &cfg, &notifier, &mut st);
        assert_eq!(report.updated, 1);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call
                    == "workspace_tokens w1 [(\"custom_pr\", \"open\")] [\"shepherd_pr\"]"),
            "{:?}",
            h.calls.borrow()
        );
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.starts_with("notify "))
        );
    }

    #[test]
    fn legacy_state_never_infers_unknown_token_ownership() {
        let (_dir, h, mut cfg) = setup();
        cfg.upkeep.token_prefix = "custom_".into();
        let mut st: WatchState = serde_json::from_str(r#"{"last":{"w1":"open"}}"#).unwrap();
        let gh = FakeBackend::new(vec![Ok(Some(status(PrState::Open))), Ok(None)]);
        let notifier = Notifier::new(&h, true);
        assert_eq!(tick(&h, &gh, &cfg, &notifier, &mut st).updated, 1);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call == "workspace_tokens w1 [(\"custom_pr\", \"open\")] []"),
            "{:?}",
            h.calls.borrow()
        );
        h.calls.borrow_mut().clear();
        assert_eq!(tick(&h, &gh, &cfg, &notifier, &mut st).cleared, 1);
        assert!(
            h.calls
                .borrow()
                .iter()
                .any(|call| call == "workspace_tokens w1 [] [\"custom_pr\"]")
        );
        assert!(
            !h.calls
                .borrow()
                .iter()
                .any(|call| call.contains("shepherd_pr"))
        );
    }
    #[test]
    fn watch_once_includes_supervision_without_pr_status() {
        let (dir, h, mut cfg) = setup();
        cfg.upkeep.pr_status = false;
        let gh = FakeBackend::new(vec![]);
        let out = run(&dir.path().join("state"), &h, &gh, &cfg, true, Some(180)).unwrap();
        assert_eq!(out.report.unwrap().supervised, 0);
        assert!(gh.calls.borrow().is_empty());
    }
}
