use std::{fmt, path::PathBuf};

#[derive(Debug)]
pub enum ShepherdError {
    Usage(String),
    Config { path: PathBuf, message: String },
    HerdrUnavailable(String),
    GithubUnavailable(String),
    Runtime(anyhow::Error),
    HerdrCommand { code: String, message: String },
    PolicyDenied(String),
    Capacity,
    Conflict { unit: u32, range: String },
    Attribution,
    NoTicketSource,
}

impl ShepherdError {
    pub fn herdr_code(&self) -> Option<&str> {
        if let Self::HerdrCommand { code, .. } = self {
            Some(code)
        } else {
            None
        }
    }
    pub fn prompt_was_submitted(&self) -> bool {
        matches!(self.herdr_code(), Some("timeout" | "agent_prompt_stalled"))
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Config { .. } => "config_invalid",
            Self::HerdrUnavailable(_) => "herdr_unavailable",
            Self::GithubUnavailable(_) => "github_unavailable",
            Self::Runtime(_) | Self::HerdrCommand { .. } => "runtime",
            Self::PolicyDenied(_) => "policy_denied",
            Self::Capacity => "capacity",
            Self::Conflict { .. } => "conflict",
            Self::Attribution => "attribution",
            Self::NoTicketSource => "no_ticket_source",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Config { .. } => 2,
            Self::HerdrUnavailable(_) => 3,
            Self::Runtime(_)
            | Self::HerdrCommand { .. }
            | Self::GithubUnavailable(_)
            | Self::Capacity
            | Self::Conflict { .. } => 1,
            Self::PolicyDenied(_) => 4,
            Self::Attribution | Self::NoTicketSource => 2,
        }
    }
}

impl fmt::Display for ShepherdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message)
            | Self::HerdrUnavailable(message)
            | Self::GithubUnavailable(message)
            | Self::PolicyDenied(message) => f.write_str(message),
            Self::Config { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Runtime(error) => write!(f, "{error:#}"),
            Self::HerdrCommand { code, message } => write!(f, "herdr {code}: {message}"),
            Self::Capacity => f.write_str("parallel agent capacity reached"),
            Self::Conflict { unit, range } => write!(f, "child unit {unit} conflicts: {range}"),
            Self::Attribution => f.write_str("human-facing text contains AI attribution"),
            Self::NoTicketSource => {
                f.write_str("no ticket source configured; paste the ticket text")
            }
        }
    }
}

impl std::error::Error for ShepherdError {}

impl From<anyhow::Error> for ShepherdError {
    fn from(error: anyhow::Error) -> Self {
        Self::Runtime(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_structured_prompt_timeouts_imply_submission() {
        let transport =
            ShepherdError::Runtime(anyhow::anyhow!("connect timeout while submitting prompt"));
        assert!(!transport.prompt_was_submitted());
        let misleading = ShepherdError::Runtime(anyhow::anyhow!("herdr command failed: timeout"));
        assert!(!misleading.prompt_was_submitted());
        let own = ShepherdError::HerdrCommand {
            code: "timeout".into(),
            message: "wait expired".into(),
        };
        assert!(own.prompt_was_submitted());
        assert_eq!(own.herdr_code(), Some("timeout"));
        let denied = ShepherdError::HerdrCommand {
            code: "agent_blocked".into(),
            message: "blocked".into(),
        };
        assert!(!denied.prompt_was_submitted());
    }
}
