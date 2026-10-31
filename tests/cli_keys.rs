use std::process::Command;

#[test]
fn keys_prints_replacement_without_editing_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.toml");
    let original = "[[keys.command]]\nkey = 'prefix+alt+g'\ncommand = 'herdr-regroup'\n";
    std::fs::write(&path, original).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--json", "keys", "--herdr-config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["data"]["toml"]
            .as_str()
            .unwrap()
            .contains("# replaces herdr-regroup binding")
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn invalid_herdr_config_includes_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("herdr.toml");
    std::fs::write(&path, "[keys").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--json", "keys", "--herdr-config"])
        .arg(&path)
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
fn global_keybindings_toggle_emits_empty_snippet() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("shepherd.toml"),
        "[upkeep]\nkeybindings = false\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--json", "keys", "--herdr-config"])
        .arg(dir.path().join("missing.toml"))
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["data"]["toml"], "");
}
