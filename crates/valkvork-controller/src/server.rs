//! gRPC server for the Valkvork controller.
//!
//! This module implements the AgentService and AdminService gRPC servers.

use std::net::SocketAddr;
use std::sync::Arc;
use tonic::transport::Server;
use tokio::sync::Mutex;
use valkvork_proto::agent::AgentServiceServer;
use valkvork_proto::admin::AdminServiceServer;
use valkvork_common::config::ControllerConfig;
use valkvork_common::errors::ValkvorkError;
use crate::agent::AgentServiceImpl;
use crate::admin::AdminServiceImpl;
use crate::state::ClusterState;

// ============================================================================
// Server Implementation
// ============================================================================

/// gRPC server for the controller.
pub struct ControllerServer {
    config: ControllerConfig,
    state: Arc<Mutex<ClusterState>>,
}

impl ControllerServer {
    pub fn new(config: ControllerConfig, state: ClusterState) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(state)),
        }
    }
    
    /// Start the gRPC servers.
    pub async fn start(&self) -> Result<(), ValkvorkError> {
        // Create state arc for sharing
        let state = self.state.clone();
        
        // Create service implementations
        let agent_service = AgentServiceImpl::new(self.config.clone(), state.clone());
        let admin_service = AdminServiceImpl::new(self.config.clone(), state);
        
        // Build agent server
        let agent_server = Server::builder()
            .add_service(AgentServiceServer::new(agent_service))
            .serve(self.config.agent_listen_addr);
        
        // Build admin server
        let admin_server = Server::builder()
            .add_service(AdminServiceServer::new(admin_service))
            .serve(self.config.admin_listen_addr);
        
        // Run both servers concurrently
        tokio::select! {
            result = agent_server => {
                if let Err(e) = result {
                    return Err(ValkvorkError::Network(NetworkError::Grpc(e)));
                }
            }
            result = admin_server => {
                if let Err(e) = result {
                    return Err(ValkvorkError::Network(NetworkError::Grpc(e)));
                }
            }
        }
        
        Ok(())
    }
}

// ============================================================================
// Agent Service Implementation
// ============================================================================

pub mod agent {
    use super::*;
    use valkvork_proto::agent::{AgentMessage, ControllerMessage, Register, RegisterAck, Heartbeat, CommandResult as ProtoCommandResult};
    use valkvork_common::models::Node;
    use valkvork_common::errors::AgentError;
    use tonic::{Request, Response, Status};
    use tokio_stream::{StreamExt, Stream};
    use std::pin::Pin;
    
    /// AgentService implementation.
    pub struct AgentServiceImpl {
        config: ControllerConfig,
        state: Arc<Mutex<ClusterState>>,
    }
    
    impl AgentServiceImpl {
        pub fn new(config: ControllerConfig, state: Arc<Mutex<ClusterState>>) -> Self {
            Self { config, state }
        }
    }
    
    #[tonic::async_trait]
    impl valkvork_proto::agent::AgentService for AgentServiceImpl {
        type ConnectStream = Pin<Box<dyn Stream<Item = Result<ControllerMessage, Status>> + Send>>;
        
        async fn connect(
            &self,
            request: Request<tonic::Streaming<AgentMessage>>,
        ) -> Result<Response<Self::ConnectStream>, Status> {
            let (tx, rx) = tokio::sync::mpsc::channel(100);
            
            // Spawn a task to handle the incoming stream
            tokio::spawn(async move {
                let mut in_stream = request.into_inner();
                let tx = tx;
                
                while let Some(result) = in_stream.next().await {
                    match result {
                        Ok(message) => {
                            if let Err(e) = Self::handle_message(&self, message).await {
                                tracing::error!("Error handling agent message: {}", e);
                                // TODO: Close the connection
                            }
                        }
                        Err(e) => {
                            tracing::error!("Stream error: {}", e);
                            break;
                        }
                    }
                }
            });
            
            let out_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            Ok(Response::new(Box::pin(out_stream)))
        }
    }
    
