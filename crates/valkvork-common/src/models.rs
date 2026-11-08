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
    /// A batch workload that exited 0 (terminal, never restarted).
    Completed,
}
string_enum!(WorkloadStatus {
    Pending => "PENDING",
    Assigned => "ASSIGNED",
    Running => "RUNNING",
    Stopping => "STOPPING",
    Lost => "LOST",
    Stopped => "STOPPED",
    Failed => "FAILED",
    Completed => "COMPLETED",
});

impl WorkloadStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            WorkloadStatus::Stopped | WorkloadStatus::Failed | WorkloadStatus::Completed
        )
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
    /// A directory the node declares (`[host_volumes.<name>]`): the
    /// scheduler places the workload only where it exists.
    pub fn is_host(&self) -> bool {
        self.kind == "host"
    }

    /// Only one live workload may use the directory at a time: writable
    /// host volumes are single-writer unless declared `single_writer = false`.
    /// An ephemeral volume belongs to one workload, so never.
    pub fn single_writer(&self) -> bool {
        self.is_host() && !self.read_only && !self.shared
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
    /// `service` (long-running, the default) or `batch` (runs to
    /// completion). Omitted for services (stable hashes).
    #[serde(
        rename = "type",
        default,
        skip_serializing_if = "ServiceType::is_service"
    )]
    pub service_type: ServiceType,
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

/// Manifest features an agent must support to run a workload that uses them.
/// Agents list theirs in `Register`; the scheduler places a workload only on a
/// node whose agent has every feature its spec uses. Protobuf ignores fields
/// a receiver does not know, so without this an older agent would start the
/// container without its secrets, check or bind address.
pub const AGENT_FEATURES: &[&str] = &[
    "batch",
    "command",
    "ephemeral_volumes",
    "health_checks",
    "host_ip",
    "node_values",
    "secret_values",
    "secrets",
];

/// What `${node.NAME}` in an [env] value can name, substituted by the agent
/// with its own node's value at container start.
pub const NODE_VALUES: &[&str] = &["advertise_ip", "class", "hostname", "id"];

/// What a service's workloads do when their process exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    /// Long-running: every exit is a failure, restarted under the policy.
    #[default]
    Service,
    /// Runs to completion: exit 0 is COMPLETED and is never restarted.
    Batch,
}

impl ServiceType {
    pub fn is_service(&self) -> bool {
        *self == ServiceType::Service
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceType::Service => "service",
            ServiceType::Batch => "batch",
        }
    }
}

/// The names in an [env] value's `${secret:NAME}` references, in order.
pub fn secret_refs(value: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(i) = rest.find("${secret:") {
        let after = &rest[i + "${secret:".len()..];
        match after.find('}') {
            Some(j) => {
                out.push(&after[..j]);
                rest = &after[j + 1..];
            }
            None => break,
        }
    }
    out
}

/// Replace each `${secret:NAME}` in `value` with `lookup(NAME)`. An unclosed
/// reference or an unknown name is an error naming it.
pub fn interpolate_secrets<'a>(
    value: &str,
    lookup: impl Fn(&str) -> Option<&'a str>,
) -> Result<String, String> {
    interpolate(value, "${secret:", "secret", lookup)
}

/// Replace each `${node.NAME}` in `value` with `lookup(NAME)` (see
/// `NODE_VALUES`). An unclosed reference or an unknown name is an error.
pub fn interpolate_node<'a>(
    value: &str,
    lookup: impl Fn(&str) -> Option<&'a str>,
) -> Result<String, String> {
    interpolate(value, "${node.", "node value", lookup)
}

