use std::process::Command;

fn validate(config: &str) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shepherd.toml"), config).unwrap();
    Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "--config-dir"])
        .arg(dir.path())
        .args(["validate-config", "--project-root"])
        .arg(dir.path())
        .output()
        .unwrap()
}

#[test]
fn validate_config_adds_resolved_fields() {
    let output = validate("");
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    let data = &value["data"];
    assert_eq!(data["schema_version"], 1);
    assert_eq!(data["enabled"], true);
    assert_eq!(data["upkeep"]["regroup"], true);
    assert!(data["source"]["global"].is_string());
    assert_eq!(data["warnings"], serde_json::json!([]));
    assert_eq!(data["shepherd"]["runner"], "codex-sol-high");
    assert_eq!(data["roles"].as_array().unwrap().len(), 5);
    assert_eq!(data["runners"]["omp-astra"]["harness"], "omp");
    assert_eq!(data["supervision"]["rules"][0]["name"], "codex-trust");
    assert_eq!(
        data["supervision"]["rules"][0]["pattern"],
        r"(?s)Trust this folder\?.*Trust and continue"
    );
}

#[test]
fn validate_config_rejects_unknown_runner() {
    let output = validate("[roles.coder]\nrunners = ['missing']\n");
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "config_invalid");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing")
    );
}
