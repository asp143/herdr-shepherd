use super::Harness;
use crate::roles::Runner;

pub struct Claude;

impl Harness for Claude {
    fn kind(&self) -> &'static str {
        "claude"
    }
    fn launch_args(&self, runner: &Runner) -> Vec<String> {
        runner.args.clone()
    }
    fn subagent_hint(&self) -> &'static str {
        "use internal subagents when supported"
    }
    fn provider_failure(&self, screen: &str) -> bool {
        super::provider_failure(screen)
    }
}
