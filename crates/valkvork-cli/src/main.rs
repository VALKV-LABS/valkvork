//! Valkvork CLI
//!
//! Main entry point for the valkvork CLI binary.
//!
//! As per FR-CON-10, CLI commands include:
//! - apply -f
//! - plan -f
//! - delete service <id>
//! - scale service <id> <n>
//! - status [service]
//! - workloads [--service]
//! - events [--kind]
//! - node list
//! - node drain <id>
//! - node undrain <id>

use clap::{Parser, Subcommand};
use std::process;
use valkvork_common::errors::{ValkvorkError, ValidationError};

mod client;
mod commands;

use client::Client;
use commands::*;

/// Valkvork CLI
#[derive(Debug, Parser)]
#[command(name = "valkvork")]
#[command(version = "0.1.0")]
#[command(about = "Valkvork workload orchestrator CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    
    /// Controller endpoint
    #[arg(long, env = "VALKVORK_CONTROLLER_ENDPOINT", default_value = "https://localhost:50052")]
    endpoint: String,
    
    /// Admin token
    #[arg(long, env = "VALKVORK_ADMIN_TOKEN")]
    token: Option<String>,
    
    /// Output format (table, json)
    #[arg(long, default_value = "table")]
    output: String,
    
    /// Verbose mode
    #[arg(short, long)]
    verbose: bool,
}

/// CLI commands
#[derive(Debug, Subcommand)]
enum Commands {
    /// Apply a manifest
    #[command(about = "Apply a service manifest")]
    Apply {
        /// Path to manifest file
        #[arg(short, long)]
        file: String,
    },
    
    /// Plan a manifest (dry-run)
    #[command(about = "Plan a service manifest (dry-run)")]
    Plan {
        /// Path to manifest file
        #[arg(short, long)]
        file: String,
    },
    
    /// Delete a service
    #[command(about = "Delete a service")]
    Delete {
        /// Service ID
        service: String,
    },
    
    /// Scale a service
    #[command(about = "Scale a service to N replicas")]
    Scale {
        /// Service ID
        service: String,
        /// Number of replicas
        replicas: u32,
    },
    
    /// Get service status
    #[command(about = "Get service or cluster status")]
    Status {
        /// Service ID (optional, defaults to cluster status)
        service: Option<String>,
    },
    
    /// List workloads
    #[command(about = "List workloads")]
    Workloads {
        /// Filter by service ID
        #[arg(long)]
        service: Option<String>,
        /// Filter by node ID
        #[arg(long)]
        node: Option<String>,
        /// Filter by status
        #[arg(long)]
        status: Option<String>,
    },
    
    /// List events
    #[command(about = "List cluster events")]
    Events {
        /// Filter by kind (NODE, WORKLOAD, DEPLOY, SCALE, AUDIT)
        #[arg(long)]
        kind: Option<String>,
        /// Show events since this timestamp (Unix milliseconds)
        #[arg(long)]
        since: Option<u64>,
        /// Maximum number of events
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    
    /// Node operations
    #[command(subcommand)]
    Node(NodeCommands),
}

/// Node commands
#[derive(Debug, Subcommand)]
enum NodeCommands {
    /// List nodes
    #[command(about = "List nodes")]
    List,
    
    /// Drain a node
    #[command(about = "Drain a node")]
    Drain {
        /// Node ID
        node_id: String,
    },
    
    /// Undrain a node
    #[command(about = "Undrain a node")]
    Undrain {
        /// Node ID
        node_id: String,
    },
}

#[tokio::main]
async fn main() {
    // Initialize logging
    init_tracing();
    
    let cli = Cli::parse();
    
    // Create client
    let client = match Client::new(&cli.endpoint, cli.token.as_deref()) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("Error: Failed to create client: {}", e);
            process::exit(1);
        }
    };
    
    // Execute command
    let result = match cli.command {
        Commands::Apply { file } => cmd_apply(&client, &file).await,
        Commands::Plan { file } => cmd_plan(&client, &file).await,
        Commands::Delete { service } => cmd_delete(&client, &service).await,
        Commands::Scale { service, replicas } => cmd_scale(&client, &service, replicas).await,
        Commands::Status { service } => cmd_status(&client, service.as_deref()).await,
        Commands::Workloads { service, node, status } => {
            cmd_workloads(&client, service.as_deref(), node.as_deref(), status.as_deref()).await
        }
        Commands::Events { kind, since, limit } => {
            cmd_events(&client, kind.as_deref(), since, limit).await
        }
        Commands::Node(cmd) => match cmd {
            NodeCommands::List => cmd_node_list(&client).await,
            NodeCommands::Drain { node_id } => cmd_node_drain(&client, &node_id).await,
            NodeCommands::Undrain { node_id } => cmd_node_undrain(&client, &node_id).await,
        },
    };
    
    // Handle result
    match result {
        Ok(_) => process::exit(0),
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(match e {
                ValkvorkError::Network(_) => 1,
                ValkvorkError::Validation(_) => 2,
                _ => 1,
            });
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
}
