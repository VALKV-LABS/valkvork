//! Configuration types for Valkvork components.
//!
//! This module provides configuration structures for the controller, agent, and CLI.

use std::net::SocketAddr;
use std::path::PathBuf;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

// ============================================================================
// Shared Configuration
// ============================================================================

/// TLS configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub enabled: bool,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub client_auth: bool,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cert_file: None,
            key_file: None,
            ca_file: None,
            client_auth: false,
        }
    }
}

// ============================================================================
// Controller Configuration
// ============================================================================

/// Controller configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerConfig {
    /// Address to listen on for agent connections (gRPC)
    pub agent_listen_addr: SocketAddr,
    
    /// Address to listen on for admin API and UI
    pub admin_listen_addr: SocketAddr,
    
    /// TLS configuration for agent connections
    pub agent_tls: TlsConfig,
    
    /// TLS configuration for admin API
    pub admin_tls: TlsConfig,
    
    /// Path to SQLite database
    pub database_path: PathBuf,
    
    /// Join token for agent authentication
    pub join_token: String,
    
    /// Admin token for CLI/UI authentication
    pub admin_token: String,
    
    /// Node unhealthy timeout in seconds
    pub node_unhealthy_timeout_seconds: u64,
    
    /// Recovery grace period in seconds
    pub recovery_grace_period_seconds: u64,
    
    /// Heartbeat interval in milliseconds (for agents)
    pub heartbeat_interval_ms: u64,
    
    /// Command timeout in seconds
    pub command_timeout_seconds: u64,
    
    /// Start timeout in seconds
    pub start_timeout_seconds: u64,
    
    /// UI configuration
    pub ui: UiConfig,
    
    /// Metrics configuration
    pub metrics: MetricsConfig,
    
    /// Autoscaler configuration
    pub autoscaler: AutoscalerConfig,
    
    /// Scheduler configuration
    pub scheduler: SchedulerConfig,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            agent_listen_addr: "0.0.0.0:50051".parse().unwrap(),
            admin_listen_addr: "0.0.0.0:50052".parse().unwrap(),
            agent_tls: TlsConfig::default(),
            admin_tls: TlsConfig::default(),
            database_path: PathBuf::from("/var/lib/valkvork/db/valkvork.db"),
            join_token: String::new(),
            admin_token: String::new(),
            node_unhealthy_timeout_seconds: 10,
            recovery_grace_period_seconds: 30,
            heartbeat_interval_ms: 2000,
            command_timeout_seconds: 30,
            start_timeout_seconds: 300, // 5 minutes
            ui: UiConfig::default(),
            metrics: MetricsConfig::default(),
            autoscaler: AutoscalerConfig::default(),
            scheduler: SchedulerConfig::default(),
        }
    }
}

/// UI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub enabled: bool,
    pub assets_path: Option<PathBuf>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            assets_path: None,
        }
    }
}

/// Metrics configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub listen_addr: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            listen_addr: "0.0.0.0:9400".parse().unwrap(),
        }
    }
}

/// Autoscaler configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoscalerConfig {
    pub enabled: bool,
    pub evaluation_interval_seconds: u64,
    pub cooldown_seconds: u64,
    pub scale_up_threshold: f64,  // multiplier (e.g., 1.1 = 10% above target)
    pub scale_down_threshold: f64, // multiplier (e.g., 0.9 = 10% below target)
    pub min_replicas: u32,
    pub max_replicas: u32,
}

impl Default for AutoscalerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            evaluation_interval_seconds: 10,
            cooldown_seconds: 180, // 3 minutes
            scale_up_threshold: 1.1,
            scale_down_threshold: 0.9,
            min_replicas: 1,
            max_replicas: 100,
        }
    }
}

/// Scheduler configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    pub max_parallel: u32,
    pub min_healthy_time_seconds: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_parallel: 1,
            min_healthy_time_seconds: 10,
        }
    }
}

