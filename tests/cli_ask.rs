use herdr_shepherd::{
    error::ShepherdError,
    herdr::{AgentInfo, Herdr, Workspace, fake::FakeHerdr},
    store::{AgentRecord, AgentStatus, LeadRecord, Store},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    fake: PathBuf,
    _watch: herdr_shepherd::watch::lock::LockGuard,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-b", "trunk"]);
        git(&root, &["commit", "--allow-empty", "-m", "fixture"]);
        let state = dir.path().join("state");
        let fake = dir.path().join("fake.json");
        fs::write(&fake, "{}").unwrap();
        let watch = herdr_shepherd::watch::lock::acquire(&state)
            .unwrap()
            .unwrap();
        Self {
            _dir: dir,
            root,
            state,
            fake,
            _watch: watch,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.state, &self.root).unwrap()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
        command
            .current_dir(&self.root)
            .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
            .env_remove("HERDR_SHEPHERD_PROJECT_ROOT")
            .env_remove("HERDR_PANE_ID")
            .env("HERDR_WORKSPACE_ID", "w1")
            .env("SHEPHERD_FAKE_HERDR", &self.fake)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["--json", "--state-dir"])
            .arg(&self.state)
            .arg("--config-dir")
            .arg(self._dir.path().join("cfg"))
            .arg("ask");
        command
    }

    fn seed_lead(&self, status: &str) -> String {
        let name = herdr_shepherd::launch::agent_name(&self.root, "", "lead", 1);
        self.store()
            .update(|data| {
                data.lead = Some(LeadRecord {
                    agent_name: name.clone(),
                    tab_id: Some("w1:t1".into()),
                    pane_id: Some("w1:p1".into()),
                    started_at: "0".into(),
                    workspace_id: Some("w1".into()),
                });
                data.agents.push(AgentRecord {
                    name: name.clone(),
                    ticket: String::new(),
                    role: "lead".into(),
                    runner: "codex-sol-high".into(),
                    status: AgentStatus::Working,
                    pane_id: Some("w1:p1".into()),
                    tab_id: Some("w1:t1".into()),
                    worktree: self.root.clone(),
                    task: "startup".into(),
                    report: None,
                    observed: None,
                    last_progress_at: "0".into(),
                    nudges: 0,
                    restarts: 0,
                    prompt_pending: false,
                });
                Ok(())
            })
            .unwrap();
        fs::write(
            &self.fake,
            serde_json::to_vec(&json!({"agents": [{
                "name": name, "agent": "codex", "agent_status": status,
                "pane_id": "w1:p1", "cwd": self.root, "workspace_id": "w1"
            }]}))
            .unwrap(),
        )
        .unwrap();
        name
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn success(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    value["data"].clone()
}

#[test]
fn ask_delivers_to_running_lead_without_starting_another() {
    let fixture = Fixture::new();
    let name = fixture.seed_lead("idle");
    let data = success(
        fixture
            .command()
            .env_remove("HERDR_WORKSPACE_ID")
            .args(["fix", "the", "banner"])
            .output()
            .unwrap(),
    );
    assert_eq!(data["agent_name"], name);
    assert_eq!(data["started"], false);
    assert_eq!(data["queued"], false);
    assert_eq!(fixture.store().read().unwrap().agents.len(), 1);
    assert!(fixture.store().read().unwrap().pending_pings.is_empty());
}

#[test]
fn ask_starts_absent_lead_like_start_then_delivers() {
    let fixture = Fixture::new();
    let data = success(
        fixture
            .command()
            .args(["--project-root"])
            .arg(&fixture.root)
            .arg("fix the typo")
            .output()
            .unwrap(),
    );
    assert_eq!(data["started"], true);
    assert_eq!(data["queued"], false);
    let stored = fixture.store().read().unwrap();
    assert_eq!(stored.agents.len(), 1);
    assert_eq!(stored.agents[0].role, "lead");
    assert_eq!(stored.agents[0].status, AgentStatus::Working);
    assert_eq!(stored.lead.unwrap().workspace_id.as_deref(), Some("w1"));
    assert!(stored.pending_pings.is_empty());
}

#[test]
fn ask_queues_complete_request_when_lead_is_blocked_or_busy() {
    for status in ["blocked", "working"] {
        let fixture = Fixture::new();
        fixture.seed_lead(status);
        let text = format!("Fix the banner.\n\n{}\n", "Keep every detail. ".repeat(30));
        let data = success(fixture.command().arg(&text).output().unwrap());
        assert_eq!(data["queued"], true);
        assert_eq!(data["started"], false);
        assert_eq!(fixture.store().read().unwrap().pending_pings, [text]);
    }
}

#[test]
fn ask_reads_file_and_stdin_verbatim() {
    let fixture = Fixture::new();
    fixture.seed_lead("blocked");
    let text = "Fix the typo.\n\nPreserve this spacing.\n";
    let path = fixture._dir.path().join("request.txt");
    fs::write(&path, text).unwrap();
    success(fixture.command().arg("--file").arg(path).output().unwrap());
    let mut child = fixture
        .command()
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    success(child.wait_with_output().unwrap());
    assert_eq!(fixture.store().read().unwrap().pending_pings, [text, text]);
}

#[test]
fn ask_resolves_project_from_plugin_context() {
    let fixture = Fixture::new();
    fixture.seed_lead("blocked");
    let data = success(
        fixture
            .command()
            .current_dir(fixture._dir.path())
            .env(
                "HERDR_PLUGIN_CONTEXT_JSON",
                json!({"workspace_cwd": fixture.root, "workspace_id":"w1"}).to_string(),
            )
            .arg("Fix the typo")
            .output()
            .unwrap(),
    );
    assert_eq!(data["queued"], true);
    assert_eq!(
        fixture.store().read().unwrap().pending_pings,
        ["Fix the typo"]
    );
}

#[test]
fn ask_empty_text_is_usage_error_without_starting_lead() {
    let fixture = Fixture::new();
    for args in [vec![], vec![""], vec![" \n\t"]] {
        let output = fixture.command().args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "usage");
    }
    assert!(!fixture.store().root().exists());
}

