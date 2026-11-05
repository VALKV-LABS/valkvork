//! Valkvork worker agent (spec §4).
//!
//! * [`runtime`]: the `ContainerRuntime` trait (FR-AGE-09) and its Docker driver.
//! * [`supervisor`]: local workload lifecycle — start/stop, restart policy
//!   (FR-AGE-06), adoption (FR-AGE-07), disk enforcement (FR-AGE-08) and
//!   per-container telemetry (FR-AGE-03).
//! * [`connection`]: the outbound stream to the controller with backoff
//!   (FR-ARC-01, FR-AGE-05).
//! * [`identity`]: node id, hostname, address and capacity (FR-AGE-02).

pub mod connection;
pub mod identity;
pub mod metrics;
pub mod runtime;
pub mod supervisor;
pub mod telemetry;
