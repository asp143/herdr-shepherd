use std::{process::Command, time::Duration};

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
    let output = crate::proc::run(&mut cmd, timeout)?
        .ok_or_else(|| anyhow::anyhow!("ticket source failed or exceeded its timeout"))?;
    serde_json::from_str(&output)
        .map_err(|err| ShepherdError::Usage(format!("invalid ticket source JSON: {err}")))
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
                matches!(fetch(&cfg, "SYM-7"), Err(ShepherdError::Usage(_))),
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
    fn source_failure_does_not_parse_stdout() {
        let cfg = config(&["sh", "-c", "printf '{}'; exit 7"]);
        assert_eq!(fetch(&cfg, "SYM-7").unwrap_err().code(), "runtime");
    }

    #[test]
    fn invalid_json_rejected() {
        let cfg = config(&["sh", "-c", "printf 'not json'"]);
        assert_eq!(fetch(&cfg, "SYM-7").unwrap_err().code(), "usage");
    }

    #[test]
    fn missing_source_executable_is_runtime_error() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent-source");
        let cfg = config(&[absent.to_str().unwrap()]);
        assert_eq!(fetch(&cfg, "SYM-7").unwrap_err().code(), "runtime");
    }
}
