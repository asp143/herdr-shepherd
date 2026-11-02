//! Resolve orchestration settings from the selected Phase 1 configuration.

use std::collections::HashSet;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize, Serializer};

use crate::{
    config::{Config, ConfigSource},
    error::ShepherdError,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShepherdSettings {
    pub runner: String,
    pub max_concurrent_tickets: usize,
    pub max_parallel_agents: usize,
    pub max_revision_rounds: u32,
    pub max_completed_assignments: usize,
    pub pr_poll_seconds: u64,
    pub integration_branch: String,
    pub protected_branches: Vec<String>,
}

impl Default for ShepherdSettings {
    fn default() -> Self {
        Self {
            runner: "claude-opus".into(),
            max_concurrent_tickets: 2,
            max_parallel_agents: 6,
            max_revision_rounds: 3,
            max_completed_assignments: 5,
            pr_poll_seconds: 180,
            integration_branch: "auto".into(),
            protected_branches: [
                "main",
                "master",
                "development",
                "develop",
                "production",
                "release-*",
                "sync/*",
            ]
            .map(str::to_owned)
            .into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeMode {
    #[default]
    Ticket,
    Child,
    None,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Role {
    pub name: String,
    pub description: String,
    pub runners: Vec<String>,
    pub worktree: WorktreeMode,
    pub stage: Option<String>,
    pub prompt: Option<String>,
}

impl Default for Role {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            runners: vec!["codex-sol-high".into()],
            worktree: WorktreeMode::Ticket,
            stage: None,
            prompt: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    #[serde(default)]
    pub name: String,
    pub harness: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SupervisionSettings {
    pub sweep_seconds: u64,
    pub report_grace_seconds: u64,
    pub stall_minutes: u64,
    pub escalate_minutes: u64,
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Rule {
    pub name: String,
    pub harness: Option<String>,
    #[serde(serialize_with = "serialize_pattern")]
    pub pattern: Regex,
    pub keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfig {
    name: String,
    harness: Option<String>,
    pattern: String,
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct SupervisionConfig {
    sweep_seconds: u64,
    report_grace_seconds: u64,
    stall_minutes: u64,
    escalate_minutes: u64,
    rules: Vec<RuleConfig>,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            sweep_seconds: 30,
            report_grace_seconds: 120,
            stall_minutes: 20,
            escalate_minutes: 15,
            rules: Vec::new(),
        }
    }
}

fn serialize_pattern<S: Serializer>(pattern: &Regex, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(pattern.as_str())
}

#[derive(Clone, Debug, Serialize)]
pub struct Resolved {
    pub shepherd: ShepherdSettings,
    pub roles: Vec<Role>,
    pub runners: IndexMap<String, Runner>,
    pub supervision: SupervisionSettings,
}

fn defaults() -> Resolved {
    let roles = [
        ("planner", "Breaks the ticket into independent units with dependencies, risks, and acceptance checks", "codex-sol-high", WorktreeMode::Ticket),
        ("researcher", "Investigates the codebase, docs, and ticket before implementation", "omp-astra", WorktreeMode::Ticket),
        ("coder", "Implements one unit and commits on its branch", "codex-sol-high", WorktreeMode::Child),
        ("reviewer", "Reviews the full ticket diff; labels findings High/Mid/Low/Wish", "omp-astra", WorktreeMode::Ticket),
        ("qa", "Runs tests and checks every acceptance criterion against the final code", "codex-sol-high", WorktreeMode::Ticket),
    ].into_iter().map(|(name, description, runner, worktree)| Role {
        name: name.into(), description: description.into(), runners: vec![runner.into()], worktree, stage: None, prompt: None,
    }).collect();
    let runners = [
        (
            "claude-opus",
            "claude",
            vec![
                "--model",
                "claude-opus-5-5",
                "--dangerously-skip-permissions",
            ],
        ),
        (
            "codex-sol-high",
            "codex",
            vec![
                "--yolo",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=high",
            ],
        ),
        (
            "omp-astra",
            "omp",
            vec![
                "--auto-approve",
                "--model",
                "openai-codex/gpt-6-astra:medium",
                "--smol",
                "openai-codex/gpt-6-luna:max",
            ],
        ),
        (
            "omp-glm-infra",
            "omp",
            vec![
                "--auto-approve",
                "--model",
                "ollama/glm-5.3-flash:cloud:medium",
            ],
        ),
    ]
    .into_iter()
    .map(|(name, harness, args)| {
        (
            name.into(),
            Runner {
                name: name.into(),
                harness: harness.into(),
                args: args.into_iter().map(str::to_owned).collect(),
            },
        )
    })
    .collect();
    Resolved {
        shepherd: ShepherdSettings::default(),
        roles,
        runners,
        supervision: SupervisionSettings {
            sweep_seconds: 30,
            report_grace_seconds: 120,
            stall_minutes: 20,
            escalate_minutes: 15,
            rules: Vec::new(),
        },
    }
}

fn invalid(cfg: &Config, message: impl ToString) -> ShepherdError {
    let path = match &cfg.source {
        ConfigSource::Global(path) | ConfigSource::Project(path) => path.clone(),
        ConfigSource::Default => "shepherd.toml".into(),
    };
    ShepherdError::Config {
        path,
        message: message.to_string(),
    }
}

fn overlay<T: Serialize + serde::de::DeserializeOwned>(
    cfg: &Config,
    base: &T,
    value: &toml::Value,
) -> Result<T, ShepherdError> {
    let extra = value
        .as_table()
        .ok_or_else(|| invalid(cfg, "settings must be a table"))?;
    let mut table = toml::Value::try_from(base)
        .map_err(|err| invalid(cfg, err))?
        .as_table()
        .cloned()
        .expect("settings serialize to a table");
    table.extend(extra.clone());
    toml::Value::Table(table)
        .try_into()
        .map_err(|err| invalid(cfg, err))
}

/// Overlay named roles and runners on the built-in catalog in the selected file.
/// New roles default to the ticket worktree and the primary codex runner.
/// Configured entries retain file order, followed by unspecified built-ins.
pub fn resolve(cfg: &Config) -> Result<Resolved, ShepherdError> {
    let mut resolved = defaults();
    if let Some(value) = cfg.raw.get("shepherd") {
        resolved.shepherd = value.clone().try_into().map_err(|err| invalid(cfg, err))?;
    }
    if let Some(value) = cfg.raw.get("runners") {
        let table = value
            .as_table()
            .ok_or_else(|| invalid(cfg, "runners must be a table"))?;
        let mut runners = IndexMap::new();
        for (name, value) in table {
            let mut runner: Runner = match resolved.runners.get(name) {
                Some(base) => overlay(cfg, base, value)?,
                None => value.clone().try_into().map_err(|err| invalid(cfg, err))?,
            };
            runner.name = name.clone();
            runners.insert(name.clone(), runner);
        }
        for (name, runner) in resolved.runners {
            runners.entry(name).or_insert(runner);
        }
        resolved.runners = runners;
    }
    if let Some(value) = cfg.raw.get("roles") {
        let table = value
            .as_table()
            .ok_or_else(|| invalid(cfg, "roles must be a table"))?;
        let mut roles = Vec::new();
        for (name, value) in table {
            let base = resolved
                .roles
                .iter()
                .find(|role| role.name == *name)
                .cloned()
                .unwrap_or_default();
            let mut role: Role = overlay(cfg, &base, value)?;
            role.name = name.clone();
            roles.push(role);
        }
        roles.extend(
            resolved
                .roles
                .into_iter()
                .filter(|role| !table.contains_key(&role.name)),
        );
        resolved.roles = roles;
    }
    if let Some(value) = cfg.raw.get("supervision") {
        let settings: SupervisionConfig =
            value.clone().try_into().map_err(|err| invalid(cfg, err))?;
        let mut rules = Vec::new();
        let mut rule_names: HashSet<String> = [
            "codex-trust",
            "press-enter",
            "codex-update-notice",
            "claude-trust",
            "claude-bypass-permissions",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for rule in settings.rules {
            if !rule_names.insert(rule.name.clone()) {
                return Err(invalid(
                    cfg,
                    format!("duplicate supervision rule name {}", rule.name),
                ));
            }
            if rule.pattern.trim().is_empty() {
                return Err(invalid(
                    cfg,
                    format!("rule {} pattern must not be empty", rule.name),
                ));
            }
            if rule.keys.is_empty() {
                return Err(invalid(
                    cfg,
                    format!("rule {} keys must not be empty", rule.name),
                ));
            }
            if let Some(harness) = &rule.harness
                && !matches!(harness.as_str(), "codex" | "omp" | "claude" | "pi")
            {
                return Err(invalid(
                    cfg,
                    format!("rule {} has unknown harness {harness}", rule.name),
                ));
            }
            for key in &rule.keys {
                if !matches!(
                    key.as_str(),
                    "enter"
                        | "esc"
                        | "y"
                        | "n"
                        | "1"
                        | "2"
                        | "3"
                        | "4"
                        | "5"
                        | "6"
                        | "7"
                        | "8"
                        | "9"
                        | "down"
                        | "up"
                ) {
                    return Err(invalid(
                        cfg,
                        format!("rule {} uses invalid key {key}", rule.name),
                    ));
                }
            }
            let pattern = Regex::new(&rule.pattern)
                .map_err(|err| invalid(cfg, format!("rule {}: {err}", rule.name)))?;
            rules.push(Rule {
                name: rule.name,
                harness: rule.harness,
                pattern,
                keys: rule.keys,
            });
        }
        resolved.supervision = SupervisionSettings {
            sweep_seconds: settings.sweep_seconds,
            report_grace_seconds: settings.report_grace_seconds,
            stall_minutes: settings.stall_minutes,
            escalate_minutes: settings.escalate_minutes,
            rules,
        };
    }
    for role in &resolved.roles {
        if role.runners.is_empty() {
            return Err(invalid(
                cfg,
                format!("role {} must specify at least one runner", role.name),
            ));
        }
        for runner in &role.runners {
            if !resolved.runners.contains_key(runner) {
                return Err(invalid(
                    cfg,
                    format!("role {} refers to unknown runner {runner}", role.name),
                ));
            }
        }
    }
    if !resolved.runners.contains_key(&resolved.shepherd.runner) {
        return Err(invalid(
            cfg,
            format!(
                "shepherd refers to unknown runner {}",
                resolved.shepherd.runner
            ),
        ));
    }
    for (name, zero) in [
        (
            "max_concurrent_tickets",
            resolved.shepherd.max_concurrent_tickets == 0,
        ),
        (
            "max_parallel_agents",
            resolved.shepherd.max_parallel_agents == 0,
        ),
        (
            "max_revision_rounds",
            resolved.shepherd.max_revision_rounds == 0,
        ),
        (
            "max_completed_assignments",
            resolved.shepherd.max_completed_assignments == 0,
        ),
    ] {
        if zero {
            return Err(invalid(cfg, format!("shepherd.{name} must be at least 1")));
        }
    }
    resolved.supervision.rules.extend(
        [
            (
                "codex-trust",
                Some("codex"),
                r"(?s)Trust this folder\?.*Trust and continue",
                &["enter"][..],
            ),
            ("press-enter", None, r"(?i)press enter to continue", &["enter"][..]),
            (
                "codex-update-notice",
                Some("codex"),
                r"(?i)update available.*(skip|later)",
                &["esc"][..],
            ),
            // Claude Code 2.1.316 presents the cancel option first and focused.
            // Require that selection explicitly before moving to confirmation.
            // Unrecognized layouts remain blocked for manual handling.
            (
                "claude-trust",
                Some("claude"),
                concat!(
                    r"(?m)^[ \t]*Accessing workspace:[ \t]*\r?$",
                    r"[\s\S]{0,1500}?^[ \t]*Quick\s+safety\s+check:\s+Is\s+this\s+a\s+project\s+you\s+created\s+or\s+one\s+you\s+trust\?",
                    r"[\s\S]{0,1500}?^[ \t]*Claude Code'll be able to read, edit, and execute files here\.[ \t]*\r?$",
                    r"[\s\S]{0,1500}?^[ \t]*❯[ \t]+No, exit[ \t]*\r?\n",
                    r"[ \t]+Yes, I trust this folder[ \t]*\r?$",
                ),
                &["down", "enter"][..],
            ),
            (
                "claude-bypass-permissions",
                Some("claude"),
                concat!(
                    r"(?m)^[ \t]*WARNING: Claude Code running in Bypass Permissions mode[ \t]*\r?$",
                    r"[\s\S]{0,1500}?^[ \t]*By\s+proceeding,\s+you\s+accept\s+all\s+responsibility\s+for\s+actions\s+taken\s+while\s+running\s+in\s+Bypass\s+Permissions\s+mode\.[ \t]*\r?$",
                    r"[\s\S]{0,1500}?^[ \t]*❯[ \t]+No, exit[ \t]*\r?\n",
                    r"[ \t]+Yes, I accept[ \t]*\r?$",
                ),
                &["down", "enter"][..],
            ),
        ]
        .into_iter()
        .map(|(name, harness, pattern, keys)| Rule {
            name: name.into(),
            harness: harness.map(str::to_owned),
            pattern: Regex::new(pattern).expect("built-in pattern is valid"),
            keys: keys.iter().map(|key| (*key).into()).collect(),
        }),
    );
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config {
            schema_version: 1,
            enabled: true,
            upkeep: Default::default(),
            source: ConfigSource::Global("/tmp/shepherd.toml".into()),
            warnings: Vec::new(),
            raw: text.parse().unwrap(),
        }
    }

    #[test]
    fn defaults_match_spec() {
        let resolved = resolve(&config("")).unwrap();
        assert_eq!(resolved.shepherd.runner, "claude-opus");
        assert_eq!(resolved.shepherd.max_concurrent_tickets, 2);
        assert_eq!(resolved.shepherd.max_parallel_agents, 6);
        assert_eq!(resolved.shepherd.max_revision_rounds, 3);
        assert_eq!(resolved.shepherd.pr_poll_seconds, 180);
        assert_eq!(resolved.shepherd.integration_branch, "auto");
        assert_eq!(
            resolved.shepherd.protected_branches,
            [
                "main",
                "master",
                "development",
                "develop",
                "production",
                "release-*",
                "sync/*"
            ]
        );
        let expected = [
            (
                "planner",
                "codex-sol-high",
                WorktreeMode::Ticket,
                "Breaks the ticket into independent units with dependencies, risks, and acceptance checks",
            ),
            (
                "researcher",
                "omp-astra",
                WorktreeMode::Ticket,
                "Investigates the codebase, docs, and ticket before implementation",
            ),
            (
                "coder",
                "codex-sol-high",
                WorktreeMode::Child,
                "Implements one unit and commits on its branch",
            ),
            (
                "reviewer",
                "omp-astra",
                WorktreeMode::Ticket,
                "Reviews the full ticket diff; labels findings High/Mid/Low/Wish",
            ),
            (
                "qa",
                "codex-sol-high",
                WorktreeMode::Ticket,
                "Runs tests and checks every acceptance criterion against the final code",
            ),
        ];
        assert_eq!(resolved.roles.len(), 5);
        for (name, runner, mode, description) in expected {
            let role = resolved.roles.iter().find(|r| r.name == name).unwrap();
            assert_eq!(role.runners, [runner]);
            assert_eq!(role.worktree, mode);
            assert_eq!(role.description, description);
            assert_eq!(role.stage, None);
            assert_eq!(role.prompt, None);
        }
        assert_eq!(
            resolved
                .runners
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                "claude-opus",
                "codex-sol-high",
                "omp-astra",
                "omp-glm-infra"
            ]
        );
        let claude = &resolved.runners["claude-opus"];
        assert_eq!(claude.harness, "claude");
        assert_eq!(
            claude.args,
            [
                "--model",
                "claude-opus-5-5",
                "--dangerously-skip-permissions"
            ]
        );
        let codex = &resolved.runners["codex-sol-high"];
        assert_eq!(codex.harness, "codex");
        assert_eq!(
            codex.args,
            [
                "--yolo",
                "-m",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=high"
            ]
        );
        assert_eq!(
            resolved.runners["omp-astra"].args,
            [
                "--auto-approve",
                "--model",
                "openai-codex/gpt-6-astra:medium",
                "--smol",
                "openai-codex/gpt-6-luna:max"
            ]
        );
        assert_eq!(
            resolved.runners["omp-glm-infra"].args,
            [
                "--auto-approve",
                "--model",
                "ollama/glm-5.3-flash:cloud:medium"
            ]
        );
        assert_eq!(resolved.supervision.sweep_seconds, 30);
        assert_eq!(resolved.supervision.report_grace_seconds, 120);
        assert_eq!(resolved.supervision.stall_minutes, 20);
        assert_eq!(resolved.supervision.escalate_minutes, 15);
    }

    #[test]
    fn init_template_matches_resolved_defaults() {
        let defaults = resolve(&config("")).unwrap();
        let template = resolve(&config(crate::lead::DEFAULT_CONFIG)).unwrap();
        assert_eq!(
            serde_json::to_value(&template).unwrap(),
            serde_json::to_value(&defaults).unwrap()
        );
        assert_eq!(
            template.runners.keys().collect::<Vec<_>>(),
            defaults.runners.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn unknown_runner_rejected() {
        for text in [
            "[roles.coder]\nrunners = ['missing']",
            "[shepherd]\nrunner = 'missing'",
        ] {
            let error = resolve(&config(text)).unwrap_err();
            assert_eq!(error.code(), "config_invalid");
            assert!(error.to_string().contains("missing"));
            assert!(error.to_string().contains("/tmp/shepherd.toml"));
        }
    }

    #[test]
    fn bad_rule_key_rejected() {
        let error = resolve(&config(
            "[[supervision.rules]]\nname='unsafe'\npattern='continue'\nkeys=['rm -rf']",
        ))
        .unwrap_err();
        assert_eq!(error.code(), "config_invalid");
        assert!(error.to_string().contains("rm -rf"));
    }

    #[test]
    fn user_role_added() {
        let resolved = resolve(&config(
            "[roles.security-reviewer]\nstage = 'review'\nprompt = 'Check permissions'",
        ))
        .unwrap();
        let role = resolved
            .roles
            .iter()
            .find(|r| r.name == "security-reviewer")
            .unwrap();
        assert_eq!(role.stage.as_deref(), Some("review"));
        assert_eq!(role.prompt.as_deref(), Some("Check permissions"));
        assert_eq!(role.runners, ["codex-sol-high"]);
        assert_eq!(role.worktree, WorktreeMode::Ticket);
        assert_eq!(resolved.roles.len(), 6);
    }

    #[test]
    fn builtin_rules_after_user_rules() {
        let resolved = resolve(&config(
            "[[supervision.rules]]\nname = 'custom'\npattern = 'Pick one'\nkeys = ['1', 'enter']",
        ))
        .unwrap();
        let rules = &resolved.supervision.rules;
        assert_eq!(
            rules.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            [
                "custom",
                "codex-trust",
                "press-enter",
                "codex-update-notice",
                "claude-trust",
                "claude-bypass-permissions"
            ]
        );
        assert_eq!(rules[1].harness.as_deref(), Some("codex"));
        assert_eq!(rules[1].keys, ["enter"]);
        assert!(
            rules[1]
                .pattern
                .is_match("Trust this folder?\nTrust and continue")
        );
        assert_eq!(rules[2].harness, None);
        assert!(rules[2].pattern.is_match("PRESS ENTER TO CONTINUE"));
        assert_eq!(rules[3].harness.as_deref(), Some("codex"));
        assert_eq!(rules[3].keys, ["esc"]);
        assert!(rules[3].pattern.is_match("Update available, try later"));
        assert_eq!(resolve(&config("")).unwrap().supervision.rules.len(), 5);
    }

    #[test]
    fn caps_must_be_positive() {
        for name in [
            "max_concurrent_tickets",
            "max_parallel_agents",
            "max_revision_rounds",
            "max_completed_assignments",
        ] {
            let error = resolve(&config(&format!("[shepherd]\n{name} = 0"))).unwrap_err();
            assert_eq!(error.code(), "config_invalid");
            assert!(error.to_string().contains(name));
        }
    }

    #[test]
    fn malformed_settings_rejected() {
        for text in [
            "[shepherd]\nmax_parallel_agents = -1",
            "[shepherd]\nrunner = 42",
            "roles = []",
            "runners = []",
            "[runners.custom]\nargs = []",
            "[roles.coder]\nworktree = 'elsewhere'",
            "[[supervision.rules]]\nname = 'broken'\npattern = '['\nkeys = ['enter']",
        ] {
            let error = resolve(&config(text)).unwrap_err();
            assert_eq!(error.code(), "config_invalid", "{text}");
        }
    }

    #[test]
    fn custom_settings_preserve_runner_order() {
        let resolved = resolve(&config(
            r#"
[shepherd]
runner = "custom"
max_concurrent_tickets = 4
max_parallel_agents = 8
max_revision_rounds = 2
pr_poll_seconds = 90
integration_branch = "staging"
protected_branches = ["trunk"]
[runners.custom]
harness = "future-harness"
args = ["--model", "custom"]
[roles.coder]
runners = ["custom", "omp-astra", "codex-sol-high"]
prompt = "Check the requested paths"
[supervision]
sweep_seconds = 10
report_grace_seconds = 40
stall_minutes = 5
escalate_minutes = 2
"#,
        ))
        .unwrap();
        assert_eq!(resolved.shepherd.runner, "custom");
        assert_eq!(resolved.shepherd.max_concurrent_tickets, 4);
        assert_eq!(resolved.shepherd.max_parallel_agents, 8);
        assert_eq!(resolved.shepherd.max_revision_rounds, 2);
        assert_eq!(resolved.shepherd.pr_poll_seconds, 90);
        assert_eq!(resolved.shepherd.integration_branch, "staging");
        assert_eq!(resolved.shepherd.protected_branches, ["trunk"]);
        let coder = resolved.roles.iter().find(|r| r.name == "coder").unwrap();
        assert_eq!(coder.runners, ["custom", "omp-astra", "codex-sol-high"]);
        assert_eq!(coder.worktree, WorktreeMode::Child);
        assert_eq!(coder.prompt.as_deref(), Some("Check the requested paths"));
        assert_eq!(resolved.runners["custom"].name, "custom");
        assert_eq!(resolved.runners["custom"].harness, "future-harness");
        assert_eq!(resolved.supervision.sweep_seconds, 10);
        assert_eq!(resolved.supervision.report_grace_seconds, 40);
        assert_eq!(resolved.supervision.stall_minutes, 5);
        assert_eq!(resolved.supervision.escalate_minutes, 2);
        assert_eq!(resolved.supervision.rules.len(), 5);
    }
}
