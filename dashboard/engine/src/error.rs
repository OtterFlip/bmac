use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};

/// Errors returned to the frontend. Each carries a stable code so the UI can
/// react to specific failures without parsing messages.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("unknown workflow: {0}")]
    UnknownWorkflow(String),
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("another operation is already changing the cluster: {0}")]
    Busy(String),
    #[error("unknown run: {0}")]
    UnknownRun(String),
    #[error("{0}")]
    InvalidResponse(String),
    #[error("{0}")]
    NotCancellable(String),
    #[error("the BMAC repository is not configured or is invalid: {0}")]
    Repository(String),
    #[error("{0}")]
    Io(String),
}

impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            EngineError::UnknownWorkflow(_) => "unknown_workflow",
            EngineError::InvalidArgument(_) => "invalid_argument",
            EngineError::Unavailable(_) => "unavailable",
            EngineError::Busy(_) => "busy",
            EngineError::UnknownRun(_) => "unknown_run",
            EngineError::InvalidResponse(_) => "invalid_response",
            EngineError::NotCancellable(_) => "not_cancellable",
            EngineError::Repository(_) => "repository",
            EngineError::Io(_) => "io",
        }
    }
}

impl From<std::io::Error> for EngineError {
    fn from(error: std::io::Error) -> Self {
        EngineError::Io(error.to_string())
    }
}

impl Serialize for EngineError {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("EngineError", 2)?;
        state.serialize_field("code", self.code())?;
        state.serialize_field("message", &self.to_string())?;
        state.end()
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;
