#![cfg(debug_assertions)]

use herdr_shepherd::store::{AgentRecord, AgentStatus, Store, TicketState};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn isolated<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_STATE_HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = isolated(&mut Command::new("git"), &root.join("empty-home"))
        .current_dir(root)
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
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

fn success(out: Output) -> Value {
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice::<Value>(&out.stdout).unwrap()["data"].clone()
}

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::create_dir(dir.path().join("home")).unwrap();
        git(&root, &["init", "-b", "main"]);
        git(&root, &["commit", "--allow-empty", "-m", "base"]);
        let remote = dir.path().join("remote.git");
        git(
            &root,
            &[
                "clone",
                "--bare",
                root.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        herdr_shepherd::trusted::ensure_snapshot(&dir.path().join("state"), &root).unwrap();
        git(&root, &["branch", "fix/t-1"]);
        git(&root, &["symbolic-ref", "HEAD", "refs/heads/fix/t-1"]);
        git(&root, &["commit", "--allow-empty", "-m", "change"]);
        let head = git(&root, &["rev-parse", "HEAD"]);
        fs::create_dir(dir.path().join("fake")).unwrap();
        fs::write(
            dir.path().join("fake/github.json"),
            r#"{"author":"fixture-user","calls":[]}"#,
        )
        .unwrap();
        fs::write(dir.path().join("herdr.json"), "{}").unwrap();
        for (file, text) in [
            ("body", "Fix the boundary"),
            ("summary", "Check boundary."),
            ("changes", "[\"Validate input\"]"),
            ("testing", "cargo test\n"),
        ] {
            fs::write(dir.path().join(file), text).unwrap();
        }
        let store = Store::open(&dir.path().join("state"), &root).unwrap();
        let fixture = Self { dir, store, head };
        let body = fixture.path("body");
        success(fixture.run(
            "ticket",
            &[
                "add",
                "--id",
                "T-1",
                "--title",
                "Fix boundary",
                "--type",
                "fix",
                "--base",
                "main",
                "--body-file",
                body.to_str().unwrap(),
            ],
        ));
        // Register only isolated fixture lifecycle/agent identities. Findings enter via public reports.
        fixture
            .store
            .update(|data| {
                let t = &mut data.tickets[0];
                t.state = TicketState::ReadyForPr;
                t.branch = "fix/t-1".into();
                t.worktree = Some(root.clone());
                t.qa_commit_sha = Some(fixture.head.clone());
                for name in ["fixture-coder", "fixture-reviewer"] {
                    data.agents.push(AgentRecord {
                        name: name.into(),
                        ticket: "T-1".into(),
                        role: "coder".into(),
                        runner: "fixture".into(),
                        status: AgentStatus::Working,
                        pane_id: None,
                        tab_id: None,
                        worktree: root.clone(),
                        task: "fixture".into(),
                        report: None,
                        observed: None,
                        last_progress_at: "0".into(),
                        nudges: 0,
                        restarts: 0,
                        prompt_pending: false,
                    });
                }
                Ok(())
            })
            .unwrap();
        fixture.report(
            "fixture-coder",
            json!([
                {"severity":"high","text":"Fixed boundary","location":"src/a.rs:5"},
                {"severity":"low","text":"Old issue","location":null},
                {"severity":"mid","text":"Fixed boundary","location":null}
            ]),
        );
        fixture
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }
    fn run(&self, group: &str, args: &[&str]) -> Output {
        isolated(
            &mut Command::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
            &self.path("home"),
        )
        .env("SHEPHERD_FAKE_GITHUB", self.path("fake"))
        .env("SHEPHERD_FAKE_HERDR", self.path("herdr.json"))
        .env("SHEPHERD_TEST_ALLOW_LOCAL_ORIGIN", "allow-local-in-tmp")
        .args(["--json", "--state-dir"])
        .arg(self.path("state"))
        .arg("--config-dir")
        .arg(self.path("cfg"))
        .args([group, "--project-root"])
        .arg(self.path("repo"))
        .args(args)
        .output()
        .unwrap()
    }
    fn report(&self, name: &str, findings: Value) {
        let report = json!({"status":"completed", "summary":"Checked boundary", "tests":[], "changed_paths":[], "blockers":[], "findings": findings, "commit_sha":null});
        fs::write(self.path("report.json"), report.to_string()).unwrap();
        success(self.run(
            "agent",
            &[
                "report",
                "--agent",
                name,
                "--report-file",
                self.path("report.json").to_str().unwrap(),
            ],
        ));
    }
    fn publish(&self, metadata: Option<Value>) -> Output {
        let summary = self.path("summary");
        let changes = self.path("changes");
        let testing = self.path("testing");
        let disposition = self.path("dispositions.json");
        let mut args = vec![
            "pr",
            "T-1",
            "--summary-file",
            summary.to_str().unwrap(),
            "--changes-file",
            changes.to_str().unwrap(),
            "--testing-file",
            testing.to_str().unwrap(),
        ];
        if let Some(metadata) = metadata {
            fs::write(&disposition, metadata.to_string()).unwrap();
            args.extend(["--finding-dispositions-file", disposition.to_str().unwrap()]);
        }
        self.run("ticket", &args)
    }
    fn backend(&self) -> Value {
        serde_json::from_slice(&fs::read(self.path("fake/github.json")).unwrap()).unwrap()
    }
}

