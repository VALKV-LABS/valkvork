//! Error types for Valkvork.
//!
//! This module provides a comprehensive error handling system with typed errors
//! for all subsystems.

use thiserror::Error;

/// Top-level error type for Valkvork
#[derive(Debug, Error)]
pub enum ValkvorkError {
    #[error("Configuration error: {0}")]
    Config(#[from] ConfigError),
    
    #[error("Validation error: {0}")]
    Validation(#[from] ValidationError),
    
    #[error("Database error: {0}")]
    Database(#[from] DatabaseError),
    
    #[error("Network error: {0}")]
    Network(#[from] NetworkError),
    
    #[error("TLS error: {0}")]
    Tls(#[from] TlsError),
    
    #[error("Runtime error: {0}")]
    Runtime(#[from] RuntimeError),
    
    #[error("Agent error: {0}")]
    Agent(#[from] AgentError),
    
    #[error("Controller error: {0}")]
    Controller(#[from] ControllerError),
    
    #[error("Scheduling error: {0}")]
    Scheduling(#[from] SchedulingError),
    
    #[error("Authentication error: {0}")]
    Authentication(#[from] AuthenticationError),
    
    #[error("Not found: {0}")]
    NotFound(String),
    
    #[error("Already exists: {0}")]
    AlreadyExists(String),
    
    #[error("Internal error: {0}")]
    Internal(String),
    
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl ValkvorkError {
    pub fn code(&self) -> &'static str {
        match self {
            ValkvorkError::Config(_) => "CONFIG_ERROR",
            ValkvorkError::Validation(_) => "VALIDATION_ERROR",
            ValkvorkError::Database(_) => "DATABASE_ERROR",
            ValkvorkError::Network(_) => "NETWORK_ERROR",
            ValkvorkError::Tls(_) => "TLS_ERROR",
            ValkvorkError::Runtime(_) => "RUNTIME_ERROR",
            ValkvorkError::Agent(_) => "AGENT_ERROR",
            ValkvorkError::Controller(_) => "CONTROLLER_ERROR",
            ValkvorkError::Scheduling(_) => "SCHEDULING_ERROR",
            ValkvorkError::Authentication(_) => "AUTH_ERROR",
            ValkvorkError::NotFound(_) => "NOT_FOUND",
            ValkvorkError::AlreadyExists(_) => "ALREADY_EXISTS",
            ValkvorkError::Internal(_) => "INTERNAL_ERROR",
            ValkvorkError::Io(_) => "IO_ERROR",
        }
    }
}

// ============================================================================
// Configuration Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Missing required field: {0}")]
    MissingField(String),
    
    #[error("Invalid value for {0}: {1}")]
    InvalidValue(String, String),
    
    #[error("Invalid path: {0}")]
    InvalidPath(String),
    
    #[error("Cannot read config file: {0}")]
    ReadError(String),
    
    #[error("Cannot parse config file: {0}")]
    ParseError(String),
}

// ============================================================================
// Validation Errors
// ============================================================================

/// Validation error codes as per spec §6.2
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorCode {
    InvalidResourceSpec,
    InvalidImageRef,
    InvalidServiceId,
    InvalidAutoscaleSpec,
    InvalidPortSpec,
    UnknownField,
    UnsatisfiableNow,
}

impl std::fmt::Display for ValidationErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationErrorCode::InvalidResourceSpec => write!(f, "INVALID_RESOURCE_SPEC"),
            ValidationErrorCode::InvalidImageRef => write!(f, "INVALID_IMAGE_REF"),
            ValidationErrorCode::InvalidServiceId => write!(f, "INVALID_SERVICE_ID"),
            ValidationErrorCode::InvalidAutoscaleSpec => write!(f, "INVALID_AUTOSCALE_SPEC"),
            ValidationErrorCode::InvalidPortSpec => write!(f, "INVALID_PORT_SPEC"),
            ValidationErrorCode::UnknownField => write!(f, "UNKNOWN_FIELD"),
            ValidationErrorCode::UnsatisfiableNow => write!(f, "UNSATISFIABLE_NOW"),
        }
    }
}

#[derive(Debug, Error)]
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
            code,
            field_path: field_path.to_string(),
            message: message.to_string(),
            is_warning: true,
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_warning {
            write!(f, "Warning: {}({}): {}", self.code, self.field_path, self.message)
        } else {
            write!(f, "{}({}): {}", self.code, self.field_path, self.message)
        }
    }
}

// ============================================================================
// Database Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("Connection error: {0}")]
    Connection(String),
    
    #[error("Query error: {0}")]
    Query(String),
    
    #[error("Migration error: {0}")]
    Migration(String),
    
    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),
    
    #[error("Database busy")]
    Busy,
    
    #[error("Integrity check failed: {0}")]
    IntegrityCheckFailed(String),
}

// ============================================================================
// Network Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("Connection refused: {0}")]
    ConnectionRefused(String),
    
    #[error("Connection timeout: {0}")]
    Timeout(String),
    
    #[error("Stream closed: {0}")]
    StreamClosed(String),
    
    #[error("Invalid message: {0}")]
    InvalidMessage(String),
    
    #[error("Grpc error: {0}")]
    Grpc(#[from] tonic::Status),
}

// ============================================================================
// TLS Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("Certificate error: {0}")]
    Certificate(String),
    
    #[error("Handshake error: {0}")]
    Handshake(String),
    
    #[error("Invalid CA bundle: {0}")]
    InvalidCaBundle(String),
    
    #[error("Invalid token: {0}")]
    InvalidToken(String),
}

// ============================================================================
// Runtime Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("Docker error: {0}")]
    Docker(String),
    
