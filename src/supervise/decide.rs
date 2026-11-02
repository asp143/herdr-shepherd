use crate::{gate::BranchPolicy, harness::Harness, roles::SupervisionSettings, store::AgentRecord};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Condition {
    PromptDeliveryUncertain,
    Blocked { screen: String },
    IdleNoReport { since_s: u64 },
    Stalled { since_min: u64 },
    Exited,
    ProviderFailure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    SendKeys { rule: String, keys: Vec<String> },
    AskLead { text: String },
    Deny { reason: String },
    NudgeReport,
    NudgeStatus,
    Resume,
    Respawn,
    Escalate { text: String },
    Nothing,
}

pub fn decide(
    rec: &AgentRecord,
    cond: &Condition,
    s: &SupervisionSettings,
    harness: &dyn Harness,
    policy: &BranchPolicy,
    in_worktree: bool,
    now_s: u64,
) -> Decision {
    if matches!(cond, Condition::PromptDeliveryUncertain) {
        return Decision::AskLead {
            text: format!(
                "{}: {} prompt delivery uncertain; inspect submission before sending input",
                rec.ticket, rec.name
            ),
        };
    }
    if let Condition::Blocked { screen } = cond {
        if let Some(reason) = policy_violation(screen, rec, policy) {
            return Decision::Deny { reason };
        }
        if provider_failure(rec, screen, harness) {
            return Decision::Respawn;
        }
        let since = rec
            .observed
            .as_deref()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .and_then(|v| v.get("condition_since").and_then(serde_json::Value::as_u64))
            .unwrap_or_else(|| rec.last_progress_at.parse().unwrap_or(now_s));
        let overdue = now_s.saturating_sub(since) >= s.escalate_minutes.saturating_mul(60);
        // Flock escalation takes precedence over further automatic input. The
        // lead can still accept safe keys; the sweep checks its due escalation
        // separately so a changing safe dialog cannot indefinitely defer it.
        if overdue && rec.role != "lead" {
            return Decision::Escalate {
                text: format!("{}: {} still blocked: {screen}", rec.ticket, rec.name),
            };
        }
        if in_worktree {
            for rule in &s.rules {
                if rule
                    .harness
                    .as_deref()
                    .is_none_or(|kind| kind == harness.kind())
                    && rule.pattern.is_match(screen)
                {
                    let hash = format!("{:x}", Sha256::digest(screen.as_bytes()));
                    let already_answered = rec
                        .observed
                        .as_deref()
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                        .and_then(|value| {
                            value
                                .get("answered_hash")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        })
                        .is_some_and(|answered| answered == hash);
                    if !already_answered {
                        return Decision::SendKeys {
                            rule: rule.name.clone(),
                            keys: rule.keys.clone(),
                        };
                    }
                    if !overdue {
                        return Decision::Nothing;
                    }
                    break;
                }
            }
        }
        if overdue {
            return Decision::Escalate {
                text: format!("{}: {} still blocked: {screen}", rec.ticket, rec.name),
            };
        }
        return Decision::AskLead {
            text: format!("{}: {} blocked: {screen}", rec.ticket, rec.name),
        };
    }
    match cond {
        Condition::IdleNoReport { since_s } if *since_s >= s.report_grace_seconds => {
            if rec.nudges == 0 {
                Decision::NudgeReport
            } else {
                Decision::AskLead {
                    text: format!("{}: {} idle without a report", rec.ticket, rec.name),
                }
            }
        }
        Condition::Stalled { since_min } if *since_min >= s.stall_minutes => {
            if rec.nudges == 0 {
                Decision::NudgeStatus
            } else {
                Decision::AskLead {
                    text: format!("{}: {} stalled without progress", rec.ticket, rec.name),
                }
            }
        }
        Condition::Exited if rec.restarts == 0 => Decision::Resume,
        Condition::Exited => Decision::AskLead {
            text: format!("{}: {} exited again after resume", rec.ticket, rec.name),
        },
        Condition::ProviderFailure => Decision::Respawn,
        _ => Decision::Nothing,
    }
}

