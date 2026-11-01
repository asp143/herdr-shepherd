use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

#[test]
fn plugin_context_selects_invoking_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("checkout");
    std::fs::create_dir(&project).unwrap();
    let path = project.join(".shepherd.toml");
    std::fs::write(&path, "[invalid").unwrap();
    let context = serde_json::json!({
        "worktree": {"checkout_path": project, "repo_root": dir.path()},
        "workspace_cwd": dir.path(), "focused_pane_cwd": dir.path(),
    });
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .current_dir(dir.path())
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .arg("--state-dir")
        .arg(dir.path().join("state"))
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--json", "validate-config"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "config_invalid");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap())
    );
}

#[test]
fn bad_project_config_exits_2() {
    let global = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let path = project.path().join(".shepherd.toml");
    std::fs::write(&path, "[invalid").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(global.path().join("state"))
        .arg("--config-dir")
        .arg(global.path())
        .args(["validate-config", "--project-root"])
        .arg(project.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "config_invalid");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains(path.to_str().unwrap())
    );
}

#[test]
fn missing_config_uses_claude_lead_default() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--state-dir"])
        .arg(dir.path().join("state"))
        .arg("--config-dir")
        .arg(dir.path())
        .args(["validate-config", "--project-root"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["data"]["shepherd"]["runner"], "claude-opus");
    assert_eq!(value["data"]["runners"]["claude-opus"]["harness"], "claude");
}

struct ConfigRepo {
    dir: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    config: PathBuf,
}

impl ConfigRepo {
    fn new(text: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = Self {
            root: dir.path().join("repo"),
            state: dir.path().join("state"),
            config: dir.path().join("config"),
            dir,
        };
        fs::create_dir(&repo.root).unwrap();
        fs::create_dir(&repo.config).unwrap();
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Config Fixture"]);
        repo.git(&["config", "user.email", "fixture@example.invalid"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        if let Some(text) = text {
            fs::write(repo.root.join(".shepherd.toml"), text).unwrap();
            repo.git(&["add", ".shepherd.toml"]);
        }
        repo.git(&["commit", "--allow-empty", "-m", "fixture"]);
        repo
    }

    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&self.root);
        command
    }

    fn git(&self, args: &[&str]) {
        let output = self.command("git").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn cli(&self, json: bool, args: &[&str]) -> Output {
        let mut command = self.command(env!("CARGO_BIN_EXE_herdr-shepherd"));
        command
            .arg("--state-dir")
            .arg(&self.state)
            .arg("--config-dir")
            .arg(&self.config);
        if json {
            command.arg("--json");
        }
        command
            .args(args)
            .arg("--project-root")
            .arg(&self.root)
            .output()
            .unwrap()
    }

    fn trust(&self) -> serde_json::Value {
        json_data(self.cli(true, &["config", "trust"]))
    }

    fn validate(&self) -> serde_json::Value {
        json_data(self.cli(true, &["validate-config"]))
    }
}

fn json_data(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["data"].clone()
}

fn state_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.push((path.strip_prefix(root).unwrap().to_path_buf(), Vec::new()));
                visit(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    if root.exists() {
        visit(root, root, &mut files);
    }
    files.sort();
    files
}

fn assert_drift_warning(value: &serde_json::Value) {
    let warnings = value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{value}");
    let warning = warnings[0].as_str().unwrap().to_lowercase();
    assert!(
        warning.contains("runtime") && warning.contains("snapshot"),
        "{warning}"
    );
    assert!(
        warning.contains("commit") && warning.contains("default branch"),
        "{warning}"
    );
    assert!(warning.contains("config trust"), "{warning}");
    assert!(!warning.contains("local-only-marker"), "{warning}");
}

#[test]
fn validate_warns_when_local_config_differs_without_writing_state() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    repo.trust();
    let before = state_files(&repo.state);
    fs::write(
        repo.root.join(".shepherd.toml"),
        "enabled=false\n# local-only-marker\n",
    )
    .unwrap();
    let value = repo.validate();
    assert_drift_warning(&value);
    assert_eq!(value["enabled"], false);
    assert!(value["source"].get("project").is_some());
    for field in [
        "schema_version",
        "upkeep",
        "shepherd",
        "roles",
        "runners",
        "supervision",
    ] {
        assert!(value.get(field).is_some(), "missing {field}: {value}");
    }
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_warns_when_snapshot_has_no_config() {
    let repo = ConfigRepo::new(None);
    repo.trust();
    let before = state_files(&repo.state);
    fs::write(repo.root.join(".shepherd.toml"), "enabled=false\n").unwrap();
    assert_drift_warning(&repo.validate());
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_warns_when_snapshot_config_is_missing_on_disk() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    repo.trust();
    let before = state_files(&repo.state);
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    assert_drift_warning(&repo.validate());
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_matching_config_has_no_warning_and_writes_no_state() {
    for text in [None, Some("enabled=true\n")] {
        let repo = ConfigRepo::new(text);
        repo.trust();
        let before = state_files(&repo.state);
        assert_eq!(repo.validate()["warnings"], serde_json::json!([]));
        assert_eq!(state_files(&repo.state), before);
    }
}

#[test]
fn validate_without_snapshot_has_no_warning_and_creates_no_state() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    assert!(!repo.state.exists());
    assert_eq!(repo.validate()["warnings"], serde_json::json!([]));
    assert!(!repo.state.exists());
}

#[test]
fn validate_existing_store_without_snapshot_stays_read_only() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.root).unwrap();
    store.update(|_| Ok(())).unwrap();
    let before = state_files(&repo.state);
    assert_eq!(repo.validate()["warnings"], serde_json::json!([]));
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn config_trust_json_warns_about_local_edits_and_uses_committed_config() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    let initial = repo.trust();
    fs::write(
        repo.root.join(".shepherd.toml"),
        "enabled=false\n# local-only-marker\n",
    )
    .unwrap();
    let value = repo.trust();
    let warnings = value["warnings"]
        .as_array()
        .expect("JSON trust warnings field");
    assert_eq!(warnings.len(), 1);
    let warning = warnings[0].as_str().unwrap().to_lowercase();
    assert!(
        warning.contains("local") && warning.contains("not captured"),
        "{warning}"
    );
    assert!(!warning.contains("local-only-marker"), "{warning}");
    assert_eq!(value["new"]["config_text"], "enabled=true\n");
    assert_eq!(value["new"]["config_blob"], initial["new"]["config_blob"]);
    let runtime =
        herdr_shepherd::trusted::committed_config(&repo.config, &repo.root, None, &repo.state)
            .unwrap();
    assert!(runtime.enabled);
}

