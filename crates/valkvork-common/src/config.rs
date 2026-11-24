//! Configuration files for the controller and agent.
//!
//! Every field has a default so that a minimal file works; unknown keys are
//! rejected so typos fail loudly at startup.

use crate::errors::ValkvorkError;
use crate::units::parse_size;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

// ============================================================================
// Controller
// ============================================================================

/// `/etc/valkvork/controller.toml`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ControllerConfig {
    /// Agent stream listener (FR-SEC-05: reachable from worker subnets only).
    pub agent_listen_addr: SocketAddr,
    /// Admin API / UI listener.
    pub admin_listen_addr: SocketAddr,

    /// Server certificate and key (PEM). When both are unset the controller
    /// serves plaintext and logs a warning; only acceptable for development.
    pub tls_cert_file: Option<PathBuf>,
    pub tls_key_file: Option<PathBuf>,

    pub database_path: PathBuf,

    /// Join token for agents (FR-SEC-01). `join_token_file` wins when set.
    pub join_token: Option<String>,
    pub join_token_file: Option<PathBuf>,
    /// Admin bearer token (FR-SEC-03). `admin_token_file` wins when set.
    pub admin_token: Option<String>,
    pub admin_token_file: Option<PathBuf>,

    /// FR-CON-02
    pub node_unhealthy_timeout_seconds: u64,
    /// FR-CON-04
    pub recovery_grace_period_seconds: u64,
    pub heartbeat_interval_ms: u32,
    /// FR-CON-09: resend a command once if not acked within this time.
    pub command_timeout_seconds: u64,
    /// FR-CON-09: ASSIGNED workloads must be RUNNING within this time.
    pub start_timeout_seconds: u64,
    /// FR-CON-05 reconcile tick.
    pub reconcile_interval_seconds: u64,
    /// FR-CON-01: in-memory heartbeat times are flushed this often.
    pub heartbeat_flush_interval_seconds: u64,

    pub autoscaler: AutoscalerConfig,

    /// Built-in web UI on the admin listener (FR-UI-02).
    pub ui: UiConfig,

    /// Prometheus `/metrics` listener (NFR-07). Unset disables it.
    pub metrics_listen_addr: Option<SocketAddr>,

    /// Per-node client certificates on the agent listener (FR-SEC-02).
    pub node_certificates: NodeCertificatesConfig,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            agent_listen_addr: "0.0.0.0:50051".parse().unwrap(),
            admin_listen_addr: "0.0.0.0:50052".parse().unwrap(),
            tls_cert_file: None,
            tls_key_file: None,
            database_path: PathBuf::from("/var/lib/valkvork/db/valkvork.db"),
            join_token: None,
            join_token_file: None,
            admin_token: None,
            admin_token_file: None,
            node_unhealthy_timeout_seconds: 10,
            recovery_grace_period_seconds: 30,
            heartbeat_interval_ms: 2000,
            command_timeout_seconds: 30,
            start_timeout_seconds: 300,
            reconcile_interval_seconds: 5,
            heartbeat_flush_interval_seconds: 30,
            autoscaler: AutoscalerConfig::default(),
            ui: UiConfig::default(),
            metrics_listen_addr: Some("0.0.0.0:9400".parse().unwrap()),
            node_certificates: NodeCertificatesConfig::default(),
        }
    }
}

impl ControllerConfig {
    pub fn load(path: &Path) -> Result<Self, ValkvorkError> {
        load_toml(path)
    }

    pub fn resolve_join_token(&self) -> Result<String, ValkvorkError> {
        resolve_secret(
            "join_token",
            self.join_token.as_deref(),
            self.join_token_file.as_deref(),
        )
    }

    pub fn resolve_admin_token(&self) -> Result<String, ValkvorkError> {
        resolve_secret(
            "admin_token",
            self.admin_token.as_deref(),
            self.admin_token_file.as_deref(),
        )
    }
}