/// Provider status must be an error, not an echoed role prompt mentioning
/// capacity or quota. Known task text is not independent failure evidence.
pub(super) fn provider_failure(rec: &AgentRecord, screen: &str, harness: &dyn Harness) -> bool {
    if !harness.provider_failure(screen) {
        return false;
    }
    let errors=regex::Regex::new(r"(?i)(rate[ _-]?limit(?:ed| exceeded)?|quota[^\n]{0,30}(?:exceeded|exhausted|reached)|insufficient credits|unauthori[sz]ed|overloaded|capacity[ _-]+(?:exceeded|exhausted|unavailable|reached)|(?:provider|authentication|authorization)[ _-]+(?:error|failed|failure))").expect("fixed provider error regex");
    let task = rec
        .task
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    errors.find_iter(screen).any(|error| {
        let evidence = error
            .as_str()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        !task.contains(&evidence)
    })
}

fn policy_violation(screen: &str, rec: &AgentRecord, policy: &BranchPolicy) -> Option<String> {
    let deny=regex::Regex::new(r"(?i)(--force(?:-with-lease)?\b|\bgh\s+pr\s+merge\b|--approve\b|\.env(?:[.\w-]*)?|Credentials[/\\]|\.pem\b|\.key\b|id_rsa\b|id_ed25519\b)").expect("fixed policy regex");
    if deny.is_match(screen) {
        return Some("dialog requests a forbidden action or secret access".into());
    }
    let github = regex::Regex::new(r"(?im)\bgh\s+([^\r\n]+)").expect("fixed GitHub command regex");
    for capture in github.captures_iter(screen) {
        let args: Vec<_> = capture[1]
            .split_whitespace()
            .map(|arg| arg.trim_matches(['\'', '"', '`', ';']))
            .collect();
        if crate::gate::check_gh(&args).is_err() {
            return Some("dialog requests a forbidden GitHub operation".into());
        }
    }
    let push = regex::Regex::new(r"(?im)\bgit\s+(?:-[^\n]*?\s+)?push\b([^\n?]*)")
        .expect("fixed push regex");
    for capture in push.captures_iter(screen) {
        for token in capture[1].split_whitespace() {
            let token = token.trim_matches(|c: char| {
                !c.is_ascii_alphanumeric() && !matches!(c, '/' | '-' | '_' | ':' | '*')
            });
            let branch = token.rsplit(':').next().unwrap_or(token);
            if policy.is_protected(branch) {
                return Some(format!("push to protected branch {branch}"));
            }
        }
    }
    let writes = regex::Regex::new(r"(?im)\b(?:touch|tee|cp|mv|rm|mkdir|sed|cat)\b[^\n]*")
        .expect("fixed write regex");
    for line in writes.find_iter(screen) {
        for token in line.as_str().split_whitespace() {
            let token = token.trim_matches(['\'', '"', '`', ';', ',']);
            if token.starts_with("../")
                || token.starts_with("~/")
                || (token.starts_with('/')
                    && !std::path::Path::new(token).starts_with(&rec.worktree))
            {
                return Some("dialog requests access outside the assigned worktree".into());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        gate::BranchPolicy,
        harness::harness_for,
        roles,
        store::{AgentRecord, AgentStatus},
    };
    fn record() -> AgentRecord {
        AgentRecord {
            name: "sh-test-coder-1".into(),
            ticket: "T-1".into(),
            role: "coder".into(),
            runner: "codex-sol-high".into(),
            status: AgentStatus::Blocked,
            pane_id: Some("w1:p1".into()),
            tab_id: Some("w1:t1".into()),
            worktree: "/tmp/ticket".into(),
            task: "implement".into(),
            report: None,
            observed: None,
            last_progress_at: "0".into(),
            nudges: 0,
            restarts: 0,
            prompt_pending: false,
            task_id: None,
            attempt_id: None,
            unit: None,
            launch_profile: None,
            completed_tasks: vec![],
            dispatch_reason: None,
        }
    }
    #[test]
    fn codex_trust_auto_answered() {
        let dir = tempfile::tempdir().unwrap();
        let settings = roles::resolve(&config::load(dir.path(), None).unwrap())
            .unwrap()
            .supervision;
        let decision = decide(
            &record(),
            &Condition::Blocked {
                screen: "Trust this folder?\nTrust and continue".into(),
            },
            &settings,
            &*harness_for("codex").unwrap(),
            &BranchPolicy::new(&[], None, None).unwrap(),
            true,
            0,
        );
        assert_eq!(
            decision,
            Decision::SendKeys {
                rule: "codex-trust".into(),
                keys: vec!["enter".into()]
            }
        );
    }
    #[test]
    fn claude_startup_dialogs_require_owned_cwd_and_claude_harness() {
        let dir = tempfile::tempdir().unwrap();
        let settings = roles::resolve(&config::load(dir.path(), None).unwrap())
            .unwrap()
            .supervision;
        let policy = BranchPolicy::new(&[], None, None).unwrap();
        for (name, screen) in [
            (
                "claude-trust",
                include_str!("../../tests/fixtures/claude-trust.txt"),
            ),
            (
                "claude-bypass-permissions",
                include_str!("../../tests/fixtures/claude-bypass-permissions.txt"),
            ),
        ] {
            for screen in [
                screen.to_owned(),
                screen.replace('\n', "\r\n"),
                screen.replace(
                    "you created or one you trust?",
                    "you created or\n one you trust?",
                ),
            ] {
                assert_eq!(
                    decide(
                        &record(),
                        &Condition::Blocked { screen },
                        &settings,
                        &*harness_for("claude").unwrap(),
                        &policy,
                        true,
                        0
                    ),
                    Decision::SendKeys {
                        rule: name.into(),
                        keys: vec!["down".into(), "enter".into()]
                    }
                );
            }
            let condition = Condition::Blocked {
                screen: screen.into(),
            };
            for (harness, inside) in [
                ("claude", true),
                ("claude", false),
                ("codex", true),
                ("omp", true),
            ] {
                let result = decide(
                    &record(),
                    &condition,
                    &settings,
                    &*harness_for(harness).unwrap(),
                    &policy,
                    inside,
                    0,
                );
                if harness == "claude" && inside {
                    assert_eq!(
                        result,
                        Decision::SendKeys {
                            rule: name.into(),
                            keys: vec!["down".into(), "enter".into()]
                        }
                    );
                } else {
                    assert!(
                        matches!(result, Decision::AskLead { .. }),
                        "{name}: {harness}, {inside}"
                    );
                }
            }
            // A policy violation takes precedence even on a recognized startup screen.
            let result = decide(
                &record(),
                &Condition::Blocked {
                    screen: format!("{screen}\nAllow git push origin main?"),
                },
                &settings,
                &*harness_for("claude").unwrap(),
                &BranchPolicy::new(&["main".into()], None, None).unwrap(),
                true,
                0,
            );
            assert!(matches!(result, Decision::Deny { .. }));
        }
    }

    #[test]
    fn claude_startup_rules_reject_ordinary_output_and_changed_selection() {
        let dir = tempfile::tempdir().unwrap();
        let settings = roles::resolve(&config::load(dir.path(), None).unwrap())
            .unwrap()
            .supervision;
        let harness = harness_for("claude").unwrap();
        let policy = BranchPolicy::new(&[], None, None).unwrap();
        for screen in [
            "Do you trust the files in this folder?\nYes, I trust this folder",
            "WARNING: Claude Code running in Bypass Permissions mode\nYes, I accept",
            "The startup dialog says Yes, I accept. Select it to proceed.",
            "Output: Accessing workspace: /tmp/ticket\nQuick safety check: Is this a project you created or one you trust?\nClaude Code'll be able to read, edit, and execute files here.\n❯ No, exit\n  Yes, I trust this folder",
        ] {
            assert!(
                matches!(
                    decide(
                        &record(),
                        &Condition::Blocked {
                            screen: screen.into()
                        },
                        &settings,
                        &*harness,
                        &policy,
                        true,
                        0
                    ),
                    Decision::AskLead { .. }
                ),
                "{screen}"
            );
        }
        for screen in [
            include_str!("../../tests/fixtures/claude-trust.txt"),
            include_str!("../../tests/fixtures/claude-bypass-permissions.txt"),
        ] {
            for ordinary in [
                screen.replace(" ❯ No, exit", "   No, exit"),
                screen
                    .replace(" ❯ No, exit", "   No, exit")
                    .replace("   Yes,", " ❯ Yes,"),
                screen
                    .lines()
                    .map(|line| format!("> {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ] {
                assert!(
                    matches!(
                        decide(
                            &record(),
                            &Condition::Blocked {
                                screen: ordinary.clone()
                            },
                            &settings,
                            &*harness,
                            &policy,
                            true,
                            0
                        ),
                        Decision::AskLead { .. }
                    ),
                    "{ordinary}"
                );
            }
        }
    }

    fn decision(rec: &AgentRecord, cond: Condition, inside: bool, now: u64) -> Decision {
        let dir = tempfile::tempdir().unwrap();
        let settings = roles::resolve(&config::load(dir.path(), None).unwrap())
            .unwrap()
            .supervision;
        decide(
            rec,
            &cond,
            &settings,
            &*harness_for("codex").unwrap(),
            &BranchPolicy::new(&[], None, None).unwrap(),
            inside,
            now,
        )
    }
    #[test]
    fn github_policy_mutations_are_denied_before_rules() {
        for command in [
            "gh api repos/o/r/git/refs -X POST -f ref=refs/heads/main",
            "gh issue delete 7 --yes",
            "gh run rerun 7",
        ] {
            assert!(
                matches!(
                    decision(
                        &record(),
                        Condition::Blocked {
                            screen: format!(
                                "Trust this folder? Trust and continue\nAllow {command}"
                            )
                        },
                        true,
                        0
                    ),
                    Decision::Deny { .. }
                ),
                "{command}"
            );
        }
    }
    #[test]
    fn policy_screen_denied_even_if_rule_matches() {
        let d = decision(
            &record(),
            Condition::Blocked {
                screen: "Trust this folder? Trust and continue. Allow git push origin main?".into(),
            },
            true,
            0,
        );
        assert!(matches!(d, Decision::Deny { .. }));
        for screen in [
            "gh pr merge 42",
            "git push origin feat/t --force-with-lease",
            "gh pr review --approve",
            "Read .env.production",
            "cat Credentials/data",
            "touch /etc/config",
        ] {
            assert!(
                matches!(
                    decision(
                        &record(),
                        Condition::Blocked {
                            screen: screen.into()
                        },
                        true,
                        0
                    ),
                    Decision::Deny { .. }
                ),
                "{screen}"
            );
        }
    }

    #[test]
    fn rule_skipped_outside_worktree() {
        assert!(matches!(
            decision(
                &record(),
                Condition::Blocked {
                    screen: "Trust this folder? Trust and continue".into()
                },
                false,
                0
            ),
            Decision::AskLead { .. }
        ));
    }
    #[test]
    fn unknown_dialog_asks_lead() {
        assert!(matches!(
            decision(
                &record(),
                Condition::Blocked {
                    screen: "Pick a package manager".into()
                },
                true,
                0
            ),
            Decision::AskLead { .. }
        ));
    }
    #[test]
    fn escalates_after_threshold() {
        assert!(matches!(
            decision(
                &record(),
                Condition::Blocked {
                    screen: "Pick a package manager".into()
                },
                true,
                15 * 60
            ),
            Decision::Escalate { .. }
        ));
    }

    #[test]
    fn idle_no_report_nudges_once_then_asks_lead() {
        let mut rec = record();
        assert_eq!(
            decision(&rec, Condition::IdleNoReport { since_s: 119 }, true, 119),
            Decision::Nothing
        );
        assert_eq!(
            decision(&rec, Condition::IdleNoReport { since_s: 120 }, true, 120),
            Decision::NudgeReport
        );
        rec.nudges = 1;
        assert!(matches!(
            decision(&rec, Condition::IdleNoReport { since_s: 240 }, true, 240),
            Decision::AskLead { .. }
        ));
    }

    #[test]
    fn stall_nudge_then_lead() {
        let mut rec = record();
        assert_eq!(
            decision(&rec, Condition::Stalled { since_min: 19 }, true, 1140),
            Decision::Nothing
        );
        assert_eq!(
            decision(&rec, Condition::Stalled { since_min: 20 }, true, 1200),
            Decision::NudgeStatus
        );
        rec.nudges = 1;
        assert!(matches!(
            decision(&rec, Condition::Stalled { since_min: 40 }, true, 2400),
            Decision::AskLead { .. }
        ));
    }

    #[test]
    fn exit_resumes_once() {
        let mut rec = record();
        assert_eq!(decision(&rec, Condition::Exited, true, 1), Decision::Resume);
        rec.restarts = 1;
        assert!(matches!(
            decision(&rec, Condition::Exited, true, 2),
            Decision::AskLead { .. }
        ));
    }

    #[test]
    fn provider_failure_respawns() {
        assert_eq!(
            decision(&record(), Condition::ProviderFailure, true, 0),
            Decision::Respawn
        );
    }
}
