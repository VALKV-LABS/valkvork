//! Valkvork Controller
//!
//! Main entry point for the valkvork-controller binary.

use std::path::PathBuf;
use valkvork_common::config::{load_controller_config, ControllerConfig};
use valkvork_common::errors::ValkvorkError;
use valkvork_controller::server::run_server;
use tracing::{info, error, debug};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    init_tracing();
    
    info!("Starting Valkvork Controller");
    
    // Load configuration
    let config_path = std::env::var("VALKVORK_CONTROLLER_CONFIG")
        .unwrap_or_else(|_| "/etc/valkvork/controller.toml".to_string());
    
    let config = match load_controller_config(&PathBuf::from(config_path)) {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to load config: {}", e);
            return Err(e.into());
        }
    };
    
    debug!("Configuration loaded: agent_listen={}, admin_listen={}", 
           config.agent_listen_addr, config.admin_listen_addr);
    
    // Validate configuration
    if config.join_token.is_empty() {
        error!("Join token must be configured");
        return Err("Join token must be configured".into());
    }
    
    if config.admin_token.is_empty() {
        error!("Admin token must be configured");
        return Err("Admin token must be configured".into());
    }
    
    // Run server
    if let Err(e) = run_server(config).await {
        error!("Server failed: {}", e);
        return Err(e.into());
    }
    
    Ok(())
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
}