    impl AgentServiceImpl {
        async fn handle_message(
            &self,
            message: AgentMessage,
        ) -> Result<(), Status> {
            match message.msg {
                Some(agent::agent_message::Msg::Register(register)) => {
                    self.handle_register(register).await?;
                }
                Some(agent::agent_message::Msg::Heartbeat(heartbeat)) => {
                    self.handle_heartbeat(heartbeat).await?;
                }
                Some(agent::agent_message::Msg::CommandResult(result)) => {
                    self.handle_command_result(result).await?;
                }
                None => return Err(Status::invalid_argument("Empty message")),
            }
            
            Ok(())
        }
        
        async fn handle_register(&self, register: Register) -> Result<(), Status> {
            // Validate join token
            if register.join_token != self.config.join_token {
                let ack = RegisterAck {
                    accepted: false,
                    reject_reason: "Invalid join token".to_string(),
                    heartbeat_interval_ms: self.config.heartbeat_interval_ms as u32,
                };
                // TODO: Send ack back
                return Err(Status::permission_denied("Invalid join token"));
            }
            
            // Create node
            let node = Node::new(
                register.node_id,
                register.hostname,
                register.ip_address,
                register.node_class,
                register.agent_version,
                register.runtime,
                register.allocatable_cpu_millicores,
                register.allocatable_memory_bytes,
                register.host_volumes,
            );
            
            // Add to state
            let state = self.state.lock().await;
            state.upsert_node(node);
            
            // TODO: Handle inventory
            
            // Send ack
            let ack = RegisterAck {
                accepted: true,
                reject_reason: String::new(),
                heartbeat_interval_ms: self.config.heartbeat_interval_ms as u32,
            };
            
            // TODO: Send ack back through the stream
            
            tracing::info!("Node {} registered", register.node_id);
            Ok(())
        }
        
        async fn handle_heartbeat(&self, heartbeat: Heartbeat) -> Result<(), Status> {
            // Update node heartbeat timestamp
            // In a real implementation, we'd track the node and update its heartbeat
            tracing::debug!("Heartbeat seq={} from node", heartbeat.seq);
            
            // Process workload statuses
            for workload_status in heartbeat.workloads {
                // TODO: Update workload status in state
            }
            
            Ok(())
        }
        
        async fn handle_command_result(&self, result: ProtoCommandResult) -> Result<(), Status> {
            tracing::debug!("Command result for {}: accepted={}", 
                           result.workload_id, result.accepted);
            
            // In a real implementation, update command tracking
            
            Ok(())
        }
    }
}

// ============================================================================
// Admin Service Implementation
// ============================================================================

pub mod admin {
    use super::*;
    use valkvork_proto::admin::*;
    use valkvork_common::validation::{parse_manifest_str, spec_to_toml};
    use tonic::{Request, Response, Status};
    
    /// AdminService implementation.
    pub struct AdminServiceImpl {
        config: ControllerConfig,
        state: Arc<Mutex<ClusterState>>,
    }
    
    impl AdminServiceImpl {
        pub fn new(config: ControllerConfig, state: Arc<Mutex<ClusterState>>) -> Self {
            Self { config, state }
        }
    }
    
    #[tonic::async_trait]
    impl valkvork_proto::admin::AdminService for AdminServiceImpl {
        async fn apply(
            &self,
            request: Request<ApplyRequest>,
        ) -> Result<Response<ApplyResponse>, Status> {
            // Parse and validate manifest
            let manifest = request.into_inner().manifest_toml;
            
            let spec = match parse_manifest_str(&manifest) {
                Ok(spec) => spec,
                Err(errors) => {
                    let error = errors.first().unwrap();
                    return Err(Status::invalid_argument(format!(
                        "{}: {}", error.code, error.message
                    )));
                }
            };
            
            // In a real implementation, this would call the writer task
            // to persist the service and update state
            
            // For now, return a mock response
            let response = ApplyResponse {
                service_id: spec.id,
                version: 1,
                change: "created".to_string(),
                warnings: vec![],
            };
            
            Ok(Response::new(response))
        }
        
        async fn plan(
            &self,
            request: Request<ApplyRequest>,
        ) -> Result<Response<PlanResponse>, Status> {
            let manifest = request.into_inner().manifest_toml;
            
            let spec = match parse_manifest_str(&manifest) {
                Ok(spec) => spec,
                Err(errors) => {
                    let error = errors.first().unwrap();
                    return Err(Status::invalid_argument(format!(
                        "{}: {}", error.code, error.message
                    )));
                }
            };
            
            // In a real implementation, check feasibility
            
            let response = PlanResponse {
                service_id: spec.id,
                version: 1,
                change: "created".to_string(),
                feasible: true,
                reason: String::new(),
                warnings: vec![],
            };
            
            Ok(Response::new(response))
        }
        
