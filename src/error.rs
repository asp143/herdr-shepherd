use std::{fmt, path::PathBuf};

#[derive(Debug)]
pub enum ShepherdError {
    Usage(String),
    Config { path: PathBuf, message: String },
    HerdrUnavailable(String),
    Runtime(anyhow::Error),
    Attribution,
    NoTicketSource,
}

impl ShepherdError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Usage(_) => "usage",
            Self::Config { .. } => "config_invalid",
            Self::HerdrUnavailable(_) => "herdr_unavailable",
            Self::Runtime(_) => "runtime",
            Self::Attribution => "attribution",
            Self::NoTicketSource => "no_ticket_source",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) | Self::Config { .. } => 2,
            Self::HerdrUnavailable(_) => 3,
            Self::Runtime(_) => 1,
            Self::Attribution | Self::NoTicketSource => 2,
        }
    }
}

impl fmt::Display for ShepherdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::HerdrUnavailable(message) => f.write_str(message),
            Self::Config { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Runtime(error) => write!(f, "{error:#}"),
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
