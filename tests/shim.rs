use herdr_shepherd::gate::shim::install_shim;
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn shim_blocks_merge() {
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