/// How agents authenticate on the agent listener (FR-SEC-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeCertificateMode {
    /// Join token only; no certificates are issued.
    Off,
    /// Certificates are issued and accepted; the join token still works for
    /// every node. The migration mode, and the default.
    Issue,
    /// A node that holds a certificate must present it. The join token only
    /// enrols nodes the controller has issued nothing to.
    Require,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeCertificatesConfig {
    pub mode: NodeCertificateMode,
    /// Lifetime of a node certificate; agents renew at two thirds of it.
    pub lifetime_days: u32,
}

impl Default for NodeCertificatesConfig {
    fn default() -> Self {
        Self {
            mode: NodeCertificateMode::Issue,
            lifetime_days: 30,
        }
    }
}

/// Web UI settings (FR-UI-02).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub enabled: bool,
    /// Session lifetime (FR-UI-06: 12 h).
    pub session_ttl_seconds: u64,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session_ttl_seconds: 12 * 3600,
        }
    }
}

/// Autoscaler tuning (FR-AUT). Defaults are the spec values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoscalerConfig {
    pub enabled: bool,
    pub evaluation_interval_seconds: u64,
    pub window_seconds: u64,
    pub sustain_seconds: u64,
    pub cooldown_seconds: u64,
}

impl Default for AutoscalerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            evaluation_interval_seconds: 10,
            window_seconds: 60,
            sustain_seconds: 120,
            cooldown_seconds: 180,
        }
    }
}

// ============================================================================
// Agent
// ============================================================================

/// `/etc/valkvork/agent.toml` (spec §10.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// `https://host:50051` (TLS, verified against `ca_file`) or `http://`
    /// for development.
    pub controller_endpoint: String,
    pub ca_file: Option<PathBuf>,
    /// TLS server name to verify; defaults to the endpoint host.
    pub tls_domain: Option<String>,
    pub join_token_file: PathBuf,
    pub node_class: String,
    pub data_dir: PathBuf,
    pub runtime: String,
    pub docker_socket: PathBuf,
    /// Docker config-format credentials (FR-AGE-04).
    pub registry_auth_file: PathBuf,
    /// Overrides the auto-detected address reported in `Register`.
    pub advertise_ip: Option<String>,
    /// Prometheus `/metrics` (NFR-07). Must be a loopback address: the
    /// agent exposes no other listener (FR-ARC-01). Unset disables it.
    pub metrics_listen_addr: Option<SocketAddr>,
    pub reserved: ReservedResources,
    pub host_volumes: BTreeMap<String, HostVolume>,
    /// How workloads' [secrets] references are resolved.
    pub secrets: SecretsConfig,
    /// Service discovery: DNS for `<service>.<domain>` on the node.
    pub discovery: DiscoveryConfig,
    /// Where a port is published when its manifest sets no `host_ip`:
    /// `"advertise"` (the node's address, the default), `"0.0.0.0"` (every
    /// address, as before this setting), or an IP.
    pub default_host_ip: String,
    /// Containers' access to the cloud instance metadata service.
    pub instance_metadata: InstanceMetadataConfig,
    /// The most processes and threads a container may have when its
    /// manifest sets no `[security] pids_limit`; 0 for no limit.
    pub default_pids_limit: u32,
}

/// agent.toml `[instance_metadata]`. With `block`, the agent drops traffic
/// from the Docker bridge to 169.254.169.254, except from containers whose
/// manifest sets `[security] instance_metadata = true`, so a workload cannot
/// use the node's role unless it asks to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceMetadataConfig {
    pub block: bool,
    /// The iptables command; it must drive the same backend as Docker's
    /// (`iptables` on Ubuntu, which is iptables-nft).
    pub iptables: String,
    /// The bridge interface whose traffic is filtered.
    pub bridge: String,
}

