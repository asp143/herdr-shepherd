use herdr_shepherd::{
    github::protection::ProtectionWarning,
    store::{Store, Ticket},
};
use std::process::Command;

#[test]
fn status_shows_cached_protection_warning() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let fake = dir.path().join("fake");
    std::fs::create_dir(&fake).unwrap();
    std::fs::write(
        fake.join("github.json"),
        r#"{"default_branch":"main","protected":{"main":false}}"#,
    )
    .unwrap();
    let store = Store::open(&state, dir.path()).unwrap();
    store
        .update(|data| {
            data.tickets.push(Ticket {
                id: "T1".into(),
                base: "main".into(),
                protection_warnings: vec![ProtectionWarning {
                    branch: "main".into(),
                    reason: "unprotected".into(),
                }],
                ..Ticket::default()
            });
            Ok(())
        })
        .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("SHEPHERD_FAKE_GITHUB", &fake)
        .arg("--state-dir")
        .arg(&state)
        .arg("status")
        .arg("--project-root")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains(
        "main has no branch protection; shepherd's push guard is best effort without it"
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("SHEPHERD_FAKE_GITHUB", &fake)
        .arg("--json")
        .arg("--state-dir")
        .arg(&state)
        .arg("status")
        .arg("--project-root")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        value["data"]["tickets"][0]["protection_warnings"][0]["branch"],
        "main"
    );
    assert_eq!(
        value["data"]["protection_warnings"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
