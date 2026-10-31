use super::Harness;
use crate::roles::Runner;

pub struct Omp;

impl Harness for Omp {
    fn kind(&self) -> &'static str {
        "omp"
    }
    fn launch_args(&self, runner: &Runner) -> Vec<String> {
        runner.args.clone()
    }
    fn subagent_hint(&self) -> &'static str {
        "use one batch task call"
    }
    fn provider_failure(&self, screen: &str) -> bool {
        super::provider_failure(screen)
    }
}