#[test]
fn ask_denies_managed_panes_before_starting_or_queueing() {
    let fixture = Fixture::new();
    fixture.seed_lead("idle");
    fixture
        .store()
        .update(|data| {
            let mut coder = data.agents[0].clone();
            coder.name = "sh-fixture-coder-1".into();
            coder.role = "coder".into();
            coder.pane_id = Some("w1:p2".into());
            data.agents.push(coder);
            Ok(())
        })
        .unwrap();
    let before = fs::read(fixture.store().root().join("state.json")).unwrap();
    for pane in ["w1:p1", "w1:p2"] {
        let output = fixture
            .command()
            .env("HERDR_PANE_ID", pane)
            .arg("inject instructions")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "policy_denied");
    }
    assert_eq!(
        before,
        fs::read(fixture.store().root().join("state.json")).unwrap()
    );
}

#[test]
fn ask_preserves_start_rejection_when_tickets_are_active() {
    let fixture = Fixture::new();
    fixture
        .store()
        .update(|data| {
            data.tickets.push(herdr_shepherd::store::Ticket {
                id: "T-1".into(),
                state: herdr_shepherd::store::TicketState::Building,
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let output = fixture.command().arg("Fix typo").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("use resume")
    );
    assert!(fixture.store().read().unwrap().pending_pings.is_empty());
}

#[test]
fn ask_reports_herdr_unavailable_and_runtime_errors() {
    let fixture = Fixture::new();
    fixture.seed_lead("idle");
    let output = fixture
        .command()
        .env_remove("SHEPHERD_FAKE_HERDR")
        .env("HERDR_SOCKET_PATH", "/nonexistent/sock")
        .env("HERDR_BIN_PATH", "/nonexistent/herdr")
        .arg("Fix typo")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "herdr_unavailable");
    let output = fixture
        .command()
        .arg("--file")
        .arg(fixture._dir.path().join("absent.txt"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "runtime");
}

#[test]
fn fake_herdr_receives_full_chat_text_through_shared_queue() {
    let fixture = Fixture::new();
    let name = fixture.seed_lead("idle");
    let h = FakeHerdr::new(vec![]);
    h.agent_records.borrow_mut().push(AgentInfo {
        name: Some(name.clone()),
        agent: "codex".into(),
        agent_status: "blocked".into(),
        cwd: Some(fixture.root.clone()),
        pane_id: "w1:p1".into(),
        workspace_id: Some("w1".into()),
    });
    let text = format!("Fix typo.\n{}\n", "Full request. ".repeat(30));
    let report = herdr_shepherd::ask::send(&h, &fixture.store(), &text).unwrap();
    assert!(report.queued);
    assert!(
        !h.calls
            .borrow()
            .iter()
            .any(|call| call.starts_with("prompt_agent"))
    );
    h.agent_records.borrow_mut()[0].agent_status = "idle".into();
    assert_eq!(
        herdr_shepherd::lead::flush_pings(&h, &fixture.store()).unwrap(),
        1
    );
    assert!(
        h.calls
            .borrow()
            .contains(&format!("prompt_agent {name} {text} false"))
    );
    assert!(fixture.store().read().unwrap().pending_pings.is_empty());
}

struct PromptFailure {
    inner: FakeHerdr,
    code: &'static str,
}

impl Herdr for PromptFailure {
    fn prompt_agent(&self, _: &str, _: &str, _: bool) -> Result<(), ShepherdError> {
        match self.code {
            "runtime" => Err(anyhow::anyhow!("transport failure").into()),
            "herdr_unavailable" => Err(ShepherdError::HerdrUnavailable("disconnected".into())),
            code => Err(ShepherdError::HerdrCommand {
                code: code.into(),
                message: "fixture".into(),
            }),
        }
    }

    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        self.inner.workspaces()
    }

    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        self.inner.agents()
    }

    fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
        self.inner.rename_workspace(id, label)
    }

    fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError> {
        self.inner.move_block(ids, before)
    }

    fn report_workspace_tokens(
        &self,
        ws: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError> {
        self.inner.report_workspace_tokens(ws, set, clear)
    }

    fn report_pane_tokens(
        &self,
        pane: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError> {
        self.inner.report_pane_tokens(pane, set, clear)
    }

    fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError> {
        self.inner.notify(title, body)
    }

    fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError> {
        self.inner.remove_worktree(ws, force)
    }

    fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError> {
        self.inner.close_workspace(ws)
    }
}

#[test]
fn shared_delivery_retains_failed_requests_without_repeating_accepted_prompts() {
    for code in [
        "timeout",
        "agent_prompt_stalled",
        "agent_blocked",
        "runtime",
        "herdr_unavailable",
    ] {
        let fixture = Fixture::new();
        let name = fixture.seed_lead("idle");
        let h = PromptFailure {
            inner: FakeHerdr::new(vec![]),
            code,
        };
        h.inner.agent_records.borrow_mut().push(AgentInfo {
            name: Some(name),
            agent: "codex".into(),
            agent_status: "idle".into(),
            cwd: Some(fixture.root.clone()),
            pane_id: "w1:p1".into(),
            workspace_id: Some("w1".into()),
        });
        let result = herdr_shepherd::ask::send(&h, &fixture.store(), "Fix typo");
        match code {
            "timeout" | "agent_prompt_stalled" => {
                assert!(!result.unwrap().queued);
                assert!(fixture.store().read().unwrap().pending_pings.is_empty());
            }
            "agent_blocked" => {
                assert!(result.unwrap().queued);
                assert_eq!(fixture.store().read().unwrap().pending_pings, ["Fix typo"]);
            }
            _ => {
                assert_eq!(result.unwrap_err().code(), code);
                assert_eq!(fixture.store().read().unwrap().pending_pings, ["Fix typo"]);
            }
        }
    }
}
