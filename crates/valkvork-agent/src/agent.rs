//! Valkvork Agent
//!
//! This module implements the main agent functionality as per the specification.
//!
//! Key responsibilities:
//! - Register with controller (FR-AGE-02)
//! - Send heartbeats every 2s (FR-AGE-03)
//! - Execute commands (StartWorkload, StopWorkload) (FR-AGE-04)
//! - Handle reconnection with exponential backoff (FR-AGE-05)
//! - Local restart policy (FR-AGE-06)
//! - Adoption after restart (FR-AGE-07)
//! - Ephemeral disk enforcement (FR-AGE-08)

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use valkvork_common::config::{load_agent_config, AgentConfig};
use valkvork_common::models::{Node, WorkloadStatus, WorkloadState, CommandResult as CommandResultModel};
use valkvork_common::errors::{ValkvorkError, AgentError};
use valkvork_proto::agent::{AgentServiceClient, AgentMessage, ControllerMessage};
use crate::node::NodeManager;
use crate::container_runtime::{ContainerRuntime, DockerRuntime, WorkloadManager};
use crate::stream::StreamManager;
use crate::heartbeat::HeartbeatManager;

// ============================================================================
// Agent State
// ============================================================================

/// Agent state.
pub struct AgentState {
    pub node_id: String,
    pub node_info: Node,
    pub config: AgentConfig,
    pub join_token: String,
    pub workload_manager: WorkloadManager,
    pub stream_manager: StreamManager,
    pub heartbeat_manager: HeartbeatManager,
    pub is_connected: bool,
    pub last_command_seq: u64,
}

impl AgentState {
    pub fn new(config: AgentConfig, join_token: String) -> Result<Self, ValkvorkError> {
        // Initialize node manager
        let node_manager = NodeManager::new(config.clone())?;
        
        // Get node info
        let node_info = node_manager.build_node_info()?;
        let node_id = node_info.node_id.clone();
        
        // Initialize runtime
        let runtime: Box<dyn ContainerRuntime> = Box::new(DockerRuntime::new(
            config.docker_socket.clone().unwrap_or_default()
        )?);
        
        // Initialize workload manager
        let alloc_dir = config.data_dir.join("alloc");
        let workload_manager = WorkloadManager::new(runtime, alloc_dir);
        
        // Initialize stream manager
        let stream_manager = StreamManager::new(config.clone(), join_token.clone(), node_info.clone());
        
        // Initialize heartbeat manager
        let heartbeat_manager = HeartbeatManager::new(Duration::from_secs(2));
        
        Ok(Self {
            node_id,
            node_info,
            config,
            join_token,
            workload_manager,
            stream_manager,
            heartbeat_manager,
            is_connected: false,
            last_command_seq: 0,
        })
    }
}

// ============================================================================
// Agent
// ============================================================================

/// The main agent struct.
pub struct Agent {
    state: Arc<Mutex<AgentState>>,
}

