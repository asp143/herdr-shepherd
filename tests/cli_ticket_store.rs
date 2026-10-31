use std::{path::Path, process::Command};

use herdr_shepherd::store::{Store, Ticket};

fn run(state: &Path, project: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(state)
        .args(["ticket", "--project-root"])
        .arg(project)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn ticket_list_and_show_use_project_store() {
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let store = Store::open(state.path(), project.path()).unwrap();
    let ticket = Ticket {
        id: "SYM-42".into(),
        title: "Fix parsing".into(),
        ..Ticket::default()
    };
    store
        .update(|data| {
            data.tickets.push(ticket.clone());
            Ok(())
        })
        .unwrap();
    let list = run(state.path(), project.path(), &["list"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );
    let list: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(list["schema_version"], 1);
    assert_eq!(list["ok"], true);
    assert_eq!(list["data"][0]["id"], "SYM-42");
    let show = run(state.path(), project.path(), &["show", "SYM-42"]);
    assert!(show.status.success());
    let show: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(show["data"], serde_json::to_value(ticket).unwrap());
}

#[test]
fn unknown_ticket_is_usage_error() {
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let out = run(state.path(), project.path(), &["show", "missing"]);
    assert_eq!(out.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "usage");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
}