// ============================================================================
// Agent Configuration
// ============================================================================

/// Agent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Controller endpoint (e.g., "https://controlplane.valkvork.internal:50051")
    pub controller_endpoint: String,
    
    /// CA certificate file path
    pub ca_file: PathBuf,
    
    /// Join token file path
    pub join_token_file: PathBuf,
    
    /// Node class
    pub node_class: String,
    
    /// Data directory
    pub data_dir: PathBuf,
    
    /// Runtime driver
    pub runtime: String,
    
    /// Docker socket path (if runtime is docker)
    pub docker_socket: Option<PathBuf>,
    
    /// Reserved resources
    pub reserved: ReservedResources,
    
    /// Host volumes
    pub host_volumes: HashMap<String, HostVolume>,
    
    /// Registry authentication
    pub registry_auth: Option<RegistryAuth>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            controller_endpoint: String::new(),
            ca_file: PathBuf::from("/etc/valkvork/ca.pem"),
            join_token_file: PathBuf::from("/etc/valkvork/join-token"),
            node_class: "default".to_string(),
            data_dir: PathBuf::from("/var/lib/valkvork"),
            runtime: "docker".to_string(),
            docker_socket: Some(PathBuf::from("/var/run/docker.sock")),
            reserved: ReservedResources::default(),
            host_volumes: HashMap::new(),
            registry_auth: None,
        }
    }
}

/// Reserved resources configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservedResources {
    pub cpu: f64,
    pub memory: String, // e.g., "384MiB"
}

impl Default for ReservedResources {
    fn default() -> Self {
        Self {
            cpu: 0.25,
            memory: "384MiB".to_string(),
        }
    }
}

/// Host volume configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostVolume {
    pub path: PathBuf,
    pub read_only: bool,
}

/// Registry authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryAuth {
    pub username: String,
    pub password: String,
}

// ============================================================================
// CLI Configuration
// ============================================================================

/// CLI configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    /// Controller endpoint
    pub controller_endpoint: String,
    
    /// Admin token
    pub admin_token: Option<String>,
    
    /// TLS configuration
    pub tls: TlsConfig,
    
    /// Output format
    pub output_format: String,
    
    /// Verbose mode
    pub verbose: bool,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            controller_endpoint: "https://localhost:50052".to_string(),
            admin_token: None,
            tls: TlsConfig::default(),
            output_format: "table".to_string(),
            verbose: false,
        }
    }
}

// ============================================================================
// Configuration loading
// ============================================================================

use std::fs;
use std::io::Read;
use crate::errors::{ConfigError, ValkvorkError};

/// Load controller configuration from TOML file
pub fn load_controller_config(path: &PathBuf) -> Result<ControllerConfig, ValkvorkError> {
    let content = fs::read_to_string(path)?;
    let config: ControllerConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Load agent configuration from TOML file
pub fn load_agent_config(path: &PathBuf) -> Result<AgentConfig, ValkvorkError> {
    let content = fs::read_to_string(path)?;
    let config: AgentConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Load CLI configuration from environment or config file
pub fn load_cli_config() -> Result<CliConfig, ValkvorkError> {
    // Try to load from ~/.config/valkvork/config.toml
    let home = std::env::var("HOME").unwrap_or_default();
    let config_path = PathBuf::from(format!("{}/.config/valkvork/config.toml", home));
    
    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let config: CliConfig = toml::from_str(&content)?;
        Ok(config)
    } else {
        Ok(CliConfig::default())
    }
}

/// Load join token from file
pub fn load_join_token(path: &PathBuf) -> Result<String, ValkvorkError> {
    let token = fs::read_to_string(path)?;
    Ok(token.trim().to_string())
}

/// Load admin token from file
pub fn load_admin_token(path: &PathBuf) -> Result<String, ValkvorkError> {
    let token = fs::read_to_string(path)?;
    Ok(token.trim().to_string())
}
