//! gRPC client for the Valkvork controller.

use std::sync::Arc;
use tonic::transport::{Channel, ClientTlsConfig};
use valkvork_proto::admin::AdminServiceClient;
use valkvork_common::errors::ValkvorkError;
use valkvork_common::tls::load_client_config;
use std::path::Path;

/// Client for connecting to the Valkvork controller.
pub struct Client {
    admin_client: AdminServiceClient<Channel>,
    token: Option<String>,
}

impl Client {
    pub fn new(endpoint: &str, token: Option<&str>) -> Result<Self, ValkvorkError> {
        // Build TLS configuration
        let tls_config = load_client_config(None)?;
        
        // Create channel with TLS
        let channel = tonic::transport::Endpoint::from_shared(endpoint.to_string())?
            .tls_config(ClientTlsConfig::new().ca_certificate(tls_config.root_certificates()))?
            .connect()
            .await?;
        
        let admin_client = AdminServiceClient::new(channel);
        
        Ok(Self {
            admin_client,
            token: token.map(|s| s.to_string()),
        })
    }
    
    pub fn admin_client(&self) -> &AdminServiceClient<Channel> {
        &self.admin_client
    }
    
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

/// Helper to add auth token to request metadata
pub fn with_auth_token(request: tonic::Request<impl prost::Message>, token: Option<&str>) -> tonic::Request<impl prost::Message> {
    let mut req = request;
    
    if let Some(token) = token {
        let metadata = req.metadata_mut();
        metadata.insert("authorization", format!("Bearer {}", token).parse().unwrap());
    }
    
    req
}
