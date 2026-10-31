use std::{fmt, path::PathBuf};

#[derive(Debug)]
pub enum ShepherdError {
    Usage(String),
    Config { path: PathBuf, message: String },
    HerdrUnavailable(String),
    GithubUnavailable(String),
    Runtime(anyhow::Error),
    PolicyDenied(String),
}

impl ShepherdError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Config { .. } => "config_invalid",
            Self::HerdrUnavailable(_) => "herdr_unavailable",
            Self::GithubUnavailable(_) => "github_unavailable",
            Self::Runtime(_) => "runtime",
            Self::PolicyDenied(_) => "policy_denied",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Config { .. } => 2,
            Self::HerdrUnavailable(_) => 3,
            Self::Runtime(_) | Self::GithubUnavailable(_) => 1,
            Self::PolicyDenied(_) => 4,
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
        }
    }
}

impl std::error::Error for ShepherdError {}

impl From<anyhow::Error> for ShepherdError {
    fn from(error: anyhow::Error) -> Self {
        Self::Runtime(error)
    }
}
