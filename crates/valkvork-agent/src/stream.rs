//! Agent stream management.
//!
//! This module handles the gRPC stream connection between the agent and controller.
//!
//! As per FR-ARC-01, agents MUST open connections outbound only.

use std::pin::Pin;
use std::time::Duration;
use futures::{Stream, StreamExt, SinkExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tonic::transport::{Channel, ClientTlsConfig};
use valkvork_proto::agent::{AgentServiceClient, AgentMessage, ControllerMessage, Register, Heartbeat, CommandResult};
use valkvork_common::models::{Node, WorkloadStatus, NodeHealth};
use valkvork_common::config::AgentConfig;
use valkvork_common::errors::{ValkvorkError, NetworkError, TlsError};
use valkvork_common::tls::load_client_config;

// ============================================================================
// Stream Manager
// ============================================================================

/// Manages the agent's connection to the controller.
pub struct StreamManager {
    config: AgentConfig,
    join_token: String,
    node_info: Node,
}

impl StreamManager {
    pub fn new(config: AgentConfig, join_token: String, node_info: Node) -> Self {
        Self {
            config,
            join_token,
            node_info,
        }
    }
    
    /// Connect to the controller and establish the stream.
    pub async fn connect(
        &self,
    ) -> Result<(
        Pin<Box<dyn Stream<Item = Result<ControllerMessage, tonic::Status>> + Send>>,
        tonic::transport::Channel,
    ), ValkvorkError> {
        // Build TLS configuration
        let tls_config = load_client_config(Some(self.config.ca_file.as_path()))?;
        
        // Create TLS connector
        let tls_connector = TlsConnector::from(Arc::new(tls_config));
        
        // Parse controller endpoint
        let endpoint = self.config.controller_endpoint.clone();
        let endpoint = tonic::transport::Endpoint::from_shared(endpoint)?
            .tls_config(ClientTlsConfig::new().ca_certificate(tls_config.root_certificates()))?;
        
        // Connect to the controller
        let channel = endpoint.connect().await?;
        
        // Create the agent service client
        let mut client = AgentServiceClient::new(channel.clone());
        
        // Open the bidirectional stream
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        
        let response_stream = client.connect(stream::iter(vec![self.build_register_message()?]));
        
        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        
        let response_future = response_stream.await?;
        
        // This is a simplified implementation
        // In practice, we need to handle the bidirectional stream properly
        
        // For now, return a placeholder
        let empty_stream = tokio_stream::empty();
        
        Ok((Box::pin(empty_stream), channel))
    }
    
    /// Build the initial register message.
    fn build_register_message(&self) -> Result<AgentMessage, ValkvorkError> {
        let register = Register {
            node_id: self.node_info.node_id.clone(),
            join_token: self.join_token.clone(),
            hostname: self.node_info.hostname.clone(),
            ip_address: self.node_info.ip_address.clone(),
            node_class: self.node_info.node_class.clone(),
            agent_version: self.node_info.agent_version.clone(),
            allocatable_cpu_millicores: self.node_info.allocatable_cpu_millicores,
            allocatable_memory_bytes: self.node_info.allocatable_memory_bytes,
            host_volumes: self.node_info.host_volumes.clone(),
            inventory: Vec::new(), // Will be populated with existing containers
            runtime: self.node_info.runtime.clone(),
        };
        
        Ok(AgentMessage::Register(register))
    }
    
    /// Build a heartbeat message.
    pub fn build_heartbeat_message(
        &self,
        seq: u64,
        node_health: &NodeHealth,
        workloads: Vec<WorkloadStatus>,
    ) -> AgentMessage {
        let heartbeat = Heartbeat {
            seq,
            node_cpu_usage_percent: node_health.cpu_usage_percent,
            node_used_memory_bytes: node_health.memory_used_bytes,
            workloads,
        };
        
        AgentMessage::Heartbeat(heartbeat)
    }
    
    /// Build a command result message.
    pub fn build_command_result_message(
        &self,
        command_id: String,
        workload_id: String,
        accepted: bool,
        error: Option<String>,
    ) -> AgentMessage {
        let result = CommandResult {
            command_id,
            workload_id,
            accepted,
            error: error.unwrap_or_default(),
        };
        
        AgentMessage::CommandResult(result)
    }
}

// ============================================================================
// Reconnection
// ============================================================================

/// Reconnection strategy with exponential backoff.
/// 
/// As per FR-AGE-05, reconnects MUST use exponential backoff: start at 1 s,
/// double each attempt, cap at 15 s, ±20% jitter.
pub struct Reconnection {
    base_delay: Duration,
    max_delay: Duration,
    jitter_factor: f64,
    attempt: u32,
}

impl Default for Reconnection {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(15),
            jitter_factor: 0.2,
            attempt: 0,
        }
    }
}

impl Reconnection {
    pub fn new() -> Self {
        Self::default()
    }
    
    /// Calculate the next delay.
    pub fn next_delay(&mut self) -> Duration {
        // Calculate base delay with exponential backoff
        let delay_secs = std::cmp::min(
            self.base_delay.as_secs() * 2u64.pow(self.attempt),
            self.max_delay.as_secs(),
        );
        
        // Apply jitter (±20%)
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let jitter = (rng.gen_range(0.0..1.0) * 2.0 - 1.0) * self.jitter_factor;
        let delay = (delay_secs as f64 * (1.0 + jitter)).max(0.0) as u64;
        
        self.attempt += 1;
        Duration::from_secs(delay)
    }
    
    /// Reset the attempt counter.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
    
    /// Get current attempt number.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

// ============================================================================
// Message Handler
// ============================================================================

/// Handles incoming controller messages.
pub struct MessageHandler;

impl MessageHandler {
    pub fn handle_controller_message(
        message: ControllerMessage,
    ) -> Result<(), ValkvorkError> {
        match message.msg {
            Some(valkvork_proto::agent::controller_message::Msg::RegisterAck(ack)) => {
                if !ack.accepted {
                    return Err(ValkvorkError::Agent(AgentError::RegistrationRejected(
                        ack.reject_reason
                    )));
                }
                Ok(())
            }
            Some(valkvork_proto::agent::controller_message::Msg::StartWorkload(start)) => {
                // Handle StartWorkload command
                // This will be processed by the workload manager
                Ok(())
            }
            Some(valkvork_proto::agent::controller_message::Msg::StopWorkload(stop)) => {
                // Handle StopWorkload command
                Ok(())
            }
            None => Err(ValkvorkError::Network(NetworkError::InvalidMessage(
                "Empty controller message".to_string()
            ))),
        }
    }
}
