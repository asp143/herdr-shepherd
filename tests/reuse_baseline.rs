//! This regression uses only APIs present before assignment reuse. The same
//! source can run against fa44a81 without modifying its production code.
use std::path::Path;

use herdr_shepherd::{
    config::{Config, ConfigSource},
    flock,
    herdr::fake::FakeHerdr,
    launch::LaunchRequest,
    roles::{self, Role},
    store::{Store, Ticket, TicketState},
};

const REPORT: &str = r#"{"status":"completed","summary":"done","tests":[],"changed_paths":[],"blockers":[],"findings":[],"commit_sha":null}"#;

fn request(root: &Path, cfg: &Config, role: &str) -> LaunchRequest {
    LaunchRequest {
        ticket: "T-1".into(),
        role: Role {
            name: role.into(),
            ..Default::default()
        },
        runners: vec![roles::resolve(cfg).unwrap().runners["codex-sol-high"].clone()],
        worktree: root.into(),
        workspace_id: "w1".into(),
        tab_label: role.into(),
        prompt: "Perform this round's assigned task".into(),
        state_dir: root.join("state"),
    }
}

fn assert_reported_role_reuses_resources(role: &str) {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/baseline-reuse-fixtures");
    std::fs::create_dir_all(&base).unwrap();
    let dir = tempfile::tempdir_in(base).unwrap();
    let store = Store::open(&dir.path().join("state"), dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T-1".into(),
                state: TicketState::Building,
                worktree: Some(dir.path().into()),
                workspace_id: Some("w1".into()),
                ..Default::default()
            });
            Ok(())
        })
        .unwrap();
    let h = FakeHerdr::new(vec![]);
    let cfg = Config {
        schema_version: 1,
        enabled: true,
        upkeep: Default::default(),
        source: ConfigSource::Default,
        warnings: vec![],
        raw: toml::Table::new(),
    };
    let first = flock::spawn(&h, &store, &cfg, request(dir.path(), &cfg, role)).unwrap();
    let pane = first.pane_id.clone().unwrap();
    h.pane_sessions
        .borrow_mut()
        .insert(pane, "owned-session".into());
    store
        .update(|data| {
            let rec = &mut data.agents[0];
            let mut observed: serde_json::Value =
                serde_json::from_str(rec.observed.as_deref().unwrap()).unwrap();
            observed["session_ref"] = "owned-session".into();
            rec.observed = Some(observed.to_string());
            Ok(())
        })
        .unwrap();
    flock::store_report(&store, &first.name, REPORT).unwrap();
    let tab_count = || {
        h.calls
            .borrow()
            .iter()
            .filter(|call| call.starts_with("create_tab "))
            .count()
    };
    let before = (
        first.name,
        first.pane_id,
        first.tab_id,
        store.read().unwrap().agents.len(),
        tab_count(),
        h.start_args.borrow().len(),
    );
    let next = flock::spawn(&h, &store, &cfg, request(dir.path(), &cfg, role)).unwrap();
    let after = (
        next.name,
        next.pane_id,
        next.tab_id,
        store.read().unwrap().agents.len(),
        tab_count(),
        h.start_args.borrow().len(),
    );
    assert_eq!(
        after, before,
        "{role}: the next assignment must retain name, pane, tab, record count, tab count and process count"
    );
    assert!(next.report.is_none(), "the old report must be consumed");
    assert!(
        h.calls.borrow().iter().any(|call| {
            call.starts_with(&format!("prompt_agent {} ", before.0)) && call.ends_with(" true")
        }),
        "the reused agent must receive a waited task prompt"
    );
}

#[test]
fn coder_reuses_reported_resources() {
    assert_reported_role_reuses_resources("coder");
}

#[test]
fn reviewer_reuses_reported_resources() {
    assert_reported_role_reuses_resources("reviewer");
}

#[test]
fn qa_reuses_reported_resources() {
    assert_reported_role_reuses_resources("qa");
}