#[test]
fn public_metadata_keeps_current_findings_and_raw_history() {
    let f = Fixture::new();
    let before = success(f.run("ticket", &["show", "T-1"]))["findings"].clone();
    let report_before = success(f.run("agent", &["status", "fixture-coder"]))["report"].clone();
    // This later report was absent when the first three dispositions were prepared.
    f.report(
        "fixture-reviewer",
        json!([{"severity":"wish","text":"New issue","location":null}]),
    );
    success(f.publish(Some(json!({"ticket_id":"T-1", "findings":[
        {"index":0,"status":"open"},
        {"index":1,"status":"resolved","reason":"Boundary test"},
        {"index":2,"status":"deferred","reason":"Follow-up"}
    ]}))));
    let expected = "create main T-1: Fix boundary ## Summary\n\nCheck boundary.\n\n## Changes\n\n- Validate input\n\n## Testing\n\n- cargo test\n\n## Open and deferred findings\n\n- #0 [High (Blocker); open] Fixed boundary (src/a.rs:5)\n- #2 [Mid; deferred] Fixed boundary\n  Reason: Follow-up\n\n## Finding history\n\n- #1 [Low; resolved] Old issue\n  Reason: Boundary test\n- #3 [Wish; current disposition unclassified] New issue\n";
    assert!(
        f.backend()["calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == expected)
    );
    let after = success(f.run("ticket", &["show", "T-1"]))["findings"].clone();
    assert_eq!(
        serde_json::to_vec(&before).unwrap(),
        serde_json::to_vec(&after.as_array().unwrap()[..3]).unwrap()
    );
    let report_after = success(f.run("agent", &["status", "fixture-coder"]))["report"].clone();
    assert_eq!(
        serde_json::to_vec(&report_before).unwrap(),
        serde_json::to_vec(&report_after).unwrap()
    );
    assert_eq!(
        git(&f.path("remote.git"), &["rev-parse", "refs/heads/fix/t-1"]),
        f.head
    );
}

fn rejected(out: Output, code: &str, message: &str) {
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["code"], code, "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains(message),
        "{error}"
    );
}

#[test]
fn legacy_and_empty_metadata_keep_every_finding_unclassified() {
    for metadata in [None, Some(json!({"ticket_id":"T-1", "findings":[]}))] {
        let f = Fixture::new();
        let before = f.store.read().unwrap();
        success(f.publish(metadata));
        let backend = f.backend();
        let body = backend["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .find(|call| call.starts_with("create "))
            .unwrap();
        assert!(body.contains("## Finding history"));
        assert!(!body.contains("## Not addressed"));
        assert!(!body.contains("## Open and deferred findings"));
        assert_eq!(body.matches("current disposition unclassified").count(), 3);
        assert!(body.contains(
            "#0 [High (Blocker); current disposition unclassified] Fixed boundary (src/a.rs:5)"
        ));
        assert!(body.contains("#1 [Low; current disposition unclassified] Old issue"));
        assert!(body.contains("#2 [Mid; current disposition unclassified] Fixed boundary"));
        let after = f.store.read().unwrap();
        assert_eq!(before.tickets[0].findings, after.tickets[0].findings);
        assert_eq!(before.agents, after.agents);
    }
}

#[test]
fn invalid_metadata_and_reasons_never_reach_backend() {
    let f = Fixture::new();
    let before = f.store.read().unwrap();
    for (metadata, code, message) in [
        (
            json!({"ticket_id":"T-2", "findings":[]}),
            "usage",
            "ticket_id",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[{"index":3,"status":"open"}]}),
            "usage",
            "out of range",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[{"index":0,"status":"open"},{"index":0,"status":"resolved"}]}),
            "usage",
            "duplicate",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[], "extra":1}),
            "usage",
            "unknown field",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[{"index":0,"status":"open","extra":1}]}),
            "usage",
            "unknown field",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[{"index":0,"status":"fixed"}]}),
            "usage",
            "unknown variant",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[{"index":0,"status":"resolved","reason":"Generated by Codex"}]}),
            "attribution",
            "attribution",
        ),
        (
            json!({"ticket_id":"T-1", "findings":[{"index":0,"status":"deferred","reason":"x".repeat(60_000)}]}),
            "usage",
            "60000",
        ),
    ] {
        rejected(f.publish(Some(metadata)), code, message);
        assert!(f.backend()["calls"].as_array().unwrap().is_empty());
        assert_eq!(f.store.read().unwrap(), before);
        assert!(!f.path("fake/github.lock").exists());
    }
    fs::write(f.path("dispositions.json"), "{").unwrap();
    rejected(
        f.run(
            "ticket",
            &[
                "pr",
                "T-1",
                "--summary-file",
                f.path("summary").to_str().unwrap(),
                "--changes-file",
                f.path("changes").to_str().unwrap(),
                "--testing-file",
                f.path("testing").to_str().unwrap(),
                "--finding-dispositions-file",
                f.path("dispositions.json").to_str().unwrap(),
            ],
        ),
        "usage",
        "invalid finding dispositions",
    );
    assert_eq!(f.store.read().unwrap(), before);
    assert!(!f.path("fake/github.lock").exists());
    assert!(
        git(
            &f.path("remote.git"),
            &["for-each-ref", "refs/heads/fix/t-1"]
        )
        .is_empty()
    );
}

