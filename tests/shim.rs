use herdr_shepherd::gate::shim::install_shim;
use std::{fs, os::unix::fs::PermissionsExt, process::Command, sync::Mutex};

// A parallel fork can inherit another thread's writable script descriptor
// until exec. Keep script creation and execution in one critical section.
static SCRIPT_EXEC_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn shim_blocks_merge() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let state = tempfile::tempdir().unwrap();
    let dir = install_shim(
        state.path(),
        std::path::Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
    )
    .unwrap();
    let output = Command::new(dir.join("gh"))
        .args(["pr", "merge", "1"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("blocked"));
}

#[test]
fn shim_passes_through_allowed() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let state = tempfile::tempdir().unwrap();
    let real = state.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("gh"), "#!/bin/sh\nprintf 'REAL %s\\n' \"$*\"\n").unwrap();
    fs::set_permissions(real.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
    let dir = install_shim(
        state.path(),
        std::path::Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
    )
    .unwrap();
    let path = std::env::join_paths([dir.clone(), real]).unwrap();
    let output = Command::new(dir.join("gh"))
        .env("PATH", path)
        .args(["pr", "view", "1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "REAL pr view 1\n");
}

#[test]
fn shim_blocks_merge_api_bypasses() {
    let _guard = SCRIPT_EXEC_LOCK.lock().unwrap();
    let state = tempfile::tempdir().unwrap();
    let real = state.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("gh"), "#!/bin/sh\nprintf 'UNEXPECTED_REAL_GH'\n").unwrap();
    fs::set_permissions(real.join("gh"), fs::Permissions::from_mode(0o700)).unwrap();
    let dir = install_shim(
        state.path(),
        std::path::Path::new(env!("CARGO_BIN_EXE_herdr-shepherd")),
    )
    .unwrap();
    let path = std::env::join_paths([dir.clone(), real]).unwrap();
    for args in [
        vec![
            "api",
            "/graphql",
            "-f",
            "query=mutation { mergePullRequest }",
        ],
        vec![
            "api",
            "https://api.github.com/graphql",
            "-f",
            "query=mutation { addPullRequestReview }",
        ],
        vec!["api", "-X=PUT", "repos/o/r/pulls/12/merge"],
        vec!["api", "-X", "PUT", "repos/o/r/pulls/12/%6derge"],
        vec!["api", "repos/o/r/pulls/12/reviews", "--input=ordinary.json"],
        vec!["pr", "-R", "o/r", "merge", "12"],
        vec!["pr", "review", "12", "-ab", "ordinary review"],
    ] {
        let output = Command::new(dir.join("gh"))
            .env("PATH", &path)
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(4),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("blocked"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("UNEXPECTED_REAL_GH"));
    }
}
