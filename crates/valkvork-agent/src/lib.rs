//! Valkvork worker agent (spec §4).
//!
//! * [`runtime`]: the `ContainerRuntime` trait (FR-AGE-09) and its Docker driver.
//! * [`supervisor`]: local workload lifecycle — start/stop, restart policy
//!   (FR-AGE-06), adoption (FR-AGE-07), disk enforcement (FR-AGE-08) and
//!   per-container telemetry (FR-AGE-03).
//! * [`connection`]: the outbound stream to the controller with backoff
//!   (FR-ARC-01, FR-AGE-05).
//! * [`certs`]: the node's client certificate (FR-SEC-02).
//! * [`identity`]: node id, hostname, address and capacity (FR-AGE-02).
//! * [`secrets`]: resolution of workloads' secret references (FR-SEC-04).
//! * [`secret_files`]: secrets delivered as files on an in-memory filesystem.
//! * [`health`]: health-check probes.
//! * [`metadata`]: blocks containers from the cloud instance metadata service.
//! * [`discovery`]: DNS for services on the node, from the controller's endpoints.

pub mod certs;
pub mod connection;
pub mod discovery;
pub mod health;
pub mod identity;
pub mod metadata;
pub mod metrics;
pub mod runtime;
pub mod secret_files;
pub mod secrets;
pub mod supervisor;
pub mod telemetry;