impl Default for InstanceMetadataConfig {
    fn default() -> Self {
        Self {
            block: true,
            iptables: "iptables".to_string(),
            bridge: "docker0".to_string(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            controller_endpoint: "https://controlplane.valkvork.internal:50051".to_string(),
            ca_file: Some(PathBuf::from("/etc/valkvork/ca.pem")),
            tls_domain: None,
            join_token_file: PathBuf::from("/etc/valkvork/join-token"),
            node_class: "general".to_string(),
            data_dir: PathBuf::from("/var/lib/valkvork"),
            runtime: "docker".to_string(),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            registry_auth_file: PathBuf::from("/etc/valkvork/registry-auth.json"),
            advertise_ip: None,
            metrics_listen_addr: Some("127.0.0.1:9401".parse().unwrap()),
            reserved: ReservedResources::default(),
            host_volumes: BTreeMap::new(),
            secrets: SecretsConfig::default(),
            discovery: DiscoveryConfig::default(),
            default_host_ip: "advertise".to_string(),
            instance_metadata: InstanceMetadataConfig::default(),
            default_pids_limit: 4096,
        }
    }
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self, ValkvorkError> {
        let cfg: Self = load_toml(path)?;
        if cfg.runtime != "docker" {
            return Err(ValkvorkError::Config(format!(
                "runtime {:?} is not supported (only \"docker\" in this release)",
                cfg.runtime
            )));
        }
        parse_size(&cfg.reserved.memory)
            .map_err(|e| ValkvorkError::Config(format!("reserved.memory: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ValkvorkError> {
        for (name, hv) in &self.host_volumes {
            hv.owner_ids()
                .map_err(|e| ValkvorkError::Config(format!("host_volumes.{name}.owner: {e}")))?;
            hv.mode_bits()
                .map_err(|e| ValkvorkError::Config(format!("host_volumes.{name}.mode: {e}")))?;
        }
        for dir in &self.secrets.file_dirs {
            if !dir.is_absolute() {
                return Err(ValkvorkError::Config(format!(
                    "secrets.file_dirs: {} must be an absolute path",
                    dir.display()
                )));
            }
        }
        if self.discovery.enabled {
            let d = &self.discovery;
            let label_ok = |l: &str| {
                !l.is_empty()
                    && l.len() <= 63
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            };
            if d.domain.is_empty() || d.domain.len() > 200 || !d.domain.split('.').all(label_ok) {
                return Err(ValkvorkError::Config(format!(
                    "discovery.domain {:?} is not a DNS name",
                    d.domain
                )));
            }
            if let Some(ip) = d.listen_ip {
                if ip.is_unspecified() || ip.is_multicast() {
                    return Err(ValkvorkError::Config(format!(
                        "discovery.listen_ip {ip} must be one address, such as the Docker bridge's gateway"
                    )));
                }
            }
        }
        if self.default_host_ip != "advertise"
            && self.default_host_ip.parse::<std::net::IpAddr>().is_err()
        {
            return Err(ValkvorkError::Config(format!(
                "default_host_ip {:?} must be \"advertise\" or an IP address such as \"0.0.0.0\"",
                self.default_host_ip
            )));
        }
        let im = &self.instance_metadata;
        if im.block
            && (im.iptables.trim().is_empty()
                || im.bridge.is_empty()
                || !im
                    .bridge
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
        {
            return Err(ValkvorkError::Config(
                "instance_metadata: iptables and bridge must be set".into(),
            ));
        }
        if let Some(addr) = self.metrics_listen_addr {
            if !addr.ip().is_loopback() {
                return Err(ValkvorkError::Config(format!(
                    "metrics_listen_addr {addr} must be a loopback address (FR-ARC-01)"
                )));
            }
        }
        Ok(())
    }
}

/// Service discovery (agent.toml `[discovery]`). The controller sends every
/// agent each service's reachable node addresses; with `enabled`, the agent
/// answers DNS for `<service>.<domain>` on `listen_ip`, port 53, forwards
/// every other query to `upstream`, and gives its containers that address as
/// their only nameserver.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoveryConfig {
    pub enabled: bool,
    pub domain: String,
    /// Unset: the Docker bridge network's gateway (usually 172.17.0.1), which
    /// only this node and its containers can reach (FR-ARC-01).
    pub listen_ip: Option<std::net::IpAddr>,
    /// Resolvers for every other name. Empty: the nameservers in
    /// /etc/resolv.conf.
    pub upstream: Vec<SocketAddr>,
    /// TTL of the answers, in seconds.
    pub ttl_seconds: u32,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            domain: "svc.valkvork.internal".to_string(),
            listen_ip: None,
            upstream: Vec::new(),
            ttl_seconds: 5,
        }
    }
}

/// Resolution of workloads' secret references (agent.toml `[secrets]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsConfig {
    /// The AWS CLI, which resolves `ssm:` references with the instance's role.
    pub aws_cli: PathBuf,
    /// Region for `ssm:` references. Unset: the instance's region from the
    /// EC2 metadata service, or off EC2 the CLI's own default.
    pub aws_region: Option<String>,
    /// Directories `file:` references may read from. Empty (the default)
    /// disables `file:` references: the agent runs as root, so a manifest
    /// could otherwise read any file on the node.
    pub file_dirs: Vec<PathBuf>,
    /// Limit for resolving one reference.
    pub timeout_seconds: u64,
    /// Keep file secrets (`file = true`) on an in-memory filesystem the agent
    /// mounts at `<data_dir>/secrets`. false keeps them in a plain directory
    /// on disk: only for development, where the agent cannot mount one.
    pub files_tmpfs: bool,
    /// How often running workloads' file secrets are resolved again and
    /// rewritten when the value changed; 0 never.
    pub refresh_seconds: u64,
    /// Reuse the last value this agent resolved for a reference when
    /// resolving it fails at container start (kept in memory only).
    pub cache: bool,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            aws_cli: PathBuf::from("aws"),
            aws_region: None,
            file_dirs: Vec::new(),
            timeout_seconds: 15,
            files_tmpfs: true,
            refresh_seconds: 300,
            cache: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReservedResources {
    /// Cores kept back for the OS, dockerd and the agent.
    pub cpu: f64,
    pub memory: String,
}

impl Default for ReservedResources {
    fn default() -> Self {
        Self {
            cpu: 0.25,
            memory: "384MiB".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostVolume {
    pub path: PathBuf,
    #[serde(default)]
    pub read_only: bool,
    /// Numeric `"uid"` or `"uid:gid"` the agent gives the directory at
    /// startup, so images that run as a non-root user can write to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Octal permission bits the agent sets at startup, e.g. `"0750"`, or
    /// `"1777"` for a scratch directory any user may write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

impl HostVolume {
    /// `owner` as (uid, optional gid).
    pub fn owner_ids(&self) -> Result<Option<(u32, Option<u32>)>, String> {
        let Some(o) = self.owner.as_deref() else {
            return Ok(None);
        };
        let num = |s: &str| {
            s.parse::<u32>()
                .map_err(|_| format!("{o:?} must be a numeric uid or uid:gid"))
        };
        match o.split_once(':') {
            None => Ok(Some((num(o)?, None))),
            Some((u, g)) => Ok(Some((num(u)?, Some(num(g)?)))),
        }
    }

    /// `mode` as permission bits.
    pub fn mode_bits(&self) -> Result<Option<u32>, String> {
        let Some(m) = self.mode.as_deref() else {
            return Ok(None);
        };
        match u32::from_str_radix(m, 8) {
            Ok(bits) if bits <= 0o7777 && !m.is_empty() => Ok(Some(bits)),
            _ => Err(format!(
                "{m:?} must be octal permission bits, e.g. \"0750\""
            )),
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn load_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ValkvorkError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ValkvorkError::Config(format!("cannot read {}: {e}", path.display())))?;
    toml::from_str(&content).map_err(|e| ValkvorkError::Config(format!("{}: {e}", path.display())))
}

fn resolve_secret(
    name: &str,
    inline: Option<&str>,
    file: Option<&Path>,
) -> Result<String, ValkvorkError> {
    let value = match (file, inline) {
        (Some(path), _) => std::fs::read_to_string(path)
            .map_err(|e| {
                ValkvorkError::Config(format!("cannot read {name} file {}: {e}", path.display()))
            })?
            .trim()
            .to_string(),
        (None, Some(v)) => v.trim().to_string(),
        (None, None) => String::new(),
    };
    if value.is_empty() {
        return Err(ValkvorkError::Config(format!(
            "{name} (or {name}_file) must be set"
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_spec_example_parses() {
        let cfg: AgentConfig = toml::from_str(
            r#"
controller_endpoint = "https://controlplane.valkvork.internal:50051"
ca_file             = "/etc/valkvork/ca.pem"
join_token_file     = "/etc/valkvork/join-token"
node_class          = "general"
data_dir            = "/var/lib/valkvork"
runtime             = "docker"
docker_socket       = "/var/run/docker.sock"

[reserved]
cpu    = 0.25
memory = "384MiB"

[host_volumes.pgdata]
path      = "/data/pgdata"
read_only = false
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.host_volumes["pgdata"].path,
            PathBuf::from("/data/pgdata")
        );
        assert_eq!(cfg.host_volumes["pgdata"].owner_ids(), Ok(None));
    }

    #[test]
    fn host_volume_owner_and_mode() {
        let hv = |owner: Option<&str>, mode: Option<&str>| HostVolume {
            path: "/d".into(),
            read_only: false,
            owner: owner.map(Into::into),
            mode: mode.map(Into::into),
        };
        assert_eq!(hv(Some("101"), None).owner_ids(), Ok(Some((101, None))));
        assert_eq!(
            hv(Some("101:102"), None).owner_ids(),
            Ok(Some((101, Some(102))))
        );
        assert!(hv(Some("nginx"), None).owner_ids().is_err());
        assert!(hv(Some("101:"), None).owner_ids().is_err());
        assert_eq!(hv(None, Some("0750")).mode_bits(), Ok(Some(0o750)));
        assert_eq!(hv(None, Some("1777")).mode_bits(), Ok(Some(0o1777)));
        assert!(hv(None, Some("0789")).mode_bits().is_err());
        assert!(hv(None, Some("17777")).mode_bits().is_err());

        let mut cfg = AgentConfig::default();
        cfg.host_volumes
            .insert("logs".into(), hv(Some("app"), None));
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("host_volumes.logs.owner"), "{err}");
    }

    #[test]
    fn controller_minimal_parses() {
        let cfg: ControllerConfig =
            toml::from_str("join_token = \"j\"\nadmin_token = \"a\"\n").unwrap();
        assert_eq!(cfg.node_unhealthy_timeout_seconds, 10);
        assert_eq!(cfg.resolve_join_token().unwrap(), "j");
    }

    #[test]
    fn agent_metrics_must_be_loopback() {
        let mut cfg = AgentConfig::default();
        assert!(cfg.validate().is_ok());
        cfg.metrics_listen_addr = Some("0.0.0.0:9401".parse().unwrap());
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn discovery_settings() {
        let cfg: AgentConfig = toml::from_str("[discovery]\nenabled = true\n").unwrap();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.discovery.domain, "svc.valkvork.internal");
        assert_eq!(
            (cfg.discovery.listen_ip, cfg.discovery.ttl_seconds),
            (None, 5)
        );
        for bad in [
            "domain = \"\"",
            "domain = \"svc..internal\"",
            "domain = \"has space.internal\"",
            "listen_ip = \"0.0.0.0\"",
            "listen_ip = \"::\"",
        ] {
            let cfg: AgentConfig =
                toml::from_str(&format!("[discovery]\nenabled = true\n{bad}\n")).unwrap();
            assert!(cfg.validate().is_err(), "{bad}");
        }
        // Off: not checked, nothing listens.
        let off: AgentConfig = toml::from_str("[discovery]\ndomain = \"\"\n").unwrap();
        assert!(off.validate().is_ok());
        assert!(toml::from_str::<AgentConfig>("[discovery]\nport = 53\n").is_err());
    }

    #[test]
    fn controller_ui_and_metrics_keys() {
        let cfg: ControllerConfig =
            toml::from_str("metrics_listen_addr = \"127.0.0.1:9999\"\n[ui]\nenabled = false\n")
                .unwrap();
        assert!(!cfg.ui.enabled);
        assert_eq!(cfg.metrics_listen_addr.unwrap().port(), 9999);
    }

    #[test]
    fn unknown_key_rejected() {
        assert!(toml::from_str::<ControllerConfig>("bogus = 1").is_err());
    }
}
