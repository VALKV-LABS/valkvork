//! Error types shared across Valkvork crates.

use thiserror::Error;

/// Top-level error type.
#[derive(Debug, Error)]
pub enum ValkvorkError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("validation failed: {}", format_validation(.0))]
    Validation(Vec<ValidationError>),

    #[error("database error: {0}")]
    Database(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("{0}")]
    FailedPrecondition(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn format_validation(errors: &[ValidationError]) -> String {
    errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

impl From<ValkvorkError> for tonic::Status {
    fn from(e: ValkvorkError) -> Self {
        match e {
            ValkvorkError::Validation(_) => tonic::Status::invalid_argument(e.to_string()),
            ValkvorkError::NotFound(_) => tonic::Status::not_found(e.to_string()),
            ValkvorkError::FailedPrecondition(_) => {
                tonic::Status::failed_precondition(e.to_string())
            }
            _ => tonic::Status::internal(e.to_string()),
        }
    }
}

// ============================================================================
// Validation errors (spec §6.2)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorCode {
    InvalidResourceSpec,
    InvalidImageRef,
    InvalidServiceId,
    InvalidAutoscaleSpec,
    InvalidPortSpec,
    UnknownField,
    InvalidVolumeSpec,
    UnsatisfiableNow,
}

impl std::fmt::Display for ValidationErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ValidationErrorCode::InvalidResourceSpec => "INVALID_RESOURCE_SPEC",
            ValidationErrorCode::InvalidImageRef => "INVALID_IMAGE_REF",
            ValidationErrorCode::InvalidServiceId => "INVALID_SERVICE_ID",
            ValidationErrorCode::InvalidAutoscaleSpec => "INVALID_AUTOSCALE_SPEC",
            ValidationErrorCode::InvalidPortSpec => "INVALID_PORT_SPEC",
            ValidationErrorCode::UnknownField => "UNKNOWN_FIELD",
            ValidationErrorCode::InvalidVolumeSpec => "INVALID_VOLUME_SPEC",
            ValidationErrorCode::UnsatisfiableNow => "UNSATISFIABLE_NOW",
        })
    }
}

/// A validation finding with a machine-readable code and TOML field path.
#[derive(Debug, Clone, Error)]
pub struct ValidationError {
    pub code: ValidationErrorCode,
    pub field_path: String,
    pub message: String,
    pub is_warning: bool,
}

impl ValidationError {
    pub fn new(code: ValidationErrorCode, field_path: &str, message: &str) -> Self {
        Self {
            code,
            field_path: field_path.to_string(),
            message: message.to_string(),
            is_warning: false,
        }
    }

    pub fn warning(code: ValidationErrorCode, field_path: &str, message: &str) -> Self {
        Self {
            is_warning: true,
            ..Self::new(code, field_path, message)
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at {}: {}",
            self.code,
            if self.field_path.is_empty() {
                "<root>"
            } else {
                &self.field_path
            },
            self.message
        )
    }
}

// ============================================================================
// Runtime errors (agent side)
// ============================================================================

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("runtime unavailable: {0}")]
    Unavailable(String),

    #[error("image pull failed: {0}")]
    ImagePullFailed(String),

    #[error("container not found: {0}")]
    NotFound(String),

    #[error("container already exists: {0}")]
    AlreadyExists(String),

    #[error("runtime error: {0}")]
    Other(String),
}
