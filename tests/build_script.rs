use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn build_uses_explicit_target_with_custom_env() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    fs::create_dir(&tools).unwrap();
    let cargo = tools.join("cargo");
    fs::write(&cargo, "#!/bin/sh\n[ \"$*\" = 'build --release --locked --target-dir target' ] || exit 9\nmkdir -p target/release\nprintf binary > target/release/herdr-shepherd\n").unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
    let script = dir.path().join("build.sh");
    fs::write(&script, include_str!("../scripts/build.sh")).unwrap();
    let path = std::env::join_paths([tools, "/usr/bin".into(), "/bin".into()]).unwrap();
    let output = Command::new("/bin/sh")
        .arg(script)
        .current_dir(dir.path())
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", "custom")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("bin/herdr-shepherd")).unwrap(),
        "binary"
    );
    assert!(!dir.path().join("custom").exists());
}