    #[error("Container not found: {0}")]
    ContainerNotFound(String),
    
    #[error("Image pull failed: {0}")]
    ImagePullFailed(String),
    
    #[error("Container start failed: {0}")]
    StartFailed(String),
    
    #[error("Container stop failed: {0}")]
    StopFailed(String),
    
    #[error("Container create failed: {0}")]
    CreateFailed(String),
    
    #[error("Cgroup not found: {0}")]
    CgroupNotFound(String),
    
    #[error("Ephemeral disk exceeded")]
    EphemeralDiskExceeded,
    
    #[error("Runtime unavailable")]
    Unavailable,
}

// ============================================================================
// Agent Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Registration rejected: {0}")]
    RegistrationRejected(String),
    
    #[error("Invalid join token")]
    InvalidJoinToken,
    
    #[error("Node ID conflict: {0}")]
    NodeIdConflict(String),
    
    #[error("Command execution failed: {0}")]
    CommandExecutionFailed(String),
}

// ============================================================================
// Controller Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("Recovery in progress")]
    RecoveryInProgress,
    
    #[error("No eligible nodes")]
    NoEligibleNodes,
    
    #[error("Scheduling failed: {0}")]
    SchedulingFailed(String),
    
    #[error("Rollout halted")]
    RolloutHalted,
}

// ============================================================================
// Scheduling Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum SchedulingError {
    #[error("Insufficient resources: {0}")]
    InsufficientResources(String),
    
    #[error("No nodes in class: {0}")]
    NoNodesInClass(String),
    
    #[error("Port conflict: {0}")]
    PortConflict(u16),
    
    #[error("Volume not available: {0}")]
    VolumeNotAvailable(String),
    
    #[error("Node draining: {0}")]
    NodeDraining(String),
    
    #[error("Node unhealthy: {0}")]
    NodeUnhealthy(String),
}

// ============================================================================
// Authentication Errors
// ============================================================================

#[derive(Debug, Error)]
pub enum AuthenticationError {
    #[error("Unauthorized")]
    Unauthorized,
    
    #[error("Invalid token")]
    InvalidToken,
    
    #[error("Expired token")]
    ExpiredToken,
    
    #[error("Invalid CSRF token")]
    InvalidCsrfToken,
    
    #[error("Session expired")]
    SessionExpired,
}

// ============================================================================
// Workload failure reasons
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkloadFailureReason {
    ExitCode(i32),
    OomKilled,
    ImagePullFailed(String),
    StartTimeout,
    EphemeralDiskExceeded,
    RestartExhausted,
    RuntimeUnavailable,
    Unknown,
}

impl std::fmt::Display for WorkloadFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkloadFailureReason::ExitCode(code) => write!(f, "EXITED({})", code),
            WorkloadFailureReason::OomKilled => write!(f, "OOM_KILLED"),
            WorkloadFailureReason::ImagePullFailed(msg) => write!(f, "IMAGE_PULL_FAILED({})", msg),
            WorkloadFailureReason::StartTimeout => write!(f, "START_TIMEOUT"),
            WorkloadFailureReason::EphemeralDiskExceeded => write!(f, "EPHEMERAL_DISK_EXCEEDED"),
            WorkloadFailureReason::RestartExhausted => write!(f, "RESTART_EXHAUSTED"),
            WorkloadFailureReason::RuntimeUnavailable => write!(f, "RUNTIME_UNAVAILABLE"),
            WorkloadFailureReason::Unknown => write!(f, "UNKNOWN"),
        }
    }
}
