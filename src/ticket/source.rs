use std::{
    io::Read,
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};

use crate::{config::Config, error::ShepherdError};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FetchedTicket {
    pub id: String,
    pub title: String,
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    pub url: Option<String>,
}

/// Run the configured source with the ticket ID as its final, literal argument.
pub fn fetch(cfg: &Config, id: &str) -> Result<FetchedTicket, ShepherdError> {
    fetch_with_timeout(cfg, id, Duration::from_secs(30))
}

fn fetch_with_timeout(
    cfg: &Config,
    id: &str,
    timeout: Duration,
) -> Result<FetchedTicket, ShepherdError> {
    let invalid = |message: &str| ShepherdError::Config {
        path: match &cfg.source {
            crate::config::ConfigSource::Global(path)
            | crate::config::ConfigSource::Project(path) => path.clone(),
            crate::config::ConfigSource::Default => "shepherd.toml".into(),
        },
        message: message.into(),
    };
    let source = cfg
        .raw
        .get("ticket_source")
        .ok_or(ShepherdError::NoTicketSource)?;
    let source = source
        .as_table()
        .ok_or_else(|| invalid("ticket_source must be a table"))?;
    let args = source.get("command").ok_or(ShepherdError::NoTicketSource)?;
    let args = args
        .as_array()
        .ok_or_else(|| invalid("ticket_source.command must be an argv array"))?;
    let args = args
        .iter()
        .map(|arg| {
            arg.as_str()
                .ok_or_else(|| invalid("ticket_source.command arguments must be strings"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some((program, arguments)) = args.split_first() else {
        return Err(ShepherdError::NoTicketSource);
    };
    if program.is_empty() {
        return Err(invalid(
            "ticket_source.command executable must not be empty",
        ));
    }
    let mut cmd = Command::new(program);
    cmd.args(arguments).arg(id);
    let output = run_source(&mut cmd, timeout)?;
    let ticket: FetchedTicket =
        serde_json::from_str(&output).context("invalid ticket source JSON")?;
    if ticket.id != id {
        return Err(
            anyhow::anyhow!("ticket source returned id {:?}, expected {id:?}", ticket.id).into(),
        );
    }
    if ticket.title.trim().is_empty() {
        return Err(anyhow::anyhow!("ticket source returned an empty title").into());
    }
    Ok(ticket)
}

struct SourceProcess {
    child: Child,
    group: Pid,
}

impl Drop for SourceProcess {
    fn drop(&mut self) {
        // The source may exit while descendants retain its stdout. Kill the
        // entire owned group before reaping, on timeout and other error paths.
        let _ = killpg(self.group, Signal::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Enforce one deadline for command execution and the complete stdout capture.
fn run_source(cmd: &mut Command, timeout: Duration) -> anyhow::Result<String> {
    let deadline = Instant::now() + timeout;
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("start ticket source")?;
    let group = Pid::from_raw(child.id().try_into().expect("child pid fits pid_t"));
    let mut process = SourceProcess { child, group };
    let stdout = process.child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let result = std::io::BufReader::new(stdout).read_to_string(&mut text);
        let _ = tx.send(result.map(|_| text));
    });
    let timeout_error =
        || anyhow::anyhow!("ticket source timeout after {} ms", timeout.as_millis());
    loop {
        if let Some(status) = process.child.try_wait().context("wait for ticket source")? {
            if !status.success() {
                anyhow::bail!("ticket source exited with non-zero status {status}");
            }
            return match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(result) => result.context("read ticket source stdout"),
                Err(mpsc::RecvTimeoutError::Timeout) => Err(timeout_error()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("ticket source stdout reader disconnected")
                }
            };
        }
        if Instant::now() >= deadline {
            return Err(timeout_error());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(command: &[&str]) -> crate::config::Config {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::load(dir.path(), None).unwrap();
        cfg.raw.insert(
            "ticket_source".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "command".into(),
                toml::Value::Array(
                    command
                        .iter()
                        .map(|arg| toml::Value::String((*arg).into()))
                        .collect(),
                ),
            )])),
        );
        cfg
    }

    #[test]
    fn fetch_parses_json() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("source.sh");
        std::fs::write(&script, r#"printf '%s\n' '{"id":"SYM-7","title":"Fix route","description":"Scope receive","acceptance_criteria":["Keep lots separate"],"url":"https://tickets.example/SYM-7"}'
"#).unwrap();
        let cfg = config(&["sh", script.to_str().unwrap()]);
        let ticket = fetch(&cfg, "SYM-7").unwrap();
        assert_eq!(ticket.id, "SYM-7");
        assert_eq!(ticket.title, "Fix route");
        assert_eq!(ticket.description, "Scope receive");
        assert_eq!(ticket.acceptance_criteria, ["Keep lots separate"]);
        assert_eq!(ticket.url.as_deref(), Some("https://tickets.example/SYM-7"));
    }

    #[test]
    fn empty_command_no_ticket_source() {
        let cfg = config(&[]);
        let error = fetch(&cfg, "SYM-7").unwrap_err();
        assert_eq!(error.code(), "no_ticket_source");
        assert_eq!(error.exit_code(), 2);
        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::load(dir.path(), None).unwrap();
        assert_eq!(fetch(&cfg, "SYM-7").unwrap_err().code(), "no_ticket_source");
    }

    #[test]
    fn malformed_command_rejected_as_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shepherd.toml");
        for source in [
            "ticket_source = []",
            "[ticket_source]\ncommand = 'sh'",
            "[ticket_source]\ncommand = [1]",
            "[ticket_source]\ncommand = ['']",
        ] {
            std::fs::write(&path, source).unwrap();
            let cfg = crate::config::load(dir.path(), None).unwrap();
            let error = fetch(&cfg, "SYM-7").unwrap_err();
            assert!(matches!(error, ShepherdError::Config { path: actual, .. } if actual == path));
        }
    }

    #[test]
    fn missing_fields_rejected() {
        let complete = serde_json::json!({
            "id": "SYM-7", "title": "Fix route", "description": "Scope receive",
            "acceptance_criteria": ["Keep lots separate"],
        });
        for field in ["id", "title", "description", "acceptance_criteria"] {
            let mut incomplete = complete.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            let fixture = incomplete.to_string();
            let cfg = config(&["sh", "-c", "printf '%s' \"$1\"", "source", &fixture]);
            assert!(
                matches!(fetch(&cfg, "SYM-7"), Err(ShepherdError::Runtime(_))),
                "{field}"
            );
        }
    }

    #[test]
    fn id_is_appended_literally_and_url_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("source.sh");
        std::fs::write(
            &script,
            r#"test "$#" -eq 2 || exit 1
test "$1" = '--fixed' || exit 1
printf '{"id":"%s","title":"Literal argv","description":"Fetched","acceptance_criteria":[]}' "$2"
"#,
        )
        .unwrap();
        let cfg = config(&["sh", script.to_str().unwrap(), "--fixed"]);
        let ticket = fetch(&cfg, "SYM-7; false").unwrap();
        assert_eq!(ticket.id, "SYM-7; false");
        assert_eq!(ticket.url, None);
    }

    #[test]
    fn timeout_errors() {
        let cfg = config(&["sh", "-c", "exec sleep 1"]);
        let started = std::time::Instant::now();
        let err = fetch_with_timeout(&cfg, "SYM-7", Duration::from_millis(40)).unwrap_err();
        assert_eq!(err.code(), "runtime");
        assert!(err.to_string().contains("timeout"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_kills_descendant_processes() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-survived");
        let cfg = config(&[
            "sh",
            "-c",
            "(sleep 0.2; printf alive > \"$1\") & wait",
            "source",
            marker.to_str().unwrap(),
        ]);
        let error = fetch_with_timeout(&cfg, "SYM-7", Duration::from_millis(60)).unwrap_err();
        assert!(error.to_string().contains("timeout"));
        thread::sleep(Duration::from_millis(250));
        assert!(!marker.exists(), "descendant survived the source timeout");
    }

    #[test]
    fn timeout_covers_stdout_after_source_exits() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-survived");
        let cfg = config(&[
            "sh",
            "-c",
            "(sleep 0.2; printf alive > \"$1\") & exit 0",
            "source",
            marker.to_str().unwrap(),
        ]);
        let started = Instant::now();
        let error = fetch_with_timeout(&cfg, "SYM-7", Duration::from_millis(60)).unwrap_err();
        assert!(error.to_string().contains("timeout"));
        assert!(started.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(250));
        assert!(
            !marker.exists(),
            "descendant retained stdout after the source exited"
        );
    }

    #[test]
    fn nonzero_exit_cleans_up_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-survived");
        let cfg = config(&[
            "sh",
            "-c",
            "(sleep 0.2; printf alive > \"$1\") & exit 7",
            "source",
            marker.to_str().unwrap(),
        ]);
        let error = fetch_with_timeout(&cfg, "SYM-7", Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("non-zero"));
        thread::sleep(Duration::from_millis(250));
        assert!(
            !marker.exists(),
            "descendant survived failed source execution"
        );
    }

    #[test]
    fn source_failure_does_not_parse_stdout() {
        let cfg = config(&["sh", "-c", "printf '{}'; exit 7"]);
        let error = fetch(&cfg, "SYM-7").unwrap_err();
        assert_eq!(error.code(), "runtime");
        assert!(error.to_string().contains("non-zero"));
        assert!(error.to_string().contains('7'));
        assert!(!error.to_string().contains("timeout"));
    }

    #[test]
    fn invalid_json_rejected() {
        let cfg = config(&["sh", "-c", "printf 'not json'"]);
        let error = fetch(&cfg, "SYM-7").unwrap_err();
        assert_eq!(error.code(), "runtime");
        assert_eq!(error.exit_code(), 1);
    }

    #[test]
    fn mismatched_id_and_blank_title_rejected() {
        for (id, title) in [("SYM-8", "Valid title"), ("SYM-7", ""), ("SYM-7", " \t")] {
            let fixture = serde_json::json!({
                "id": id, "title": title, "description": "Ticket text",
                "acceptance_criteria": [],
            })
            .to_string();
            let cfg = config(&["sh", "-c", "printf '%s' \"$1\"", "source", &fixture]);
            let error = fetch(&cfg, "SYM-7").unwrap_err();
            assert_eq!(error.code(), "runtime");
            assert_eq!(error.exit_code(), 1);
            assert!(
                error
                    .to_string()
                    .contains(if id != "SYM-7" { "id" } else { "title" })
            );
        }
    }

    #[test]
    fn missing_source_executable_is_runtime_error() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent-source");
        let cfg = config(&[absent.to_str().unwrap()]);
        assert_eq!(fetch(&cfg, "SYM-7").unwrap_err().code(), "runtime");
    }
}
