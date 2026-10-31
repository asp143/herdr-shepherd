use std::{path::Path, process::Command};

fn run(project: &Path, config_dir: &Path, id: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env_remove("HERDR_PLUGIN_STATE_DIR")
        .args(["--json", "--config-dir"])
        .arg(config_dir)
        .args(["ticket", "--project-root"])
        .arg(project)
        .args(["fetch", id])
        .output()
        .unwrap()
}

#[test]
fn ticket_fetch_json_envelope_without_store() {
    let project = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let script = project.path().join("source.sh");
    std::fs::write(
        &script,
        r#"printf '{"id":"%s","title":"Fetch ticket","description":"Use literal argv","acceptance_criteria":["Keep order"],"url":null}' "$1"
"#,
    )
    .unwrap();
    let source = toml::Table::from_iter([(
        "ticket_source".into(),
        toml::Value::Table(toml::Table::from_iter([(
            "command".into(),
            toml::Value::Array(vec![
                toml::Value::String("sh".into()),
                toml::Value::String(script.to_str().unwrap().into()),
            ]),
        )])),
    )]);
    std::fs::write(
        global.path().join("shepherd.toml"),
        toml::to_string(&source).unwrap(),
    )
    .unwrap();
    let output = run(project.path(), global.path(), "SYM-7; false");
    assert_eq!(output.status.code(), Some(0));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["id"], "SYM-7; false");
    assert_eq!(value["data"]["title"], "Fetch ticket");
    assert_eq!(
        value["data"]["acceptance_criteria"],
        serde_json::json!(["Keep order"])
    );
}

#[test]
fn ticket_fetch_without_source_exits_2() {
    let project = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    let output = run(project.path(), global.path(), "SYM-7");
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "no_ticket_source");
}

#[test]
fn invalid_source_output_exits_1() {
    let project = tempfile::tempdir().unwrap();
    let global = tempfile::tempdir().unwrap();
    for fixture in [
        "not json".to_string(),
        "{}".to_string(),
        serde_json::json!({"id":"SYM-8","title":"Wrong ticket","description":"Text","acceptance_criteria":[]}).to_string(),
        serde_json::json!({"id":"SYM-7","title":"","description":"Text","acceptance_criteria":[]}).to_string(),
    ] {
        let source = toml::Table::from_iter([(
            "ticket_source".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "command".into(),
                toml::Value::Array(["sh", "-c", "printf '%s' \"$1\"", "source", &fixture].into_iter().map(|arg| toml::Value::String(arg.into())).collect()),
            )])),
        )]);
        std::fs::write(global.path().join("shepherd.toml"), toml::to_string(&source).unwrap()).unwrap();
        let output = run(project.path(), global.path(), "SYM-7");
        assert_eq!(output.status.code(), Some(1));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "runtime");
    }
}
