#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Why a Redmine call failed. Tools branch on the variant, never on the message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RedmineError {
    /// Redmine answered with a non-success status.
    #[error("{message}")]
    Http {
        status: u16,
        path: String,
        message: String,
    },
    /// The request was not sent, got no answer, or the body broke off.
    #[error("{message}")]
    Transport { path: String, message: String },
    /// The body was not the JSON we expected, or a request body would not encode.
    #[error("{message}")]
    Json { path: String, message: String },
    /// A write was attempted with REDMINE_READONLY on. Nothing was sent.
    #[error("Redmine MCP is read-only. Set REDMINE_READONLY=false to enable writes.")]
    ReadOnly,
    /// The body passed the caller's byte limit. The read stopped there.
    #[error("{path} is larger than {limit} bytes.")]
    TooLarge { path: String, limit: u64 },
}

impl RedmineError {
    /// The HTTP status, for errors where Redmine actually answered.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Http { path, .. }
            | Self::Transport { path, .. }
            | Self::Json { path, .. }
            | Self::TooLarge { path, .. } => Some(path),
            Self::ReadOnly => None,
        }
    }
}

/// What a tool handler returns on failure. Keeps the `RedmineError` intact so a
/// caller can still branch on status, for example a 404 from an optional plugin
/// endpoint. Becomes text only at the MCP boundary.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolError {
    #[error(transparent)]
    Redmine(#[from] RedmineError),
    /// The arguments or the response ruled the call out. The message says why.
    #[error("{0}")]
    Refused(String),
}

impl ToolError {
    pub fn refused(message: impl Into<String>) -> Self {
        Self::Refused(message.into())
    }
}
