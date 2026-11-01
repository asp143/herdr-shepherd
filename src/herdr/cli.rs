use std::{
    collections::HashMap,
    env, io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde_json::json;

use super::{AgentInfo, AgentStart, Herdr, Workspace, WorktreeInfo, model, socket};
use crate::{error::ShepherdError, proc};

pub struct CliHerdr {
    pub bin: PathBuf,
    pub socket: PathBuf,
}

impl CliHerdr {
    pub fn from_env() -> Self {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"));
        Self {
            bin: env::var_os("HERDR_BIN_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("herdr")),
            socket: env::var_os("HERDR_SOCKET_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".config/herdr/herdr.sock")),
        }
    }

    fn command(&self, args: &[&str]) -> Result<String, ShepherdError> {
        // Check connectivity rather than existence: stale sockets also mean the
        // server is unavailable, regardless of how the CLI reports its error.
        drop(socket::connect(&self.socket)?);
        match proc::run(
            Command::new(&self.bin)
                .env("HERDR_SOCKET_PATH", &self.socket)
                .args(args),
            Duration::from_secs(20),
        ) {
            Ok(Some(out)) => Ok(out),
            Ok(None) => {
                drop(socket::connect(&self.socket)?);
                Err(ShepherdError::Runtime(anyhow::anyhow!(
                    "herdr command failed or timed out: {}",
                    args.join(" ")
                )))
            }
            Err(err)
                if err
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                Err(ShepherdError::HerdrUnavailable(format!(
                    "herdr binary {} unavailable (socket {})",
                    self.bin.display(),
                    self.socket.display()
                )))
            }
            Err(err) => Err(ShepherdError::Runtime(err)),
        }
    }

    fn capture(&self, args: &[&str]) -> Result<proc::Captured, ShepherdError> {
        drop(socket::connect(&self.socket)?);
        proc::capture(
            Command::new(&self.bin)
                .env("HERDR_SOCKET_PATH", &self.socket)
                .args(args),
            Duration::from_secs(45),
            None,
        )
        .map_err(|error| {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                ShepherdError::HerdrUnavailable(format!(
                    "herdr binary {} unavailable (socket {})",
                    self.bin.display(),
                    self.socket.display()
                ))
            } else {
                ShepherdError::Runtime(error)
            }
        })
    }

    /// Some herdr commands acknowledge success without JSON (pane run),
    /// or print plain terminal text (pane wait-output and agent read).
    fn capture_success(&self, args: &[&str]) -> Result<String, ShepherdError> {
        let output = self.capture(args)?;
        if !output.status.success() {
            return Err(command_error(&output));
        }
        Ok(output.stdout)
    }

    fn request(&self, args: &[&str]) -> Result<serde_json::Value, ShepherdError> {
        let output = self.capture_success(args)?;
        if output.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&output)
            .map_err(anyhow::Error::from)
            .map_err(Into::into)
    }
}

fn response_error(output: &proc::Captured) -> Option<String> {
    [&output.stderr, &output.stdout]
        .into_iter()
        .find_map(|text| {
            serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/code")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
        })
}

fn command_error(output: &proc::Captured) -> ShepherdError {
    // Do not echo arguments: pane environment scripts and prompts can contain
    // private user text. Preserve the server's error code for launch decisions.
    if let Some(code) = response_error(output) {
        ShepherdError::HerdrCommand {
            code,
            message: "command failed".into(),
        }
    } else {
        anyhow::anyhow!("herdr command failed: exit {}", output.status).into()
    }
}

fn id(value: &serde_json::Value, key: &str) -> Result<String, ShepherdError> {
    value
        .as_str()
        .or_else(|| value.get(key).and_then(serde_json::Value::as_str))
        .or_else(|| value.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("herdr response lacks {key}").into())
}

