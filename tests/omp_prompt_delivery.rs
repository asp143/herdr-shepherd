#![cfg(feature = "test-support")]

use herdr_shepherd::{
    config::{Config, ConfigSource, Upkeep},
    harness::harness_for,
    herdr::fake::FakeHerdr,
    launch::{LaunchRequest, launch},
    prompts::role_prompt,
    roles::resolve,
    store::{Store, Ticket},
};
use std::{fs, path::PathBuf, process::Command};

/// Opt-in real OMP boundary test; never uses real auth, config or providers.
#[test]
#[ignore = "requires explicit SHEPHERD_TEST_OMP_BIN, Python 3 and bwrap"]
fn installed_omp_submits_managed_flock_task_once() {
    let omp = PathBuf::from(
        std::env::var_os("SHEPHERD_TEST_OMP_BIN").expect("explicit fixture OMP binary"),
    );
    assert!(omp.is_absolute());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let state = root.join("state");
    let custom = root.join("custom.yml");
    fs::write(&custom, "paste:\n  largeMenuThreshold: 100\n").unwrap();
    let cfg = Config {
        schema_version: 1,
        enabled: true,
        upkeep: Upkeep::default(),
        source: ConfigSource::Default,
        warnings: vec![],
        raw: toml::Table::new(),
    };
    let resolved = resolve(&cfg).unwrap();
    let role = resolved
        .roles
        .iter()
        .find(|r| r.name == "coder")
        .unwrap()
        .clone();
    let mut runner = resolved.runners["omp-astra"].clone();
    runner.args = [
        "--model",
        "fixture/probe",
        "--no-session",
        "--no-tools",
        "--no-lsp",
        "--no-pty",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--no-context-files",
        "--no-title",
        "--thinking",
        "off",
        "--system-prompt",
        "Synthetic terminal transport fixture.",
        "--config",
    ]
    .map(str::to_string)
    .to_vec();
    runner.args.push(custom.to_string_lossy().into_owned());
    let ticket = Ticket {
        id: "SYNTHETIC-OMP".into(),
        title: "Synthetic delivery fixture".into(),
        branch: "fix/synthetic-omp".into(),
        worktree: Some(root.into()),
        acceptance: (0..150)
            .map(|i| format!("synthetic acceptance {i}"))
            .collect(),
        ..Default::default()
    };
    let prompt = role_prompt(
        &root.join("shepherd-fixture"),
        &ticket,
        &role,
        "Inspect synthetic fixtures and submit a report; this fixture disables all tools.",
        "sh-fixture-coder-1",
        harness_for("omp").unwrap().as_ref(),
    );
    assert!(prompt.lines().count() > 150);
    let h = FakeHerdr::new(vec![]);
    let record = launch(
        &h,
        &Store::open(&state, root).unwrap(),
        LaunchRequest {
            ticket: ticket.id,
            role,
            runners: vec![runner],
            worktree: root.into(),
            workspace_id: "fixture:w1".into(),
            tab_label: "coder".into(),
            prompt,
            state_dir: state,
        },
    )
    .unwrap();
    assert_eq!(h.accepted_prompts.borrow().len(), 1);
    let args_file = root.join("args.json");
    let task_file = root.join("task.txt");
    fs::write(
        &args_file,
        serde_json::to_vec(&h.start_args.borrow()[0]).unwrap(),
    )
    .unwrap();
    fs::write(&task_file, record.task).unwrap();
    let script =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/omp_prompt_delivery.py");
    let output = Command::new("bwrap")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev-bind",
            "/dev",
            "/dev",
            "--bind",
        ])
        .arg(root)
        .arg(root)
        .args([
            "--unshare-net",
            "--",
            "env",
            "-i",
            "PATH=/usr/bin:/bin",
            "python3",
        ])
        .arg(script)
        .arg(root)
        .arg(omp)
        .arg(args_file)
        .arg(task_file)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("summary.json")).unwrap()).unwrap();
    for case in summary.as_array().unwrap() {
        assert_eq!(case["requests"].as_array().unwrap().len(), 1);
        println!("{}: exactly one synthetic provider turn", case["name"]);
    }
}
