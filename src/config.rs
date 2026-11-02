use std::{
    env, fs,
    io::{ErrorKind, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::error::ShepherdError;

#[derive(Debug, Serialize)]
pub struct Config {
    pub schema_version: u32,
    pub enabled: bool,
    pub upkeep: Upkeep,
    pub source: ConfigSource,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pub raw: toml::Table,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upkeep {
    pub regroup: bool,
    pub pr_status: bool,
    pub notifications: bool,
    pub model_badge: bool,
    pub keybindings: bool,
    pub token_prefix: String,
}

impl Default for Upkeep {
    fn default() -> Self {
        Self {
            regroup: true,
            pr_status: true,
            notifications: true,
            model_badge: true,
            keybindings: true,
            token_prefix: "shepherd_".into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    Default,
    Global(PathBuf),
    Project(PathBuf),
}

pub(crate) fn known_hook_event(event: &str) -> bool {
    matches!(
        event,
        "ticket.queued"
            | "ticket.started"
            | "ticket.stage_changed"
            | "ticket.blocked"
            | "ticket.pr_opened"
            | "ticket.pr_comment"
            | "ticket.revised"
            | "ticket.merged"
            | "ticket.cleaned_up"
            | "ticket.closed"
            | "agent.spawned"
            | "agent.reported"
            | "agent.retired"
            | "agent.unblocked"
            | "agent.escalated"
    )
}

pub fn resolve_config_dir(cli: Option<&Path>) -> PathBuf {
    cli.map(Path::to_path_buf)
        .or_else(|| env::var_os("HERDR_PLUGIN_CONFIG_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config/herdr/plugins/config/herdr-shepherd")
        })
}

fn invalid(path: &Path, message: impl ToString) -> ShepherdError {
    ShepherdError::Config {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

fn read_table(path: &Path) -> Result<Option<toml::Table>, ShepherdError> {
    match fs::read_to_string(path) {
        Ok(text) => text.parse().map(Some).map_err(|err| invalid(path, err)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(invalid(path, err)),
    }
}

fn read_project_table(path: &Path) -> Result<Option<toml::Table>, ShepherdError> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(invalid(path, err)),
    };
    if !file.metadata().map_err(|err| invalid(path, err))?.is_file() {
        return Err(invalid(path, "project config must be a regular file"));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| invalid(path, err))?;
    text.parse().map(Some).map_err(|err| invalid(path, err))
}

pub fn load(config_dir: &Path, project_root: Option<&Path>) -> Result<Config, ShepherdError> {
    let global_path = config_dir.join("shepherd.toml");
    let global = read_table(&global_path)?;
    let project_path = project_root.map(|root| root.join(".shepherd.toml"));
    let project = project_path
        .as_deref()
        .map(read_project_table)
        .transpose()?
        .flatten();
    from_tables(global_path, global, project_path, project)
}

/// Parse only a project configuration whose bytes came from a trusted commit.
pub(crate) fn load_committed(
    config_dir: &Path,
    project_path: &Path,
    text: Option<&str>,
) -> Result<Config, ShepherdError> {
    let global_path = config_dir.join("shepherd.toml");
    let global = read_table(&global_path)?;
    let project = text
        .map(|text| {
            text.parse::<toml::Table>()
                .map_err(|err| invalid(project_path, err))
        })
        .transpose()?;
    from_tables(global_path, global, Some(project_path.into()), project)
}

fn from_tables(
    global_path: PathBuf,
    global: Option<toml::Table>,
    project_path: Option<PathBuf>,
    project: Option<toml::Table>,
) -> Result<Config, ShepherdError> {
    if let Some(table) = &global
        && table
            .get("schema_version")
            .is_some_and(|value| !matches!(value, toml::Value::Integer(1)))
    {
        return Err(invalid(&global_path, "schema_version must be 1"));
    }
    let upkeep = match global.as_ref().and_then(|table| table.get("upkeep")) {
        Some(value) => value
            .clone()
            .try_into()
            .map_err(|err| invalid(&global_path, err))?,
        None => Upkeep::default(),
    };
    let (raw, source, path) = match (project, global) {
        (Some(project), _) => {
            let path = project_path.expect("project table has a project path");
            (project, ConfigSource::Project(path.clone()), path)
        }
        (None, Some(global)) => (
            global,
            ConfigSource::Global(global_path.clone()),
            global_path,
        ),
        (None, None) => (toml::Table::new(), ConfigSource::Default, global_path),
    };
    let mut warnings = Vec::new();
    if matches!(source, ConfigSource::Project(_)) && raw.contains_key("upkeep") {
        warnings.push("[upkeep] in project config is ignored".into());
    }
    if let Some(hooks) = raw.get("hooks").and_then(toml::Value::as_array) {
        for hook in hooks {
            if let Some(event) = hook.get("on").and_then(toml::Value::as_str)
                && !known_hook_event(event)
            {
                warnings.push(format!("unknown lifecycle hook event: {event}"));
            }
        }
    }
    let schema_version = match raw.get("schema_version") {
        None => 1,
        Some(toml::Value::Integer(1)) => 1,
        Some(_) => return Err(invalid(&path, "schema_version must be 1")),
    };
    let enabled = match raw.get("enabled") {
        None => true,
        Some(toml::Value::Boolean(enabled)) => *enabled,
        Some(_) => return Err(invalid(&path, "enabled must be a boolean")),
    };
    Ok(Config {
        schema_version,
        enabled,
        upkeep,
        source,
        warnings,
        raw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_event_warns() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("shepherd.toml"),
            "[[hooks]]\non = 'ticket.typo'\ncommand = ['true']\n",
        )
        .unwrap();
        let cfg = load(dir.path(), None).unwrap();
        assert!(
            cfg.warnings
                .iter()
                .any(|warning| warning.contains("ticket.typo"))
        );
    }

    #[test]
    fn defaults_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load(dir.path(), None).unwrap();
        assert_eq!(cfg.source, ConfigSource::Default);
        assert!(cfg.upkeep.regroup);
        assert_eq!(cfg.upkeep.token_prefix, "shepherd_");
        assert_eq!(
            crate::roles::resolve(&cfg).unwrap().shepherd.runner,
            "claude-opus"
        );
    }

    #[test]
    fn project_overrides_global_in_full() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            global.path().join("shepherd.toml"),
            "enabled = false\n[roles.coder]\ndescription = 'global'\n",
        )
        .unwrap();
        std::fs::write(project.path().join(".shepherd.toml"), "enabled = true\n").unwrap();
        let cfg = load(global.path(), Some(project.path())).unwrap();
        assert!(cfg.enabled);
        assert_eq!(
            cfg.source,
            ConfigSource::Project(project.path().join(".shepherd.toml"))
        );
        assert!(!cfg.raw.contains_key("roles"));
    }

    #[test]
    fn upkeep_only_from_global() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            global.path().join("shepherd.toml"),
            "[upkeep]\nregroup = false\n",
        )
        .unwrap();
        std::fs::write(
            project.path().join(".shepherd.toml"),
            "[upkeep]\nregroup = true\n",
        )
        .unwrap();
        let cfg = load(global.path(), Some(project.path())).unwrap();
        assert!(!cfg.upkeep.regroup);
        assert_eq!(cfg.warnings, ["[upkeep] in project config is ignored"]);
    }

    #[test]
    fn invalid_project_toml_is_config_error() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join(".shepherd.toml"), "[invalid").unwrap();
        let err = load(global.path(), Some(project.path())).unwrap_err();
        assert!(
            matches!(err, ShepherdError::Config { path, .. } if path.ends_with(".shepherd.toml"))
        );
    }

    #[test]
    fn wrong_schema_version_rejected() {
        let global = tempfile::tempdir().unwrap();
        std::fs::write(global.path().join("shepherd.toml"), "schema_version = 2\n").unwrap();
        assert!(matches!(
            load(global.path(), None),
            Err(ShepherdError::Config { .. })
        ));
    }

    #[test]
    fn unknown_sections_preserved() {
        let global = tempfile::tempdir().unwrap();
        std::fs::write(
            global.path().join("shepherd.toml"),
            "[roles.coder]\ndescription = 'x'\n",
        )
        .unwrap();
        let cfg = load(global.path(), None).unwrap();
        assert_eq!(cfg.raw["roles"]["coder"]["description"].as_str(), Some("x"));
    }
    #[test]
    fn unknown_upkeep_field_rejected() {
        let global = tempfile::tempdir().unwrap();
        fs::write(
            global.path().join("shepherd.toml"),
            "[upkeep]\nnotifictions = false\n",
        )
        .unwrap();
        assert!(matches!(
            load(global.path(), None),
            Err(ShepherdError::Config { .. })
        ));
    }
    #[test]
    fn global_schema_validated_when_project_overrides() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(global.path().join("shepherd.toml"), "schema_version = 2\n").unwrap();
        fs::write(
            project.path().join(".shepherd.toml"),
            "schema_version = 1\n",
        )
        .unwrap();
        assert!(matches!(
            load(global.path(), Some(project.path())),
            Err(ShepherdError::Config { .. })
        ));
    }
}
