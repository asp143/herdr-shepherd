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

#[test]
fn detached_child_retries_transient_parent_lock() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("shepherd.toml"), "[upkeep]\npr_status=false\n").unwrap();
    let guard = herdr_shepherd::watch::lock::acquire(dir.path())
        .unwrap()
        .unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(dir.path())
        .arg("--config-dir")
        .arg(&config)
        .args(["watch", "--once"])
        .env("SHEPHERD_WATCH_DETACHED", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(120));
    drop(guard);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["data"]["status"], "completed");
}

#[test]
fn detach_rotates_watch_log_past_five_mib() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("shepherd.toml"), "[upkeep]\npr_status=false\n").unwrap();
    let log = std::fs::File::create(dir.path().join("watch.log")).unwrap();
    let old_size = 5 * 1024 * 1024 + 1;
    log.set_len(old_size).unwrap();
    drop(log);
    std::fs::write(dir.path().join("watch.log.1"), "older log").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(dir.path())
        .arg("--config-dir")
        .arg(&config)
        .args(["watch", "--detach", "--interval", "1"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let _child = DetachedChild(value["data"]["pid"].as_u64().unwrap() as u32);
    assert_eq!(
        std::fs::metadata(dir.path().join("watch.log.1"))
            .unwrap()
            .len(),
        old_size
    );
    assert!(
        std::fs::metadata(dir.path().join("watch.log"))
            .unwrap()
            .len()
            < 5 * 1024 * 1024
    );
}
