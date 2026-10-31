#[test]
fn manifest_parses_and_matches_crate() {
    let text =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/herdr-plugin.toml")).unwrap();
    let manifest: toml::Table = text.parse().unwrap();
    assert_eq!(manifest["id"].as_str(), Some("herdr-shepherd"));
    assert_eq!(
        manifest["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(manifest["min_herdr_version"].as_str(), Some("0.9.5"));
    for section in ["build", "actions", "events", "startup"] {
        for item in manifest[section].as_array().unwrap() {
            let command = item["command"].as_array().unwrap();
            assert!(matches!(
                command[0].as_str(),
                Some("bin/herdr-shepherd" | "sh")
            ));
        }
    }
    let events: Vec<_> = manifest["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["on"].as_str().unwrap())
        .collect();
    assert_eq!(events, ["worktree.created", "worktree.removed"]);
    assert_eq!(manifest["startup"].as_array().unwrap().len(), 1);
    assert_eq!(
        manifest["startup"][0]["command"],
        toml::Value::Array(
            ["bin/herdr-shepherd", "watch", "--detach"]
                .into_iter()
                .map(|v| toml::Value::String(v.into()))
                .collect()
        )
    );
    assert!(!manifest.contains_key("keys"));
}
