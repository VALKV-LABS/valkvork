//! CLI command implementations.

use std::fs;
use std::path::Path;
use valkvork_proto::admin::*;
use valkvork_common::validation::{parse_manifest_str, spec_to_toml};
use valkvork_common::errors::{ValkvorkError, ValidationError};
use super::client::{Client, with_auth_token};

/// Apply a manifest
pub async fn cmd_apply(client: &Client, file_path: &str) -> Result<(), ValkvorkError> {
    let content = fs::read_to_string(file_path)?;
    
    // Parse and validate the manifest
    let _spec = parse_manifest_str(&content)?;
    
    // Send to controller
    let request = ApplyRequest {
        manifest_toml: content,
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().apply(request).await?;
    
    let response = response.into_inner();
    
    println!("Applied service: {}", response.service_id);
    println!("Version: {}", response.version);
    println!("Change: {}", response.change);
    
    if !response.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in response.warnings {
            println!("  - {}", warning);
        }
    }
    
    Ok(())
}

/// Plan a manifest (dry-run)
pub async fn cmd_plan(client: &Client, file_path: &str) -> Result<(), ValkvorkError> {
    let content = fs::read_to_string(file_path)?;
    
    // Parse and validate the manifest
    let spec = parse_manifest_str(&content)?;
    
    // Send to controller
    let request = ApplyRequest {
        manifest_toml: content,
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().plan(request).await?;
    
    let response = response.into_inner();
    
    println!("Planned service: {}", response.service_id);
    println!("Version: {}", response.version);
    println!("Change: {}", response.change);
    println!("Feasible: {}", response.feasible);
    
    if !response.reason.is_empty() {
        println!("Reason: {}", response.reason);
    }
    
    if !response.warnings.is_empty() {
        println!("\nWarnings:");
        for warning in response.warnings {
            println!("  - {}", warning);
        }
    }
    
    Ok(())
}

/// Delete a service
pub async fn cmd_delete(client: &Client, service_id: &str) -> Result<(), ValkvorkError> {
    let request = DeleteServiceRequest {
        service_id: service_id.to_string(),
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().delete_service(request).await?;
    
    let response = response.into_inner();
    
    if response.accepted {
        println!("Service '{}' marked for deletion", service_id);
    } else if let Some(error) = response.error {
        return Err(ValkvorkError::Internal(error));
    }
    
    Ok(())
}

/// Scale a service
pub async fn cmd_scale(client: &Client, service_id: &str, replicas: u32) -> Result<(), ValkvorkError> {
    let request = ScaleServiceRequest {
        service_id: service_id.to_string(),
        replicas,
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().scale_service(request).await?;
    
    let response = response.into_inner();
    
    if let Some(error) = response.error {
        return Err(ValkvorkError::Internal(error));
    }
    
    println!("Scaled service '{}' from {} to {} replicas", 
             service_id, response.old_replicas, response.new_replicas);
    
    Ok(())
}

/// Get status
pub async fn cmd_status(client: &Client, service_id: Option<&str>) -> Result<(), ValkvorkError> {
    if let Some(service_id) = service_id {
        // Get specific service status
        let request = GetServiceRequest {
            service_id: service_id.to_string(),
        };
        
        let request = with_auth_token(tonic::Request::new(request), client.token());
        let response = client.admin_client().get_service(request).await?;
        
        let service = response.into_inner();
        
        println!("Service: {}", service.service_id);
        println!("Version: {}", service.version);
        println!("Status: {}", service.status);
        println!("Rollout: {}", service.rollout_status);
        println!("Replicas: {}/{} (target/running)", 
                 service.target_replicas, service.running_replicas);
        println!("Pending: {}", service.pending_replicas);
        println!("Failed: {}", service.failed_replicas);
        
        println!("\nWorkloads:");
        for workload in service.workloads {
            println!("  {}: {} ({})", 
                     workload.workload_id, 
                     workload.status, 
                     workload.status_reason.as_deref().unwrap_or(""));
        }
    } else {
        // Get cluster status
        let request = ListServicesRequest {
            service_id: String::new(),
        };
        
        let request = with_auth_token(tonic::Request::new(request), client.token());
        let response = client.admin_client().list_services(request).await?;
        
        let services = response.into_inner();
        
        println!("Cluster Status");
        println!("==============");
        println!("Services: {}", services.services.len());
        
        let mut total_running = 0u32;
        let mut total_pending = 0u32;
        let mut total_failed = 0u32;
        
        for service in services.services {
            total_running += service.running_replicas;
            // Note: pending and failed counts need to be added to the proto
        }
        
        println!("Total workloads: {} running, {} pending, {} failed", 
                 total_running, total_pending, total_failed);
        
        println!("\nServices:");
        for service in services.services {
            println!("  {}: v{} {} {}/{} replicas", 
                     service.service_id, 
                     service.version,
                     service.status,
                     service.running_replicas,
                     service.target_replicas);
        }
    }
    
    Ok(())
}

/// List workloads
pub async fn cmd_workloads(
    client: &Client,
    service_id: Option<&str>,
    node_id: Option<&str>,
    status: Option<&str>,
) -> Result<(), ValkvorkError> {
    let request = ListWorkloadsRequest {
        service_id: service_id.unwrap_or("").to_string(),
        node_id: node_id.unwrap_or("").to_string(),
        status: status.unwrap_or("").to_string(),
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().list_workloads(request).await?;
    
    let workloads = response.into_inner();
    
    println!("Workloads: {}", workloads.workloads.len());
    
    for workload in workloads.workloads {
        println!("  {}: service={} node={} status={} reason={:?}", 
                 workload.workload_id,
                 workload.service_id,
                 workload.node_id,
                 workload.status,
                 workload.status_reason);
    }
    
    Ok(())
}

/// List events
pub async fn cmd_events(
    client: &Client,
    kind: Option<&str>,
    since: Option<u64>,
    limit: u32,
) -> Result<(), ValkvorkError> {
    let request = ListEventsRequest {
        kind: kind.unwrap_or("").to_string(),
        since,
        limit,
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().list_events(request).await?;
    
    let events = response.into_inner();
    
    println!("Events: {}", events.events.len());
    
    for event in events.events {
        println!("  [{:?}] {}: {}", 
                 event.created_at,
                 event.kind,
                 event.message);
    }
    
    Ok(())
}

/// List nodes
pub async fn cmd_node_list(client: &Client) -> Result<(), ValkvorkError> {
    let request = ListNodesRequest {
        node_id: String::new(),
        node_class: String::new(),
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().list_nodes(request).await?;
    
    let nodes = response.into_inner();
    
    println!("Nodes: {}", nodes.nodes.len());
    
    for node in nodes.nodes {
        println!("  {}: {} ({}) status={} draining={} workloads={}", 
                 node.node_id,
                 node.hostname,
                 node.ip_address,
                 node.status,
                 node.draining,
                 node.workload_count);
    }
    
    Ok(())
}

/// Drain a node
pub async fn cmd_node_drain(client: &Client, node_id: &str) -> Result<(), ValkvorkError> {
    let request = DrainNodeRequest {
        node_id: node_id.to_string(),
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().drain_node(request).await?;
    
    let response = response.into_inner();
    
    if response.accepted {
        println!("Draining node '{}'", node_id);
    } else if let Some(error) = response.error {
        return Err(ValkvorkError::Internal(error));
    }
    
    Ok(())
}

/// Undrain a node
pub async fn cmd_node_undrain(client: &Client, node_id: &str) -> Result<(), ValkvorkError> {
    let request = UndrainNodeRequest {
        node_id: node_id.to_string(),
    };
    
    let request = with_auth_token(tonic::Request::new(request), client.token());
    let response = client.admin_client().undrain_node(request).await?;
    
    let response = response.into_inner();
    
    if response.accepted {
        println!("Undraining node '{}'", node_id);
    } else if let Some(error) = response.error {
        return Err(ValkvorkError::Internal(error));
    }
    
    Ok(())
}
