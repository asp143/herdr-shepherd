use super::Harness;
use crate::roles::Runner;

pub struct Codex;

impl Harness for Codex {
    fn kind(&self) -> &'static str {
        "codex"
    }
    fn launch_args(&self, runner: &Runner) -> Vec<String> {
        runner.args.clone()
    }
    fn subagent_hint(&self) -> &'static str {
        "spawn sub-agents (multi_agent is enabled)"
    }
    fn provider_failure(&self, screen: &str) -> bool {
        super::provider_failure(screen)
    }
}
