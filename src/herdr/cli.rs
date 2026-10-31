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

    fn request(&self, args: &[&str]) -> Result<serde_json::Value, ShepherdError> {
        let output = self.capture(args)?;
        if !output.status.success() {
            return Err(command_error(&output));
        }
        serde_json::from_str(&output.stdout)
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
        self.request(&["pane", "run", pane, &script])?;
        self.request(&[
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
        self.request(&args).map(|_| ())
    }

    fn send_keys(&self, name: &str, keys: &[String]) -> Result<(), ShepherdError> {
        let mut args = vec!["agent", "send-keys", name];
        args.extend(keys.iter().map(String::as_str));
        self.request(&args).map(|_| ())
    }

    fn read_visible(&self, name: &str) -> Result<String, ShepherdError> {
        let value = self.request(&["agent", "read", name, "--source", "visible"])?;
        [
            "/result/text",
            "/result/output",
            "/result/screen",
            "/result/content",
        ]
        .into_iter()
        .find_map(|pointer| {
            value
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow::anyhow!("herdr read response lacks screen text").into())
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
            assert_eq!(error.prompt_was_submitted(), expected == Some("timeout"));
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
}
