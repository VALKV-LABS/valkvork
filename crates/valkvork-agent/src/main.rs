//! Valkvork Agent
//!
//! Main entry point for the valkvork-agent binary.

use std::sync::Arc;
use tokio::sync::Mutex;
use valkvork_common::config::{load_agent_config, AgentConfig};
use valkvork_common::errors::ValkvorkError;
use valkvork_agent::agent::Agent;
use tracing::{info, error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
    
    info!("Starting Valkvork Agent");
    
    // Load configuration
    let config = match load_agent_config(&std::path::PathBuf::from("/etc/valkvork/agent.toml")) {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to load config: {}", e);
            return Err(e.into());
        }
    };
    
    // Load join token
    let join_token = match std::fs::read_to_string(&config.join_token_file) {
        Ok(token) => token.trim().to_string(),
        Err(e) => {
            error!("Failed to read join token: {}", e);
            return Err(ValkvorkError::from(e).into());
        }
    };
    
    // Create agent
    let agent = match Agent::new(config, join_token) {
        Ok(agent) => agent,
        Err(e) => {
            error!("Failed to create agent: {}", e);
            return Err(e.into());
        }
    };
    
    // Start agent
    if let Err(e) = agent.start().await {
        error!("Agent failed: {}", e);
        return Err(e.into());
    }
    
    Ok(())
}
