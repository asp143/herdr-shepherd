mod claude;
mod codex;
mod omp;
mod pi;

use crate::{error::ShepherdError, roles::Runner};

/// An observation, not an admission policy or an estimated utilization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextHealth {
    Healthy,
    NearLimit,
    Exhausted,
    Unknown,
}

pub trait Harness {
    fn kind(&self) -> &'static str;
    fn launch_args(&self, runner: &Runner) -> Vec<String>;
    /// Apply shepherd-owned transport settings without changing the pinned runner.
    fn managed_args(
        &self,
        args: Vec<String>,
        state_dir: &std::path::Path,
    ) -> Result<Vec<String>, ShepherdError> {
        if self.kind() == "omp" {
            omp::managed_args(args, state_dir)
        } else {
            Ok(args)
        }
    }
    fn subagent_hint(&self) -> &'static str;
    fn provider_failure(&self, screen: &str) -> bool;
    /// AgentInfo has no context telemetry. No supported harness currently has
    /// a verified, bounded visible gauge in our fixtures, so all return Unknown.
    /// Do not infer health from conversation text, provider quota errors, or
    /// invented percentages. A future override needs harness-specific evidence.
    /// Unknown eligibility and assignment caps belong to the dispatcher.
    fn context_health(&self, _screen: &str) -> ContextHealth {
        ContextHealth::Unknown
    }
    fn resume_args(&self, runner: &Runner, session_ref: Option<&str>) -> Vec<String> {
        let mut args = runner.args.clone();
        match (self.kind(), session_ref) {
            ("codex", Some(reference)) => args.extend(["resume".into(), reference.into()]),
            ("codex", None) => args.extend(["resume".into(), "--last".into()]),
            ("omp" | "claude", Some(reference)) => {
                args.extend(["--resume".into(), reference.into()])
            }
            ("omp", None) => args.push("--resume".into()),
            ("claude", None) => args.push("--continue".into()),
            ("pi", Some(reference)) => args.extend(["--session".into(), reference.into()]),
            ("pi", None) => args.push("--continue".into()),
            _ => {}
        }
        args
    }
}

pub fn harness_for(kind: &str) -> Result<Box<dyn Harness>, ShepherdError> {
    match kind {
        "codex" => Ok(Box::new(codex::Codex)),
        "omp" => Ok(Box::new(omp::Omp)),
        "claude" => Ok(Box::new(claude::Claude)),
        "pi" => Ok(Box::new(pi::Pi)),
        _ => Err(ShepherdError::Config {
            path: std::path::PathBuf::from("<runner>"),
            message: format!("unknown harness: {kind}"),
        }),
    }
}

fn provider_failure(screen: &str) -> bool {
    regex::Regex::new(
        r"(?i)(rate limit|quota|insufficient credits|unauthori[sz]ed|overloaded|capacity)",
    )
    .expect("provider failure regex is valid")
    .is_match(screen)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_health_is_unknown_without_a_verified_harness_signal() {
        // These are untrusted conversation text, not verified status gauges.
        // Neither a percentage nor an exhaustion phrase proves utilization.
        for kind in ["codex", "omp", "claude", "pi"] {
            let harness = harness_for(kind).unwrap();
            for screen in [
                "",
                "idle",
                "context 99% full",
                "context exhausted",
                "100% context left",
                "quota exhausted",
            ] {
                assert_eq!(
                    harness.context_health(screen),
                    ContextHealth::Unknown,
                    "{kind}: {screen}"
                );
            }
        }
    }

    #[test]
    fn managed_omp_overlay_precedes_literal_argument_separator() {
        let dir = tempfile::tempdir().unwrap();
        let pinned = vec![
            "--config=custom.yml".into(),
            "--".into(),
            "positional prompt".into(),
        ];
        let args = harness_for("omp")
            .unwrap()
            .managed_args(pinned.clone(), dir.path())
            .unwrap();
        assert_eq!(args[0], pinned[0]);
        assert_eq!(args[1], "--config");
        assert_eq!(&args[3..], &pinned[1..]);
        assert_eq!(
            std::fs::read_to_string(&args[2]).unwrap(),
            "paste:\n  largeMenuThreshold: 0\n"
        );
    }

    #[test]
    fn managed_omp_overlay_failure_is_not_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("unavailable");
        std::fs::write(&state, "synthetic non-directory").unwrap();
        assert!(
            harness_for("omp")
                .unwrap()
                .managed_args(vec![], &state)
                .is_err()
        );
        for kind in ["codex", "claude", "pi"] {
            let args = vec!["--configured-option".into()];
            assert_eq!(
                harness_for(kind)
                    .unwrap()
                    .managed_args(args.clone(), &state)
                    .unwrap(),
                args
            );
        }
    }

    #[test]
    fn codex_args_verbatim() {
        let runner = Runner {
            name: "codex-sol-high".into(),
            harness: "codex".into(),
            args: vec![
                "--yolo".into(),
                "-m".into(),
                "gpt-6.1-sol".into(),
                "-c".into(),
                "model_reasoning_effort=high".into(),
            ],
        };
        assert_eq!(
            harness_for("codex")
                .unwrap()
                .managed_args(
                    harness_for("codex").unwrap().launch_args(&runner),
                    std::path::Path::new("/does-not-exist-synthetic-fixture"),
                )
                .unwrap(),
            runner.args
        );
    }
}
