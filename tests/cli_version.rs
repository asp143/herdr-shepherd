#[test]
fn version_json_envelope() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "version"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn unknown_subcommand_exits_2() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["--json", "nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "usage");
}

#[test]
fn json_flag_is_global() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .args(["version", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
}

#[test]
fn help_exits_successfully() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8(out.stdout).unwrap().contains("version"));
}
