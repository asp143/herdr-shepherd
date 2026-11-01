use std::{fmt, path::PathBuf};

#[derive(Debug)]
pub enum ShepherdError {
    Usage(String),
    Config { path: PathBuf, message: String },
    HerdrUnavailable(String),
    GithubUnavailable(String),
    Runtime(anyhow::Error),
    HerdrCommand { code: String, message: String },
    PromptDeliveryUncertain(Box<ShepherdError>),
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
        } else if let Self::PromptDeliveryUncertain(source) = self {
            source.herdr_code()
        } else {
            None
        }
    }
    /// Apply only at the prompt boundary. A missing acknowledgement can follow
    /// application acceptance, a partial PTY write, or text staged in an editor.
    pub fn into_prompt_error(self) -> Self {
        match &self {
            Self::HerdrUnavailable(_) | Self::PromptDeliveryUncertain(_) => self,
            Self::HerdrCommand { code, .. }
                if matches!(
                    code.as_str(),
                    "agent_blocked" | "agent_not_ready" | "empty_agent_prompt"
                ) =>
            {
                self
            }
            _ => Self::PromptDeliveryUncertain(Box::new(self)),
        }
    }

    pub fn prompt_submission_uncertain(&self) -> bool {
        matches!(self, Self::PromptDeliveryUncertain(_))
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Config { .. } => "config_invalid",
            Self::HerdrUnavailable(_) => "herdr_unavailable",
            Self::GithubUnavailable(_) => "github_unavailable",
            Self::Runtime(_) | Self::HerdrCommand { .. } | Self::PromptDeliveryUncertain(_) => {
                "runtime"
            }
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
            | Self::PromptDeliveryUncertain(_)
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
            Self::PromptDeliveryUncertain(source) => write!(
                f,
                "prompt delivery uncertain: {source}; inspect delivery before sending more input"
            ),
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
    fn prompt_errors_distinguish_rejection_from_unknown_delivery() {
        for code in ["timeout", "agent_prompt_stalled", "agent_prompt_failed"] {
            let error = ShepherdError::HerdrCommand {
                code: code.into(),
                message: "command failed".into(),
            }
            .into_prompt_error();
            assert!(error.prompt_submission_uncertain());
            assert_eq!(error.herdr_code(), Some(code));
            assert_eq!(error.code(), "runtime");
            assert_eq!(error.exit_code(), 1);
        }
        let local =
            ShepherdError::Runtime(crate::proc::DeadlineExceeded.into()).into_prompt_error();
        assert!(local.prompt_submission_uncertain());
        for code in ["agent_blocked", "agent_not_ready", "empty_agent_prompt"] {
            let error = ShepherdError::HerdrCommand {
                code: code.into(),
                message: "rejected before input".into(),
            }
            .into_prompt_error();
            assert!(!error.prompt_submission_uncertain());
            assert_eq!(error.herdr_code(), Some(code));
        }
        assert!(
            !ShepherdError::HerdrUnavailable("not invoked".into())
                .into_prompt_error()
                .prompt_submission_uncertain()
        );
        // An observation timeout outside the prompt boundary retains its meaning.
        assert!(
            !ShepherdError::HerdrCommand {
                code: "timeout".into(),
                message: "startup wait".into()
            }
            .prompt_submission_uncertain()
        );
    }
}
