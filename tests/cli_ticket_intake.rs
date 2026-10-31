use std::{fs, process::Command};
#[test]
fn add_then_cancel_ticket_uses_json_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["commit", "--allow-empty", "-m", "base"]);
    let remote = dir.path().join("origin.git");
    git(&["init", "--bare", remote.to_str().unwrap()]);
    git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&["push", "origin", "main"]);
    let body = dir.path().join("ticket.txt");
    fs::write(&body, "Print hello\n").unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["--json", "--state-dir"])
            .arg(dir.path().join("state"))
            .args(["--config-dir"])
            .arg(dir.path().join("cfg"))
            .args(["ticket", "--project-root"])
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    let out = run(&[
        "add",
        "--id",
        "T-1",
        "--title",
        "Hello",
        "--body-file",
        body.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["state"], "queued");
    assert_eq!(v["data"]["branch"], "feat/t-1");
    let out = run(&["cancel", "T-1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["state"], "closed");
}
