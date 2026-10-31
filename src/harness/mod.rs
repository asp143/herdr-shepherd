mod claude;
mod codex;
mod omp;
mod pi;

use crate::{error::ShepherdError, roles::Runner};

pub trait Harness {
    fn kind(&self) -> &'static str;
    fn launch_args(&self, runner: &Runner) -> Vec<String>;
    fn subagent_hint(&self) -> &'static str;
    fn provider_failure(&self, screen: &str) -> bool;
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
            harness_for("codex").unwrap().launch_args(&runner),
            runner.args
        );
    }
}