#[test]
fn explicit_metadata_preserves_publication_gates() {
    for scenario in [
        "state",
        "missing QA",
        "stale QA",
        "protected branch",
        "backend identity",
    ] {
        let f = Fixture::new();
        f.store
            .update(|data| {
                let ticket = &mut data.tickets[0];
                match scenario {
                    "state" => ticket.state = TicketState::Building,
                    "missing QA" => ticket.qa_commit_sha = None,
                    "stale QA" => ticket.qa_commit_sha = Some("0".repeat(40)),
                    "protected branch" => ticket.branch = "main".into(),
                    _ => {}
                }
                Ok(())
            })
            .unwrap();
        if scenario == "backend identity" {
            fs::write(f.path("fake/github.json"), json!({"calls":[],"author":"fixture-user", "status":{
                "number":7,"url":"https://github.com/fake/shepherd/pull/7", "state":"open", "merged_at":null,
                "base":"main","head":"unrelated-branch","head_oid":f.head,
                "checks":"pending","review":"none","comment_ids":[]
            }}).to_string()).unwrap();
        }
        let before = f.store.read().unwrap();
        let (code, message) = match scenario {
            "state" => ("usage", "ready_for_pr"),
            "missing QA" => ("usage", "completed QA"),
            "stale QA" => ("conflict", "QA-tested"),
            "protected branch" => ("policy_denied", "protected"),
            _ => ("github_unavailable", "match"),
        };
        rejected(
            f.publish(Some(
                json!({"ticket_id":"T-1","findings":[{"index":0,"status":"open"}]}),
            )),
            code,
            message,
        );
        assert_eq!(f.store.read().unwrap(), before);
        assert!(f.backend()["calls"].as_array().unwrap().iter().all(|call| {
            let call = call.as_str().unwrap();
            !call.starts_with("push ")
                && !call.starts_with("create ")
                && !call.starts_with("reviewers ")
        }));
        assert!(
            git(
                &f.path("remote.git"),
                &["for-each-ref", "refs/heads/fix/t-1"]
            )
            .is_empty()
        );
    }
}

#[cfg(feature = "test-support")]
#[test]
fn library_publication_revalidates_metadata_without_cli_prevalidation() {
    use herdr_shepherd::{
        config,
        gate::{BranchPolicy, Gate},
        github::fake::FakeBackend,
        ticket::{pr, pr_body::FindingDispositions},
    };
    let f = Fixture::new();
    let backend = FakeBackend::new(vec![]);
    let gate = Gate {
        policy: BranchPolicy::new(&[], Some("main"), Some("main")).unwrap(),
        ticket_branch: "fix/t-1".into(),
        worktree: f.path("repo"),
        state_dir: f.path("state"),
        config_dir: f.path("cfg"),
    };
    for text in [
        r#"{"ticket_id":"T-2","findings":[]}"#,
        r#"{"ticket_id":"T-1","findings":[{"index":3,"status":"open"}]}"#,
        r#"{"ticket_id":"T-1","findings":[{"index":0,"status":"open"},{"index":0,"status":"resolved"}]}"#,
        r#"{"ticket_id":"T-1","findings":[{"index":0,"status":"resolved","reason":"Generated by Codex"}]}"#,
    ] {
        let metadata = FindingDispositions::parse(text).unwrap();
        assert!(
            pr::open_with_dispositions(
                &f.store,
                &backend,
                &gate,
                &config::load(&f.path("cfg"), None).unwrap(),
                "T-1",
                "Summary",
                &["Change".into()],
                &["Test".into()],
                Some(&metadata),
                "now"
            )
            .is_err()
        );
        assert!(backend.calls.borrow().is_empty());
    }
}
