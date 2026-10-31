use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

fn command() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"));
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HERDR_SOCKET_PATH", "/nonexistent/sock")
        .env("HERDR_BIN_PATH", "/nonexistent/herdr");
    cmd
}

#[test]
fn works_without_herdr() {
    let out = command()
        .args(["--json", "prune", "--root", "/nonexistent", "--no-size"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["entries"], serde_json::json!([]));
    assert!(
        v["data"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("herdr"))
    );
}

fn snapshot(root: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut rows = vec![];
    let mut todo = vec![root.to_path_buf()];
    while let Some(p) = todo.pop() {
        let metadata = fs::symlink_metadata(&p).unwrap();
        rows.push((p.clone(), metadata.modified().unwrap()));
        if metadata.is_dir() {
            todo.extend(fs::read_dir(&p).unwrap().map(|entry| entry.unwrap().path()));
        }
    }
    rows.sort();
    rows
}

#[test]
fn never_writes() {
    let root = tempfile::tempdir().unwrap();
    let git = root.path().join("example/feat-one");
    fs::create_dir_all(&git).unwrap();
    assert!(
        Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["init", "--quiet", "--initial-branch", "feat/one"])
            .arg(&git)
            .status()
            .unwrap()
            .success()
    );
    fs::write(git.join("ordinary.txt"), "ordinary fixture\n").unwrap();
    for args in [
        vec!["config", "user.name", "Fixture"],
        vec!["config", "user.email", "fixture@example.invalid"],
        vec!["add", "ordinary.txt"],
        vec!["commit", "-m", "fixture"],
    ] {
        let out = Command::new("git")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&git)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(git.join(".git/index").exists());
    fs::write(git.join("ordinary.txt"), "modified fixture\n").unwrap();
    fs::create_dir(root.path().join("example/orphan")).unwrap();
    fs::write(root.path().join("example/stray.txt"), "ordinary fixture\n").unwrap();
    let before = snapshot(root.path());
    let out = command()
        .args(["--json", "prune", "--no-size", "--root"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(snapshot(root.path()), before);
}