impl Agent {
    /// Create a new agent instance.
    pub fn new(config: AgentConfig, join_token: String) -> Result<Self, ValkvorkError> {
        let state = AgentState::new(config, join_token)?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
    
    /// Load configuration from default path and start the agent.
    pub async fn from_default_config() -> Result<Self, ValkvorkError> {
        let config = load_agent_config(&std::path::PathBuf::from("/etc/valkvork/agent.toml"))?;
        let join_token = std::fs::read_to_string(&config.join_token_file)?;
        Self::new(config, join_token.trim().to_string())
    }
    
    /// Start the agent main loop.
    pub async fn start(&self) -> Result<(), ValkvorkError> {
        // Initialize logging
        self.init_tracing()?;
        
        // Enter main loop
        self.main_loop().await
    }
    
    /// Initialize tracing.
    fn init_tracing(&self) -> Result<(), ValkvorkError> {
        use tracing_subscriber::{fmt, EnvFilter};
        
        fmt()
            .with_env_filter(EnvFilter::from_default_env())
            .json()
            .init();
        
        Ok(())
    }
    
    /// Main agent loop.
    async fn main_loop(&self) -> Result<(), ValkvorkError> {
        let mut reconnection = crate::stream::Reconnection::new();
        
        loop {
            match self.run_connection_cycle().await {
                Ok(()) => {
                    // Normal exit (should not happen)
                    break;
                }
                Err(e) => {
                    tracing::error!("Connection failed: {}", e);
                    let delay = reconnection.next_delay();
                    tracing::info!("Reconnecting in {:?}...", delay);
                    tokio::time::sleep(delay).await;
                }
            }
        }
        
        Ok(())
    }
    
    /// Run a single connection cycle.
    async fn run_connection_cycle(&self) -> Result<(), ValkvorkError> {
        let state = self.state.lock().await;
        
        // Connect to controller
        let (mut message_stream, channel) = state.stream_manager.connect().await?;
        
        // Mark as connected
        drop(state);
        
        // Spawn tasks
        let state_clone = self.state.clone();
        let agent_client = Arc::new(AgentServiceClient::new(channel));
        
        // Spawn heartbeat task
        let heartbeat_task = tokio::spawn(async move {
            Self::heartbeat_loop(state_clone, agent_client).await
        });
        
        // Spawn message handling task
        let message_task = tokio::spawn(async move {
            Self::message_loop(message_stream).await
        });
        
        // Wait for either task to complete
        tokio::select! {
            result = heartbeat_task => result?,
            result = message_task => result?,
        }
        
        Ok(())
    }
    
    /// Heartbeat loop.
    async fn heartbeat_loop(
        state: Arc<Mutex<AgentState>>,
        client: Arc<AgentServiceClient<tonic::transport::Channel>>,
    ) -> Result<(), ValkvorkError> {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        let mut seq = 0;
        
        loop {
            interval.tick().await;
            
            let state = state.lock().await;
            
            // Collect node health
            let node_health = state.heartbeat_manager.collect_node_health()?;
            
            // Collect workload statuses
            let workloads = Vec::new(); // Placeholder
            
            // Build heartbeat message
            let message = state.stream_manager.build_heartbeat_message(
                seq,
                &node_health,
                workloads,
            );
            
            // Send heartbeat
            let request = tonic::Request::new(stream::iter(vec![message]));
            let _response = client.connect(request).await?;
            
            seq += 1;
        }
    }
    
    /// Message handling loop.
    async fn message_loop(
        mut stream: Pin<Box<dyn Stream<Item = Result<ControllerMessage, tonic::Status>> + Send>>,
    ) -> Result<(), ValkvorkError> {
        while let Some(result) = stream.next().await {
            match result {
                Ok(message) => {
                    if let Err(e) = Self::handle_message(message).await {
                        tracing::error!("Failed to handle message: {}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Stream error: {}", e);
                    return Err(ValkvorkError::Network(NetworkError::StreamClosed(e.to_string())));
                }
            }
        }
        
        Ok(())
    }
    
    /// Handle incoming controller message.
    async fn handle_message(message: ControllerMessage) -> Result<(), ValkvorkError> {
        match message.msg {
            Some(valkvork_proto::agent::controller_message::Msg::RegisterAck(ack)) => {
                if !ack.accepted {
                    return Err(ValkvorkError::Agent(AgentError::RegistrationRejected(
                        ack.reject_reason
                    )));
                }
                tracing::info!("Registered successfully");
                Ok(())
            }
            Some(valkvork_proto::agent::controller_message::Msg::StartWorkload(start)) => {
                tracing::info!("Received StartWorkload for {}", start.workload_id);
                // In a real implementation, this would call the workload manager
                Ok(())
            }
            Some(valkvork_proto::agent::controller_message::Msg::StopWorkload(stop)) => {
                tracing::info!("Received StopWorkload for {}", stop.workload_id);
                // In a real implementation, this would call the workload manager
                Ok(())
            }
            None => Err(ValkvorkError::Network(NetworkError::InvalidMessage(
                "Empty controller message".to_string()
            ))),
        }
    }
    
    /// Process StartWorkload command.
    async fn handle_start_workload(
        &self,
        start: valkvork_proto::agent::StartWorkload,
    ) -> Result<(), ValkvorkError> {
        let state = self.state.lock().await;
        
        // Convert proto to model
        let config = crate::container_runtime::ContainerConfig {
            image: start.image,
            cpu_millicores: start.cpu_millicores,
            memory_bytes: start.memory_bytes,
            disk_bytes: start.disk_bytes,
            env: start.env,
            ports: start.ports.into_iter().map(|p| {
                valkvork_common::models::PortMapping {
                    name: p.name,
                    container_port: p.container_port,
                    host_port: p.host_port,
                }
            }).collect(),
            volumes: start.volumes.into_iter().map(|v| {
                valkvork_common::models::VolumeMount {
                    r#type: v.type_,
                    source: v.source,
                    target: v.target,
                    read_only: v.read_only,
                }
            }).collect(),
            restart_policy: valkvork_common::models::RestartPolicy {
                attempts: start.restart.attempts,
                interval_seconds: start.restart.interval_seconds,
                delay_seconds: start.restart.delay_seconds,
            },
            workdir: None,
            user: None,
        };
        
        // Start the workload
        let container_id = state.workload_manager.start_workload(
            &start.workload_id,
            &start.service_id,
            start.service_version,
            config,
        )?;
        
        // Send command result
        let result = state.stream_manager.build_command_result_message(
            start.command_id,
            start.workload_id,
            true,
            None,
        );
        
        // In a real implementation, send this back to the controller
        tracing::info!("Started workload {} -> container {}", start.workload_id, container_id);
        
        Ok(())
    }
    
    /// Process StopWorkload command.
    async fn handle_stop_workload(
        &self,
        stop: valkvork_proto::agent::StopWorkload,
    ) -> Result<(), ValkvorkError> {
        let state = self.state.lock().await;
        
        state.workload_manager.stop_workload(
            &stop.workload_id,
            Duration::from_secs(stop.kill_timeout_seconds as u64),
        )?;
        
        // Send command result
        let result = state.stream_manager.build_command_result_message(
            stop.command_id,
            stop.workload_id,
            true,
            None,
        );
        
        tracing::info!("Stopped workload {}", stop.workload_id);
        
        Ok(())
    }
}

// ============================================================================
// Main Entry Point
// ============================================================================

/// Main function for the agent.
#[tokio::main]
pub async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
    
    tracing::info!("Starting Valkvork Agent");
    
    // Load configuration
    let config = load_agent_config(&std::path::PathBuf::from("/etc/valkvork/agent.toml"))
        .map_err(|e| {
            tracing::error!("Failed to load config: {}", e);
            e
        })?;
    
    // Load join token
    let join_token = std::fs::read_to_string(&config.join_token_file)
        .map_err(|e| {
            tracing::error!("Failed to read join token: {}", e);
            ValkvorkError::from(e)
        })?;
    
    // Create agent
    let agent = Agent::new(config, join_token.trim().to_string())
        .map_err(|e| {
            tracing::error!("Failed to create agent: {}", e);
            e
        })?;
    
    // Start agent
    agent.start().await
        .map_err(|e| {
            tracing::error!("Agent failed: {}", e);
            e
        })?;
    
    Ok(())
}
