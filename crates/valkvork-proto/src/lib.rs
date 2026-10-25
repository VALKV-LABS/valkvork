//! gRPC protocol definitions for Valkvork.
//!
//! This crate contains the generated protobuf types and gRPC service definitions
//! for both the AgentService (agent <-> controller) and AdminService (CLI/UI <-> controller).

pub mod agent {
    tonic::include_proto!("valkvork.v1.agent");
}

pub mod admin {
    tonic::include_proto!("valkvork.v1.admin");
}