#[test]
fn config_trust_human_output_warns_about_uncaptured_local_edits() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    fs::write(
        repo.root.join(".shepherd.toml"),
        "enabled=false\n# local-only-marker\n",
    )
    .unwrap();
    let output = repo.cli(false, &["config", "trust"]);
    assert!(output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("warning:") && text.contains("not captured"),
        "{text}"
    );
    assert!(!text.contains("local-only-marker"), "{text}");
}

#[test]
fn validate_existing_empty_project_state_directory_stays_read_only() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.root).unwrap();
    fs::create_dir_all(store.root()).unwrap();
    let before = state_files(&repo.state);
    assert_eq!(repo.validate()["warnings"], serde_json::json!([]));
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_snapshot_with_missing_lock_file_stays_read_only() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    repo.trust();
    let store = herdr_shepherd::store::Store::open(&repo.state, &repo.root).unwrap();
    fs::remove_file(store.root().join("state.lock")).unwrap();
    let before = state_files(&repo.state);
    fs::write(repo.root.join(".shepherd.toml"), "enabled=false\n").unwrap();
    assert_drift_warning(&repo.validate());
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_rejects_project_symlinks_without_parsing_target_contents() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    repo.trust();
    fs::remove_file(repo.root.join(".shepherd.toml")).unwrap();
    let target = repo.dir.path().join("invalid-config");
    fs::write(&target, "[symlink-target-marker\n").unwrap();
    std::os::unix::fs::symlink(&target, repo.root.join(".shepherd.toml")).unwrap();
    let before = state_files(&repo.state);
    let output = repo.cli(true, &["validate-config"]);
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "config_invalid");
    assert!(
        !value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("symlink-target-marker")
    );
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_linked_worktree_compares_its_file_to_shared_snapshot() {
    let mut repo = ConfigRepo::new(Some("enabled=true\n"));
    repo.trust();
    let before = state_files(&repo.state);
    let worktree = repo.dir.path().join("linked");
    repo.git(&[
        "worktree",
        "add",
        "-b",
        "feat/validation",
        worktree.to_str().unwrap(),
        "main",
    ]);
    repo.root = worktree;
    fs::write(repo.root.join(".shepherd.toml"), "enabled=false\n").unwrap();
    assert_drift_warning(&repo.validate());
    assert_eq!(state_files(&repo.state), before);
}

#[test]
fn validate_human_output_preserves_existing_warnings_and_adds_drift() {
    let repo = ConfigRepo::new(Some("enabled=true\n"));
    repo.trust();
    fs::write(
        repo.root.join(".shepherd.toml"),
        "enabled=false\n[upkeep]\nregroup=false\n",
    )
    .unwrap();
    let output = repo.cli(false, &["validate-config"]);
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let warnings = value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2);
    assert_eq!(warnings[0], "[upkeep] in project config is ignored");
    assert!(warnings[1].as_str().unwrap().contains("config trust"));
}