/// Shell-quote an environment snapshot without inheriting GitHub credentials.
/// Only explicitly supplied variables are exported; the launcher supplies a
/// small safe set rather than serializing its entire process environment.
pub fn env_script(values: &HashMap<String, String>) -> Result<String, ShepherdError> {
    let mut script = concat!(
        "unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN ",
        "SSH_AUTH_SOCK GIT_SSH GIT_SSH_COMMAND GIT_ASKPASS SSH_ASKPASS ",
        "GIT_CONFIG GIT_CONFIG_PARAMETERS GIT_CONFIG_COUNT GIT_CONFIG_SYSTEM ",
        "GIT_CONFIG_GLOBAL GIT_CONFIG_NOSYSTEM; ",
        "for shepherd_git_var in $(env | sed -n ",
        "'s/^\\(GIT_CONFIG_\\(KEY\\|VALUE\\)_[0-9][0-9]*\\)=.*/\\1/p'); ",
        "do unset \"$shepherd_git_var\"; done; "
    )
    .to_owned();
    let mut keys: Vec<_> = values.keys().collect();
    keys.sort();
    for key in keys {
        if key.is_empty()
            || !key.bytes().enumerate().all(|(n, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (n > 0 && byte.is_ascii_digit())
            })
        {
            return Err(ShepherdError::Usage(
                "invalid environment variable name".into(),
            ));
        }
        let value = values[key].replace('\'', "'\\''");
        script.push_str(&format!("export {key}='{value}'; "));
    }
    Ok(script)
}

fn metadata_request(
    h: &CliHerdr,
    kind: &str,
    id: &str,
    set: &[(String, String)],
    clear: &[String],
) -> Result<(), ShepherdError> {
    let mut tokens = serde_json::Map::new();
    for (key, value) in set {
        tokens.insert(key.clone(), json!(value));
    }
    for key in clear {
        tokens.insert(key.clone(), serde_json::Value::Null);
    }
    let mut params = json!({"source":"shepherd", "tokens":tokens});
    params[format!("{kind}_id")] = json!(id);
    socket::request(
        &h.socket,
        &json!({
            "id":"shepherd-metadata",
            "method":format!("{kind}.report_metadata"),
            "params":params,
        }),
    )
    .map(|_| ())
}

impl Herdr for CliHerdr {
    fn create_tab(
        &self,
        ws: &str,
        label: &str,
        cwd: &Path,
    ) -> Result<(String, String), ShepherdError> {
        let value = self.request(&[
            "tab",
            "create",
            "--workspace",
            ws,
            "--cwd",
            &cwd.to_string_lossy(),
            "--label",
            label,
            "--no-focus",
        ])?;
        Ok((
            id(&value["result"]["tab"], "tab_id")?,
            id(&value["result"]["root_pane"], "pane_id")?,
        ))
    }

    fn create_worktree(
        &self,
        repo: &Path,
        branch: &str,
        label: &str,
        base: &str,
        path: &Path,
    ) -> Result<Workspace, ShepherdError> {
        let value = self.request(&[
            "worktree",
            "create",
            "--cwd",
            &repo.to_string_lossy(),
            "--branch",
            branch,
            "--label",
            label,
            "--base",
            base,
            "--path",
            &path.to_string_lossy(),
            "--no-focus",
        ])?;
        let mut workspace: Workspace = serde_json::from_value(value["result"]["workspace"].clone())
            .map_err(anyhow::Error::from)?;
        workspace.worktree.get_or_insert(WorktreeInfo {
            repo_key: repo.to_string_lossy().into_owned(),
            repo_root: Some(repo.into()),
            checkout_path: Some(path.into()),
            is_linked_worktree: true,
        });
        Ok(workspace)
    }

