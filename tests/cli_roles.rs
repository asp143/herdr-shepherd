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

#[test]
fn validate_config_rejects_typos_in_every_section() {
    for (text, typo) in [
        (
            "[shepherd]\nmax_concurent_tickets = 9",
            "max_concurent_tickets",
        ),
        ("[roles.coder]\nrunner = 'codex-sol-high'", "runner"),
        ("[runners.codex-sol-high]\nharnes = 'codex'", "harnes"),
        ("[supervision]\nsweep_second = 5", "sweep_second"),
        (
            "[[supervision.rules]]\nname = 'custom'\npattern = 'Continue'\nkeys = ['enter']\nharnes = 'codex'",
            "harnes",
        ),
    ] {
        let output = validate(text);
        assert_eq!(output.status.code(), Some(2), "{text}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "config_invalid", "{text}");
        assert!(
            value["error"]["message"].as_str().unwrap().contains(typo),
            "{text}"
        );
    }
}

#[test]
fn validate_config_rejects_empty_role_runners() {
    let output = validate("[roles.coder]\nrunners = []");
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "config_invalid");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("coder")
    );
}

#[test]
fn validate_config_rejects_invalid_rules() {
    for fields in [
        "name = 'custom'\npattern = ''\nkeys = ['enter']",
        "name = 'custom'\npattern = '   '\nkeys = ['enter']",
        "name = 'custom'\npattern = 'Continue'\nkeys = []",
        "name = 'custom'\npattern = 'Continue'\nkeys = ['enter']\nharness = 'future'",
        "name = 'codex-trust'\npattern = 'Continue'\nkeys = ['enter']",
        "name = 'press-enter'\npattern = 'Continue'\nkeys = ['enter']",
        "name = 'codex-update-notice'\npattern = 'Continue'\nkeys = ['enter']",
        "name = 'custom'\npattern = 'Continue'\nkeys = ['enter']\n[[supervision.rules]]\nname = 'custom'\npattern = 'Next'\nkeys = ['y']",
    ] {
        let text = format!("[[supervision.rules]]\n{fields}");
        let output = validate(&text);
        assert_eq!(output.status.code(), Some(2), "{text}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "config_invalid", "{text}");
    }
}

#[test]
fn validate_config_preserves_catalog_file_order() {
    let config = "[roles.z-last]\nstage = 'review'\n[roles.coder]\nprompt = 'Keep cwd'\n[roles.a-first]\nstage = 'qa'\n[runners.z-last]\nharness = 'codex'\n[runners.omp-astra]\nargs = ['--model', 'custom']\n[runners.a-first]\nharness = 'omp'";
    let first = validate(config);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let value: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let roles = value["data"]["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|role| role["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        [
            "z-last",
            "coder",
            "a-first",
            "planner",
            "researcher",
            "reviewer",
            "qa"
        ]
    );
    let runners = value["data"]["runners"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        runners,
        [
            "z-last",
            "omp-astra",
            "a-first",
            "codex-sol-high",
            "omp-glm-infra"
        ]
    );
    let second = validate(config);
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(value["data"]["roles"], second["data"]["roles"]);
    assert_eq!(
        value["data"]["runners"].to_string(),
        second["data"]["runners"].to_string()
    );
}

#[test]
fn validate_config_accepts_supported_rule_harnesses() {
    for harness in [None, Some("codex"), Some("omp"), Some("claude"), Some("pi")] {
        let mut text =
            "[[supervision.rules]]\nname = 'custom'\npattern = 'Continue'\nkeys = ['enter']"
                .to_string();
        if let Some(harness) = harness {
            text.push_str(&format!("\nharness = '{harness}'"));
        }
        let output = validate(&text);
        assert!(
            output.status.success(),
            "{text}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
