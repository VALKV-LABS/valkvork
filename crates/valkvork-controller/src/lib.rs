//! Valkvork controller (spec §5).
//!
//! * [`core`]: the single actor that owns cluster state and the SQLite writer
//!   connection (FR-CON-01). Reconciler, scheduler calls, dispatcher, health
//!   checks, rollouts, drains and the autoscaler all run inside it.
//! * [`grpc`]: thin adapters exposing `AgentService` and `AdminService`.

pub mod core;
pub mod db;
pub mod grpc;
pub mod model;
pub mod scheduler;
pub mod server;