    fn start_agent(
        &self,
        name: &str,
        kind: &str,
        pane: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<AgentStart, ShepherdError> {
        let cleaned = env.get("GIT_CONFIG_GLOBAL").ok_or_else(|| {
            ShepherdError::Usage("agent launch requires sanitized Git configuration".into())
        })?;
        let state_dir = Path::new(cleaned).parent().ok_or_else(|| {
            ShepherdError::Usage("sanitized Git configuration needs a state directory".into())
        })?;
        let source = ["HOME", "XDG_CONFIG_HOME", "GIT_CONFIG_GLOBAL"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
            .collect();
        let prepared = crate::gate::env::prepare_agent_gitconfig(&source, state_dir)?;
        let mut values = env.clone();
        values.insert(
            "GIT_CONFIG_GLOBAL".into(),
            prepared.to_string_lossy().into_owned(),
        );
        let sequence = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let suffix = format!("{sequence:x}");
        let sentinel = format!("shepherd-ready-{suffix}");
        // Splitting the marker prevents the terminal's echoed command from
        // satisfying wait-output before the exports have actually executed.
        let script = format!(
            "{}printf '%s%s\\n' 'shepherd-ready-' '{suffix}'",
            env_script(&values)?
        );
        self.capture_success(&["pane", "run", pane, &script])?;
        self.capture_success(&[
            "pane",
            "wait-output",
            pane,
            "--match",
            &sentinel,
            "--timeout",
            "10000",
        ])?;
        let mut command = vec![
            "agent",
            "start",
            name,
            "--kind",
            kind,
            "--pane",
            pane,
            "--timeout",
            "30000",
            "--",
        ];
        command.extend(args.iter().map(String::as_str));
        let output = self.capture(&command)?;
        if output.status.success() {
            return Ok(AgentStart::Ready);
        }
        if response_error(&output).as_deref() == Some("agent_not_ready") {
            return Ok(AgentStart::NotReady);
        }
        Err(command_error(&output))
    }

    fn pane_cwd(&self, pane: &str) -> Result<Option<PathBuf>, ShepherdError> {
        let value = self.request(&["pane", "get", pane])?;
        Ok([
            "/result/pane/foreground_cwd",
            "/result/foreground_cwd",
            "/result/pane/cwd",
            "/result/cwd",
            "/result/pane/process_cwd",
        ]
        .into_iter()
        .find_map(|path| {
            value
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
        }))
    }
    fn pane_session_ref(&self, pane: &str) -> Result<Option<String>, ShepherdError> {
        let value = self.request(&["pane", "get", pane])?;
        Ok([
            "/result/pane/agent_session/value",
            "/result/pane/agent_session_ref",
            "/result/pane/session_ref",
            "/result/pane/agent_session/session_ref",
            "/result/agent_session_ref",
        ]
        .into_iter()
        .find_map(|path| {
            value
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }))
    }

    fn prompt_agent(
        &self,
        name: &str,
        text: &str,
        until_working: bool,
    ) -> Result<(), ShepherdError> {
        let mut args = vec!["agent", "prompt", name, text];
        if until_working {
            args.extend(["--wait", "--until", "working", "--timeout", "30000"]);
        }
        self.request(&args)
            .map(|_| ())
            .map_err(ShepherdError::into_prompt_error)
    }

    fn send_keys(&self, name: &str, keys: &[String]) -> Result<(), ShepherdError> {
        let mut args = vec!["agent", "send-keys", name];
        args.extend(keys.iter().map(String::as_str));
        self.request(&args).map(|_| ())
    }

    fn read_visible(&self, name: &str) -> Result<String, ShepherdError> {
        let output = self.capture_success(&["agent", "read", name, "--source", "visible"])?;
        // The installed CLI prints terminal text. Preserve it byte-for-byte,
        // while accepting JSON envelopes from compatible clients and fixtures.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&output)
            && let Some(text) = [
                "/result/text",
                "/result/output",
                "/result/screen",
                "/result/content",
            ]
            .into_iter()
            .find_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
        {
            return Ok(text.to_owned());
        }
        Ok(output)
    }

    fn agent_get(&self, name: &str) -> Result<Option<AgentInfo>, ShepherdError> {
        let output = self.capture(&["agent", "get", name])?;
        if !output.status.success() {
            if response_error(&output).is_some_and(|code| {
                matches!(
                    code.as_str(),
                    "agent_not_found" | "not_found" | "agent_target_not_found"
                )
            }) {
                return Ok(None);
            }
            return Err(command_error(&output));
        }
        let value: serde_json::Value =
            serde_json::from_str(&output.stdout).map_err(anyhow::Error::from)?;
        let agent = value
            .pointer("/result/agent")
            .filter(|value| value.is_object())
            .unwrap_or(&value["result"]);
        serde_json::from_value(agent.clone())
            .map(Some)
            .map_err(anyhow::Error::from)
            .map_err(Into::into)
    }

    fn close_tab(&self, tab: &str) -> Result<(), ShepherdError> {
        self.request(&["tab", "close", tab]).map(|_| ())
    }

    fn focus_agent(&self, name: &str) -> Result<(), ShepherdError> {
        self.request(&["agent", "focus", name]).map(|_| ())
    }

    fn report_workspace_tokens(
        &self,
        ws: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError> {
        metadata_request(self, "workspace", ws, set, clear)
    }

    fn report_pane_tokens(
        &self,
        pane: &str,
        set: &[(String, String)],
        clear: &[String],
    ) -> Result<(), ShepherdError> {
        metadata_request(self, "pane", pane, set, clear)
    }

    fn notify(&self, title: &str, body: &str) -> Result<(), ShepherdError> {
        self.command(&["notification", "show", title, "--body", body])
            .map(|_| ())
    }

    fn remove_worktree(&self, ws: &str, force: bool) -> Result<(), ShepherdError> {
        let mut args = vec!["worktree", "remove", "--workspace", ws];
        if force {
            args.push("--force");
        }
        self.command(&args).map(|_| ())
    }

    fn close_workspace(&self, ws: &str) -> Result<(), ShepherdError> {
        self.command(&["workspace", "close", ws]).map(|_| ())
    }

    fn workspaces(&self) -> Result<Vec<Workspace>, ShepherdError> {
        model::parse_snapshot(&self.command(&["api", "snapshot"])?).map_err(ShepherdError::Runtime)
    }

    fn agents(&self) -> Result<Vec<AgentInfo>, ShepherdError> {
        model::parse_agent_list(&self.command(&["agent", "list"])?).map_err(ShepherdError::Runtime)
    }

    fn rename_workspace(&self, id: &str, label: &str) -> Result<(), ShepherdError> {
        self.command(&["workspace", "rename", id, label])
            .map(|_| ())
    }

    fn move_block(&self, ids: &[String], before: Option<&str>) -> Result<(), ShepherdError> {
        socket::request(
            &self.socket,
            &json!({
                "id": "shepherd-move",
                "method": "workspace.move_block",
                "params": {"workspace_ids": ids, "before_workspace_id": before}
            }),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn prompt_errors_preserve_server_codes_only() {
        for (text, expected) in [
            (
                r#"{"error":{"code":"timeout","message":"wait expired"}}"#,
                Some("timeout"),
            ),
            ("connect timeout", None),
            (
                r#"{"error":{"code":"permission_denied","message":"timeout in text"}}"#,
                Some("permission_denied"),
            ),
        ] {
            let output = crate::proc::capture(
                Command::new("/bin/sh").args(["-c", "cat >&2; exit 1"]),
                Duration::from_secs(2),
                Some(text.as_bytes()),
            )
            .unwrap();
            let error = command_error(&output);
            assert_eq!(error.herdr_code(), expected);
            assert!(
                !error.prompt_submission_uncertain(),
                "generic commands do not imply prompt dispatch"
            );
        }
    }
    #[test]
    fn prompt_cli_preserves_argv_and_quarantines_unacknowledged_delivery() {
        for (response, exit, code, uncertain) in [
            (
                r#"{"result":{"agent":{"agent_status":"working"}}}"#,
                0,
                None,
                false,
            ),
            (
                r#"{"error":{"code":"timeout","message":"private prompt text"}}"#,
                1,
                Some("timeout"),
                true,
            ),
            (
                r#"{"error":{"code":"agent_prompt_stalled","message":"private prompt text"}}"#,
                1,
                Some("agent_prompt_stalled"),
                true,
            ),
            (
                r#"{"error":{"code":"agent_blocked","message":"private prompt text"}}"#,
                1,
                Some("agent_blocked"),
                false,
            ),
            (
                r#"{"error":{"code":"agent_not_ready","message":"private prompt text"}}"#,
                1,
                Some("agent_not_ready"),
                false,
            ),
            ("truncated acknowledgement", 0, None, true),
            ("connection lost after dispatch", 1, None, true),
        ] {
            let (dir, h, _listener) = output_fixture(response);
            let log = dir.path().join("argv");
            std::fs::write(&log, "").unwrap();
            std::fs::write(dir.path().join("exit-code"), format!("{exit}\n")).unwrap();
            let writers = retain_fixture_writers(dir.path());
            let result = h.prompt_agent("sh-fixture-researcher-1", "task with spaces", true);
            drop(writers);
            let diagnostic = format!("{result:?}");
            if exit == 0 && !uncertain {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.herdr_code(), code);
                assert_eq!(error.prompt_submission_uncertain(), uncertain);
                assert!(!error.to_string().contains("private prompt text"));
                assert!(!error.to_string().contains("task with spaces"));
            }
            assert_eq!(
                std::fs::read_to_string(log).expect(&diagnostic),
                "agent\nprompt\nsh-fixture-researcher-1\ntask with spaces\n--wait\n--until\nworking\n--timeout\n30000\n"
            );
        }
    }

    #[test]
    fn metadata_uses_socket_tokens_and_null_clears() {
        use std::io::{BufRead, BufReader, Write};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            for (method, id_key, id) in [
                ("workspace.report_metadata", "workspace_id", "w1"),
                ("pane.report_metadata", "pane_id", "p1"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], method);
                assert_eq!(request["params"][id_key], id);
                assert_eq!(request["params"]["source"], "shepherd");
                assert_eq!(
                    request["params"]["tokens"],
                    json!({"shepherd_pr":"open","shepherd_stage":null})
                );
                stream.write_all(b"{\"result\":{}}\n").unwrap();
            }
        });
        let h = CliHerdr {
            bin: dir.path().join("missing-herdr"),
            socket,
        };
        h.report_workspace_tokens(
            "w1",
            &[("shepherd_pr".into(), "open".into())],
            &["shepherd_stage".into()],
        )
        .unwrap();
        h.report_pane_tokens(
            "p1",
            &[("shepherd_pr".into(), "open".into())],
            &["shepherd_stage".into()],
        )
        .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn missing_binary_is_herdr_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let h = CliHerdr {
            bin: dir.path().join("missing-herdr"),
            socket: socket.clone(),
        };
        let err = h.workspaces().unwrap_err();
        assert!(matches!(err, ShepherdError::HerdrUnavailable(_)));
        assert!(err.to_string().contains(socket.to_str().unwrap()));
    }

    #[test]
    fn stale_socket_is_herdr_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        drop(UnixListener::bind(&socket).unwrap());
        let h = CliHerdr {
            bin: PathBuf::from("herdr"),
            socket: socket.clone(),
        };
        let err = h.workspaces().unwrap_err();
        assert!(matches!(err, ShepherdError::HerdrUnavailable(_)));
        assert!(err.to_string().contains(socket.to_str().unwrap()));
    }
    #[test]
    fn pane_helpers_use_foreground_and_session_value_schema() {
        let (_dir, h, _listener) = output_fixture(
            r#"{"result":{"pane":{"cwd":"/tmp/shell","foreground_cwd":"/tmp/foreground","agent_session":{"source":"integration","agent":"codex","kind":"thread","value":"fixture-session"}}}}"#,
        );
        assert_eq!(
            h.pane_cwd("owned:p1").unwrap().as_deref(),
            Some(std::path::Path::new("/tmp/foreground"))
        );
        assert_eq!(
            h.pane_session_ref("owned:p1").unwrap().as_deref(),
            Some("fixture-session")
        );
    }
    #[test]
    fn start_agent_accepts_silent_run_and_text_wait() {
        let (dir, h, _listener) = start_fixture(r#"{"result":{"agent":{"agent_status":"idle"}}}"#);
        let values = HashMap::from([(
            "GIT_CONFIG_GLOBAL".into(),
            dir.path()
                .join("agent.gitconfig")
                .to_string_lossy()
                .into_owned(),
        )]);
        let writers = retain_fixture_writers(dir.path());
        let result = h.start_agent(
            "sh-fixture-coder-1",
            "codex",
            "owned:p1",
            &["--yolo".into(), "-m".into(), "gpt-6.1-sol".into()],
            &values,
        );
        drop(writers);
        assert!(matches!(result, Ok(AgentStart::Ready)), "{result:?}");
    }

    #[test]
    fn start_agent_surfaces_unsupported_omp_overlay_without_retry() {
        let (dir, h, _listener) = start_fixture(r#"{"error":{"code":"unsupported_config"}}"#);
        let starts = dir.path().join("argv");
        std::fs::write(&starts, "").unwrap();
        std::fs::write(dir.path().join("exit-code"), "1\n").unwrap();
        let values = HashMap::from([(
            "GIT_CONFIG_GLOBAL".into(),
            dir.path()
                .join("agent.gitconfig")
                .to_string_lossy()
                .into_owned(),
        )]);
        let harness = crate::harness::harness_for("omp").unwrap();
        let args = harness
            .managed_args(vec!["--model".into(), "fixture/probe".into()], dir.path())
            .unwrap();
        let writers = retain_fixture_writers(dir.path());
        let error = h
            .start_agent("sh-fixture-coder-1", "omp", "owned:p1", &args, &values)
            .unwrap_err();
        drop(writers);
        assert_eq!(error.herdr_code(), Some("unsupported_config"));
        let sent = std::fs::read_to_string(starts).unwrap();
        assert_eq!(sent.lines().filter(|line| *line == "start").count(), 1);
        assert_eq!(sent.lines().filter(|line| *line == "--config").count(), 1);
        assert!(sent.ends_with(&format!("{}\n", args.last().unwrap())));
    }

    fn output_fixture(text: &str) -> (tempfile::TempDir, CliHerdr, UnixListener) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        // Never write executable inodes while parallel tests can fork. A child
        // can inherit a writer before CLOEXEC takes effect and cause ETXTBSY.
        let bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/herdr-output.sh");
        std::fs::write(dir.path().join("response"), text).unwrap();
        (dir, CliHerdr { bin, socket }, listener)
    }

    fn start_fixture(text: &str) -> (tempfile::TempDir, CliHerdr, UnixListener) {
        let (dir, mut h, listener) = output_fixture(text);
        h.bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/herdr-start.sh");
        (dir, h, listener)
    }

    fn retain_fixture_writers(dir: &Path) -> Vec<std::fs::File> {
        // Model the writable inode a concurrent fork can retain until exec,
        // even after the creating thread has closed its CLOEXEC descriptor.
        let writers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(entry.path())
                    .unwrap()
            })
            .collect();
        assert!(!writers.is_empty());
        writers
    }

    #[test]
    fn output_fixture_runs_with_retained_writers() {
        let screen = "Trust 'this folder'?\nTrust and continue\n";
        let (dir, h, _listener) = output_fixture(screen);
        let writers = retain_fixture_writers(dir.path());
        let result = h.read_visible("sh-fixture-coder-1");
        drop(writers);
        assert_eq!(result.unwrap(), screen);
    }

    #[test]
    fn read_visible_accepts_plain_terminal_text() {
        let screen = "Trust this folder?\nTrust and continue\n";
        let (_dir, h, _listener) = output_fixture(screen);
        assert_eq!(h.read_visible("sh-fixture-coder-1").unwrap(), screen);
    }

    #[test]
    fn read_visible_accepts_json_envelope() {
        let (_dir, h, _listener) =
            output_fixture(r#"{"result":{"text":"Trust this folder?\nTrust and continue\n"}}"#);
        assert_eq!(
            h.read_visible("sh-fixture-coder-1").unwrap(),
            "Trust this folder?\nTrust and continue\n"
        );
    }

    #[test]
    fn send_keys_accepts_empty_acknowledgement() {
        let (_dir, h, _listener) = output_fixture("");
        h.send_keys("sh-fixture-coder-1", &["enter".into()])
            .unwrap();
    }

    #[test]
    fn structured_request_rejects_malformed_nonempty_response() {
        let (_dir, h, _listener) = output_fixture("not JSON\n");
        assert!(h.pane_cwd("owned:p1").is_err());
    }
}
