#[test]
fn second_lock_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let _guard = herdr_shepherd::watch::lock::acquire(dir.path())
        .unwrap()
        .unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(dir.path())
        .args(["watch", "--detach"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["data"]["status"], "already_running");
}

/// Own only this test's detached child; even assertion failures stop it.
struct DetachedChild(u32);
impl Drop for DetachedChild {
    fn drop(&mut self) {
        let _ = std::process::Command::new("kill")
            .arg(self.0.to_string())
            .status();
    }
}

#[test]
fn detach_handshake_prevents_duplicate_pollers() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("shepherd.toml"), "[upkeep]\npr_status=false\n").unwrap();
    let command = || {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
        cmd.args(["--json", "--state-dir"])
            .arg(dir.path())
            .arg("--config-dir")
            .arg(&config)
            .args(["watch", "--detach", "--interval", "1"]);
        cmd
    };
    let first = command().output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(value["data"]["status"], "started");
    let child = DetachedChild(value["data"]["pid"].as_u64().unwrap() as u32);
    assert!(
        herdr_shepherd::watch::lock::acquire(dir.path())
            .unwrap()
            .is_none()
    );
    let second = command().output().unwrap();
    assert!(second.status.success());
    let value: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(value["data"]["status"], "already_running");
    drop(child);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while herdr_shepherd::watch::lock::acquire(dir.path())
        .unwrap()
        .is_none()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "detached child did not release lock"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
