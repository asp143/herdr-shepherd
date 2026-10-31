use std::{fs, process::Command};

#[test]
fn emit_keeps_json_envelope_and_passes_payload() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("hook-output");
    let payload = dir.path().join("payload.json");
    fs::write(&payload, r#"{"pr":12}"#).unwrap();
    let text = format!(
        "[[hooks]]\non = 'ticket.cleaned_up'\ncommand = ['sh', '-c', 'cat > \"$1\"; echo hook-stdout', 'hook', '{}']\n",
        out.display()
    );
    fs::write(dir.path().join("shepherd.toml"), text).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_herdr-shepherd"))
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(dir.path())
        .args(["--json", "--config-dir"])
        .arg(dir.path())
        .args(["hooks", "emit", "ticket.cleaned_up", "--json-file"])
        .arg(payload)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["data"]["event"], "ticket.cleaned_up");
    assert_eq!(fs::read_to_string(out).unwrap(), r#"{"pr":12}"#);
}