        async fn delete_service(
            &self,
            request: Request<DeleteServiceRequest>,
        ) -> Result<Response<DeleteServiceResponse>, Status> {
            let service_id = request.into_inner().service_id;
            
            // In a real implementation, mark service for deletion
            
            let response = DeleteServiceResponse {
                accepted: true,
                error: None,
            };
            
            Ok(Response::new(response))
        }
        
        async fn get_service(
            &self,
            request: Request<GetServiceRequest>,
        ) -> Result<Response<ServiceDetail>, Status> {
            let service_id = request.into_inner().service_id;
            
            // In a real implementation, get service from state
            
            // For now, return a mock response
            let response = ServiceDetail {
                service_id: service_id.clone(),
                version: 1,
                spec_toml: format!("[service]\nid = \"{}\"", service_id),
                status: "ACTIVE".to_string(),
                rollout_status: "NONE".to_string(),
                target_replicas: 1,
                running_replicas: 0,
                pending_replicas: 0,
                failed_replicas: 0,
                workloads: vec![],
                scaling_events: vec![],
                created_at: 0,
                updated_at: 0,
            };
            
            Ok(Response::new(response))
        }
        
        async fn list_services(
            &self,
            request: Request<ListServicesRequest>,
        ) -> Result<Response<ListServicesResponse>, Status> {
            // In a real implementation, list services from state
            
            let response = ListServicesResponse {
                services: vec![],
            };
            
            Ok(Response::new(response))
        }
        
        async fn list_workloads(
            &self,
            request: Request<ListWorkloadsRequest>,
        ) -> Result<Response<ListWorkloadsResponse>, Status> {
            // In a real implementation, list workloads from state
            
            let response = ListWorkloadsResponse {
                workloads: vec![],
            };
            
            Ok(Response::new(response))
        }
        
        async fn list_nodes(
            &self,
            request: Request<ListNodesRequest>,
        ) -> Result<Response<ListNodesResponse>, Status> {
            // In a real implementation, list nodes from state
            
            let response = ListNodesResponse {
                nodes: vec![],
            };
            
            Ok(Response::new(response))
        }
        
        async fn drain_node(
            &self,
            request: Request<DrainNodeRequest>,
        ) -> Result<Response<DrainNodeResponse>, Status> {
            let node_id = request.into_inner().node_id;
            
            // In a real implementation, set node draining in state
            
            let response = DrainNodeResponse {
                accepted: true,
                error: None,
            };
            
            Ok(Response::new(response))
        }
        
        async fn undrain_node(
            &self,
            request: Request<UndrainNodeRequest>,
        ) -> Result<Response<UndrainNodeResponse>, Status> {
            let node_id = request.into_inner().node_id;
            
            // In a real implementation, clear node draining in state
            
            let response = UndrainNodeResponse {
                accepted: true,
                error: None,
            };
            
            Ok(Response::new(response))
        }
        
        async fn scale_service(
            &self,
            request: Request<ScaleServiceRequest>,
        ) -> Result<Response<ScaleServiceResponse>, Status> {
            let request = request.into_inner();
            
            // In a real implementation, update service target replicas
            
            let response = ScaleServiceResponse {
                service_id: request.service_id,
                old_replicas: 1,
                new_replicas: request.replicas,
                error: None,
            };
            
            Ok(Response::new(response))
        }
        
        async fn list_events(
            &self,
            request: Request<ListEventsRequest>,
        ) -> Result<Response<ListEventsResponse>, Status> {
            // In a real implementation, list events from database
            
            let response = ListEventsResponse {
                events: vec![],
            };
            
            Ok(Response::new(response))
        }
    }
}

// ============================================================================
// Main Server Entry Point
// ============================================================================

pub async fn run_server(config: ControllerConfig) -> Result<(), ValkvorkError> {
    // Initialize state
    let state = ClusterState::new();
    
    // Create server
    let server = ControllerServer::new(config, state);
    
    // Start server
    server.start().await
}