fn interpolate<'a>(
    value: &str,
    open: &str,
    what: &str,
    lookup: impl Fn(&str) -> Option<&'a str>,
) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(i) = rest.find(open) {
        out.push_str(&rest[..i]);
        let after = &rest[i + open.len()..];
        let j = after.find('}').ok_or_else(|| format!("unclosed {open}"))?;
        let name = &after[..j];
        out.push_str(lookup(name).ok_or_else(|| format!("no {what} {name:?}"))?);
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

impl ServiceSpec {
    /// The `AGENT_FEATURES` this spec uses.
    pub fn required_features(&self) -> Vec<&'static str> {
        let mut f = Vec::new();
        if self.command.is_some() || self.entrypoint.is_some() {
            f.push("command");
        }
        if self.check.is_some() {
            f.push("health_checks");
        }
        if self.ports.iter().any(|p| p.host_ip.is_some()) {
            f.push("host_ip");
        }
        if !self.secrets.is_empty() {
            f.push("secrets");
        }
        if self.env.values().any(|v| v.contains("${secret:")) {
            f.push("secret_values");
        }
        if self.env.values().any(|v| v.contains("${node.")) {
            f.push("node_values");
        }
        if self.service_type == ServiceType::Batch {
            f.push("batch");
        }
        if self.volumes.iter().any(|v| v.kind == "ephemeral") {
            f.push("ephemeral_volumes");
        }
        f
    }

    pub fn is_batch(&self) -> bool {
        self.service_type == ServiceType::Batch
    }
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
    /// A batch service whose run has finished, applied again unchanged: a
    /// new version, so it runs once more.
    Rerun,
}
string_enum!(ChangeKind {
    Created => "created",
    Unchanged => "unchanged",
    ScaleOnly => "scale-only",
    PolicyOnly => "policy-only",
    Replacing => "replacing",
    Rerun => "rerun",
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
            && self.service_type == other.service_type
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
            service_type: ServiceType::Service,
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

    #[test]
    fn secret_references_in_values() {
        let v = "postgresql://valkv:${secret:PW}@db:5432/x?k=${secret:K}";
        assert_eq!(secret_refs(v), vec!["PW", "K"]);
        assert_eq!(secret_refs("no refs, ${other}"), Vec::<&str>::new());
        let look = |n: &str| match n {
            "PW" => Some("p4ss"),
            "K" => Some("key"),
            _ => None,
        };
        assert_eq!(
            interpolate_secrets(v, look).unwrap(),
            "postgresql://valkv:p4ss@db:5432/x?k=key"
        );
        assert_eq!(interpolate_secrets("plain", look).unwrap(), "plain");
        assert!(interpolate_secrets("${secret:NOPE}", look)
            .unwrap_err()
            .contains("NOPE"));
        assert!(interpolate_secrets("${secret:PW", look).is_err());
    }

    /// A service spec's hash is unchanged by the new type field; a batch
    /// spec's is not.
    #[test]
    fn service_type_keeps_old_hashes() {
        let a = spec();
        assert!(!a.canonical_json().contains("\"type\""));
        let mut b = spec();
        b.service_type = ServiceType::Batch;
        assert!(b.canonical_json().contains("\"type\":\"batch\""));
        assert!(!a.same_replacement_fields(&b));
        assert_eq!(b.required_features(), vec!["batch"]);
    }

    #[test]
    fn node_values_in_env() {
        let look = |n: &str| match n {
            "advertise_ip" => Some("10.0.0.5"),
            "id" => Some("n-1"),
            _ => None,
        };
        assert_eq!(
            interpolate_node("http://${node.advertise_ip}:4000/x?n=${node.id}", look).unwrap(),
            "http://10.0.0.5:4000/x?n=n-1"
        );
        assert!(interpolate_node("${node.nope}", look)
            .unwrap_err()
            .contains("nope"));
        assert!(interpolate_node("${node.id", look).is_err());
        // Secrets and node values don't see each other's syntax.
        assert_eq!(
            interpolate_node("${secret:X}", look).unwrap(),
            "${secret:X}"
        );
        let mut s = spec();
        s.env.insert("A".into(), "${node.id}".into());
        assert_eq!(s.required_features(), vec!["node_values"]);
    }
}
