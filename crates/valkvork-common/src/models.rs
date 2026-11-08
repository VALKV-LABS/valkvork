//! Core data models shared by the controller, agent and CLI.
//!
//! Units follow spec §2.6: CPU in integer millicores, sizes in bytes,
//! durations in seconds, timestamps in Unix epoch milliseconds.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// Current wall-clock time in Unix epoch milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Generates `as_str`, `Display` and `FromStr` for a fieldless enum whose
/// variants map to fixed strings (the values stored in SQLite and on the wire).
macro_rules! string_enum {
    ($name:ident { $($variant:ident => $s:expr),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok($name::$variant),)+
                    other => Err(format!("invalid {}: {}", stringify!($name), other)),
                }
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeStatus {
    Healthy,
    Unhealthy,
}
string_enum!(NodeStatus { Healthy => "HEALTHY", Unhealthy => "UNHEALTHY" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceStatus {
    Active,
    Degraded,
    Deleting,
}
string_enum!(ServiceStatus { Active => "ACTIVE", Degraded => "DEGRADED", Deleting => "DELETING" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RolloutStatus {
    None,
    InProgress,
    Halted,
}
string_enum!(RolloutStatus { None => "NONE", InProgress => "IN_PROGRESS", Halted => "HALTED" });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesiredState {
    Run,
    Stop,
}
string_enum!(DesiredState { Run => "RUN", Stop => "STOP" });

/// Workload status (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkloadStatus {
    Pending,
    Assigned,
    Running,
    Stopping,
    Lost,
    Stopped,
    Failed,
}
string_enum!(WorkloadStatus {
    Pending => "PENDING",
    Assigned => "ASSIGNED",
    Running => "RUNNING",
    Stopping => "STOPPING",
    Lost => "LOST",
    Stopped => "STOPPED",
    Failed => "FAILED",
});

impl WorkloadStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, WorkloadStatus::Stopped | WorkloadStatus::Failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Node,
    Workload,
    Deploy,
    Scale,
    Audit,
}
string_enum!(EventKind {
    Node => "NODE",
    Workload => "WORKLOAD",
    Deploy => "DEPLOY",
    Scale => "SCALE",
    Audit => "AUDIT",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubjectType {
    Node,
    Service,
    Workload,
}
string_enum!(SubjectType { Node => "node", Service => "service", Workload => "workload" });

// ============================================================================
// Service specification (canonical, validated manifest)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestartPolicy {
    pub attempts: u32,
    pub interval_seconds: u32,
    pub delay_seconds: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            interval_seconds: 300,
            delay_seconds: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdatePolicy {
    pub max_parallel: u32,
    pub min_healthy_time_seconds: u32,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            max_parallel: 1,
            min_healthy_time_seconds: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoscaleSpec {
    pub min_replicas: u32,
    pub max_replicas: u32,
    pub target_cpu_percent: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortSpec {
    pub name: String,
    pub container_port: u16,
    pub host_port: u16,
    /// Host address to publish on: an IP, or `"advertise"` for the node's
    /// advertised address. Unset: every address (0.0.0.0). Omitted when unset,
    /// so specs stored before it existed keep their hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub source: String,
    pub target: String,
    pub read_only: bool,
    /// Manifest `single_writer = false`: several workloads may use the
    /// directory at once. Stored inverted and omitted when false, so specs
    /// stored before the setting existed keep their canonical JSON and hash.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,
}

impl VolumeSpec {
    /// Only one live workload may use the directory at a time: writable
    /// host volumes are single-writer unless declared `single_writer = false`.
    pub fn single_writer(&self) -> bool {
        !self.read_only && !self.shared
    }
}

/// A health check (`[check]`): the agent probes the workload through one of
/// its published ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSpec {
    /// `tcp` (connects) or `http` (GET, healthy on 2xx and 3xx).
    #[serde(rename = "type")]
    pub kind: String,
    /// A `[[ports]]` name.
    pub port: String,
    /// HTTP path; only for `http`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub interval_seconds: u32,
    pub timeout_seconds: u32,
    /// Consecutive failures that make the workload unhealthy and restart it.
    pub failures: u32,
    /// After a start, failures before the first success are not counted
    /// for this long.
    pub grace_seconds: u32,
}

/// Canonical form of a validated manifest. Field order and `BTreeMap` make the
/// JSON serialisation deterministic, which `spec_hash` relies on (FR-MAN-02).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceSpec {
    pub id: String,
    pub image: String,
    /// Replaces the image's entrypoint. Omitted when unset (stable hashes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// Replaces the image's default command (Docker CMD), keeping its
    /// entrypoint. Omitted when unset (stable hashes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    pub replicas: u32,
    pub kill_timeout_seconds: u32,
    pub cpu_millicores: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub required_class: Option<String>,
    pub env: BTreeMap<String, String>,
    /// Environment variables whose values the agent resolves at container
    /// start: name -> reference (`ssm:<parameter>` or `file:<path>`). Only
    /// the references are stored. Omitted when empty (stable hashes).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
    pub ports: Vec<PortSpec>,
    pub volumes: Vec<VolumeSpec>,
    pub autoscale: Option<AutoscaleSpec>,
    pub update: UpdatePolicy,
    pub restart: RestartPolicy,
    /// Omitted when unset (stable hashes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<CheckSpec>,
}

/// Classification of the difference between a stored spec and a new one
/// (FR-MAN-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Unchanged,
    ScaleOnly,
    PolicyOnly,
    Replacing,
}
string_enum!(ChangeKind {
    Created => "created",
    Unchanged => "unchanged",
    ScaleOnly => "scale-only",
    PolicyOnly => "policy-only",
    Replacing => "replacing",
});

impl ServiceSpec {
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).expect("ServiceSpec serialises")
    }

    /// SHA-256 of the canonical JSON form, hex encoded.
    pub fn spec_hash(&self) -> String {
        hex::encode(Sha256::digest(self.canonical_json().as_bytes()))
    }

    /// True when every replacement-requiring field (spec §3.3) is equal.
    pub fn same_replacement_fields(&self, other: &ServiceSpec) -> bool {
        self.image == other.image
            && self.entrypoint == other.entrypoint
            && self.command == other.command
            && self.cpu_millicores == other.cpu_millicores
            && self.memory_bytes == other.memory_bytes
            && self.disk_bytes == other.disk_bytes
            && self.required_class == other.required_class
            && self.env == other.env
            && self.secrets == other.secrets
            && self.ports == other.ports
            && self.volumes == other.volumes
            && self.restart == other.restart
            && self.kill_timeout_seconds == other.kill_timeout_seconds
            && self.check == other.check
    }

    /// Classify `new` against `self` (the stored spec), ignoring live target
    /// replicas; the caller handles manual-scale drift.
    pub fn classify_change(&self, new: &ServiceSpec) -> ChangeKind {
        if !self.same_replacement_fields(new) {
            ChangeKind::Replacing
        } else if self.autoscale != new.autoscale || self.update != new.update {
            ChangeKind::PolicyOnly
        } else if self.replicas != new.replicas {
            ChangeKind::ScaleOnly
        } else {
            ChangeKind::Unchanged
        }
    }

    /// Updates and drains stop the old workload before starting its
    /// replacement when any volume is single-writer.
    pub fn stop_first(&self) -> bool {
        self.volumes.iter().any(VolumeSpec::single_writer)
    }

    /// Human-readable field-level diff, used by `plan` output.
    pub fn diff_lines(&self, new: &ServiceSpec) -> Vec<String> {
        let mut out = Vec::new();
        macro_rules! cmp {
            ($label:expr, $a:expr, $b:expr) => {
                if $a != $b {
                    out.push(format!("{}: {:?} → {:?}", $label, $a, $b));
                }
            };
        }
        cmp!("image", self.image, new.image);
        cmp!("replicas", self.replicas, new.replicas);
        cmp!(
            "kill_timeout_seconds",
            self.kill_timeout_seconds,
            new.kill_timeout_seconds
        );
        cmp!("cpu_millicores", self.cpu_millicores, new.cpu_millicores);
        cmp!("memory_bytes", self.memory_bytes, new.memory_bytes);
        cmp!("disk_bytes", self.disk_bytes, new.disk_bytes);
        cmp!("required_class", self.required_class, new.required_class);
        cmp!("env", self.env, new.env);
        cmp!("ports", self.ports, new.ports);
        cmp!("volumes", self.volumes, new.volumes);
        cmp!("autoscale", self.autoscale, new.autoscale);
        cmp!("update", self.update, new.update);
        cmp!("restart", self.restart, new.restart);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            id: "web".into(),
            image: "nginx:1.27".into(),
            entrypoint: None,
            command: None,
            replicas: 1,
            kill_timeout_seconds: 10,
            cpu_millicores: 500,
            memory_bytes: 512 << 20,
            disk_bytes: 1 << 30,
            required_class: None,
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            ports: vec![],
            volumes: vec![],
            autoscale: None,
            update: UpdatePolicy::default(),
            restart: RestartPolicy::default(),
            check: None,
        }
    }

    /// Specs stored before `single_writer` existed keep their hash, so
    /// re-applying an unchanged manifest after an upgrade is `unchanged`.
    #[test]
    fn volume_sharing_flag_keeps_old_hashes() {
        let mut a = spec();
        a.volumes.push(VolumeSpec {
            kind: "host".into(),
            source: "pgdata".into(),
            target: "/d".into(),
            read_only: false,
            shared: false,
        });
        let json = a.canonical_json();
        assert!(!json.contains("shared"));
        let old: ServiceSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(old.spec_hash(), a.spec_hash());
        assert!(old.stop_first());
        a.volumes[0].shared = true;
        assert!(a.canonical_json().contains("\"shared\":true"));
        assert!(!a.stop_first());
    }

    #[test]
    fn hash_is_stable_and_sensitive() {
        let a = spec();
        let mut b = spec();
        assert_eq!(a.spec_hash(), b.spec_hash());
        b.replicas = 2;
        assert_ne!(a.spec_hash(), b.spec_hash());
    }

    #[test]
    fn classify() {
        let a = spec();
        let mut b = spec();
        assert_eq!(a.classify_change(&b), ChangeKind::Unchanged);
        b.replicas = 3;
        assert_eq!(a.classify_change(&b), ChangeKind::ScaleOnly);
        b.update.max_parallel = 2;
        assert_eq!(a.classify_change(&b), ChangeKind::PolicyOnly);
        b.image = "nginx:1.28".into();
        assert_eq!(a.classify_change(&b), ChangeKind::Replacing);
    }

    #[test]
    fn status_round_trip() {
        for s in [
            "PENDING", "ASSIGNED", "RUNNING", "STOPPING", "LOST", "STOPPED", "FAILED",
        ] {
            assert_eq!(s.parse::<WorkloadStatus>().unwrap().as_str(), s);
        }
    }
}
