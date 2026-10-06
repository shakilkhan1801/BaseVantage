use alloy::primitives::Address;

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(thiserror::Error, Debug, Clone)]
pub enum EngineError {
    #[error("config invalid: {0}")]
    Config(String),

    #[error("rpc error: {0}")]
    Rpc(String),

    #[error("no healthy rpc endpoint")]
    NoHealthyEndpoint,

    #[error("quote failed: {0}")]
    Quote(String),

    #[error("no route found: {0}")]
    NoRoute(String),

    #[error("pinned quote rejected: {0}")]
    PinExpired(String),

    #[error("safety refuse: {0}")]
    SafetyRefused(String),

    #[error("safety block: {0}")]
    SafetyBlocked(String),

    #[error("watchlist: {0}")]
    Watchlist(String),

    #[error("cache: {0}")]
    Cache(String),

    #[error("settlement: {0}")]
    Settlement(String),
}

impl EngineError {
    pub fn config(violations: &[ConfigViolation]) -> Self {
        let joined = violations
            .iter()
            .map(|v| format!("{}: {}", v.key, v.message))
            .collect::<Vec<_>>()
            .join("; ");
        Self::Config(joined)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigViolation {
    pub key: String,
    pub message: String,
}

impl ConfigViolation {
    pub fn new(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// One line of a refusal card shown to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalCard {
    pub title: String,
    pub lines: Vec<String>,
}

impl RefusalCard {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            lines: Vec::new(),
        }
    }

    pub fn line(mut self, line: impl Into<String>) -> Self {
        self.lines.push(line.into());
        self
    }

    pub fn render(&self) -> String {
        let mut out = format!("== {} ==", self.title);
        for line in &self.lines {
            out.push('\n');
            out.push_str(line);
        }
        out
    }
}

pub fn checksum_ok(addr: &str) -> bool {
    addr.parse::<Address>()
        .is_ok_and(|a| a.to_checksum(None) == addr)
}
