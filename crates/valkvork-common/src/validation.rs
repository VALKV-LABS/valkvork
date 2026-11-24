//! Manifest parsing and validation (spec §6.1, §6.2).
//!
//! Parsing goes through serde structs with `deny_unknown_fields`, so typos are
//! rejected as `UNKNOWN_FIELD` (VE-06). The result is canonicalised into a
//! [`ServiceSpec`] and checked against VE-01 … VE-07.

use crate::errors::{ValidationError, ValidationErrorCode as Code};
use crate::models::{
    interpolate_node, interpolate_secrets, interpolate_workload, AutoscaleSpec, CheckSpec,
    LoggingSpec, PortSpec, RestartPolicy, SecuritySpec, ServiceSpec, ServiceType, UpdatePolicy,
    VolumeSpec,
};
use crate::units::{parse_duration, parse_size};
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

// ============================================================================
// Raw manifest (what the TOML file looks like)
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    service: RawService,
    resources: RawResources,
    #[serde(default)]
    constraints: Option<RawConstraints>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    ports: Vec<RawPort>,
    #[serde(default)]
    volumes: Vec<RawVolume>,
    #[serde(default)]
    autoscale: Option<RawAutoscale>,
    #[serde(default)]
    update: Option<RawUpdate>,
    #[serde(default)]
    restart: Option<RawRestart>,
    #[serde(default)]
    check: Option<RawCheck>,
    #[serde(default)]
    security: Option<RawSecurity>,
    #[serde(default)]
    logging: Option<RawLogging>,
    #[serde(default)]
    sysctls: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawSecurity {
    read_only_root: bool,
    no_new_privileges: bool,
    cap_drop: Vec<String>,
    cap_add: Vec<String>,
    tmpfs: Vec<String>,
    instance_metadata: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogging {
    driver: String,
    #[serde(default)]
    options: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    port: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    command: Option<Vec<String>>,
    #[serde(default)]
    interval: Option<String>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    failures: Option<i64>,
    #[serde(default)]
    grace: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    id: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    image: String,
    replicas: i64,
    #[serde(default)]
    kill_timeout: Option<String>,
    #[serde(default)]
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    command: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResources {
    cpu: f64,
    memory: String,
    #[serde(default)]
    disk: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConstraints {
    #[serde(default)]
    required_class: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPort {
    name: String,
    container_port: i64,
    host_port: i64,
    #[serde(default)]
    host_ip: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVolume {
    #[serde(rename = "type")]
    kind: String,
    source: String,
    target: String,
    #[serde(default)]
    read_only: bool,
    /// Defaults to true for writable volumes (only one live workload uses
    /// the directory at a time); false allows sharing it.
    single_writer: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAutoscale {
    min_replicas: i64,
    max_replicas: i64,
    target_cpu_percent: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    #[serde(default)]
    max_parallel: Option<i64>,
    #[serde(default)]
    min_healthy_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRestart {
    #[serde(default)]
    attempts: Option<i64>,
    #[serde(default)]
    interval: Option<String>,
    #[serde(default)]
    delay: Option<String>,
}

// ============================================================================
// Public API
// ============================================================================

/// Result of a successful parse: the canonical spec plus any warnings.
#[derive(Debug, Clone)]
pub struct ParsedManifest {
    pub spec: ServiceSpec,
    pub warnings: Vec<ValidationError>,
}

/// Parse and validate a manifest. On failure every error (not warnings) is
/// returned; nothing is partially accepted.
pub fn parse_manifest_str(content: &str) -> Result<ParsedManifest, Vec<ValidationError>> {
    let raw: RawManifest = toml::from_str(content).map_err(|e| vec![toml_error(content, &e)])?;

    let mut issues = Vec::new();
    let spec = canonicalise(raw, &mut issues);
    if let Some(spec) = &spec {
        validate_service_spec_into(spec, &mut issues);
    }

    let (errors, warnings): (Vec<_>, Vec<_>) = issues.into_iter().partition(|e| !e.is_warning);
    match spec {
        Some(spec) if errors.is_empty() => Ok(ParsedManifest { spec, warnings }),
        _ => Err(errors),
    }
}

/// Validate an already-canonical spec (VE-01 … VE-07). Returns errors and
/// warnings together; check `is_warning`.
pub fn validate_service_spec(spec: &ServiceSpec) -> Vec<ValidationError> {
    let mut out = Vec::new();
    validate_service_spec_into(spec, &mut out);
    out
}

// ============================================================================
// Canonicalisation
// ============================================================================

fn duration_field(
    path: &str,
    v: Option<&str>,
    default: u32,
    out: &mut Vec<ValidationError>,
) -> u32 {
    match v {
        None => default,
        Some(s) => match parse_duration(s) {
            Ok(secs) if secs <= u32::MAX as u64 => secs as u32,
            _ => {
                out.push(ValidationError::new(
                    Code::InvalidResourceSpec,
                    path,
                    &format!("invalid duration {s:?} (use e.g. \"10s\", \"5m\")"),
                ));
                default
            }
        },
    }
}

fn size_field(path: &str, s: &str, out: &mut Vec<ValidationError>) -> u64 {
    match parse_size(s) {
        Ok(b) => b.as_u64(),
        Err(_) => {
            out.push(ValidationError::new(
                Code::InvalidResourceSpec,
                path,
                &format!("invalid size {s:?} (use IEC units, e.g. \"512MiB\")"),
            ));
            0
        }
    }
}

fn canonicalise(raw: RawManifest, out: &mut Vec<ValidationError>) -> Option<ServiceSpec> {
    let mut ok = true;

    if raw.service.replicas < 0 || raw.service.replicas > u32::MAX as i64 {
        out.push(ValidationError::new(
            Code::InvalidResourceSpec,
            "service.replicas",
            "replicas must be >= 0",
        ));
        ok = false;
    }
    if raw.resources.cpu <= 0.0 || !raw.resources.cpu.is_finite() {
        out.push(ValidationError::new(
            Code::InvalidResourceSpec,
            "resources.cpu",
            "cpu must be > 0",
        ));
        ok = false;
    }

    let mut ports = Vec::new();
    for (i, p) in raw.ports.iter().enumerate() {
        let in_range = |v: i64| (1..=65535).contains(&v);
        if !in_range(p.container_port) {
            out.push(ValidationError::new(
                Code::InvalidPortSpec,
                &format!("ports[{i}].container_port"),
                "port must be in 1-65535",
            ));
            ok = false;
        }
        if !in_range(p.host_port) {
            out.push(ValidationError::new(
                Code::InvalidPortSpec,
                &format!("ports[{i}].host_port"),
                "port must be in 1-65535",
            ));
            ok = false;
        }
        if let Some(ip) = &p.host_ip {
            if ip != "advertise" && ip.parse::<std::net::IpAddr>().is_err() {
                out.push(ValidationError::new(
                    Code::InvalidPortSpec,
                    &format!("ports[{i}].host_ip"),
                    &format!("{ip:?} is not an IP address or \"advertise\""),
                ));
                ok = false;
            }
        }
        ports.push(PortSpec {
            name: p.name.clone(),
            container_port: p.container_port.clamp(0, 65535) as u16,
            host_port: p.host_port.clamp(0, 65535) as u16,
            host_ip: p.host_ip.clone(),
        });
    }

    let autoscale = raw.autoscale.map(|a| {
        if a.min_replicas < 1 || a.max_replicas < a.min_replicas || a.max_replicas > u32::MAX as i64
        {
            out.push(ValidationError::new(
                Code::InvalidAutoscaleSpec,
                "autoscale.min_replicas",
                "require 1 <= min_replicas <= max_replicas",
            ));
            ok = false;
        }
        AutoscaleSpec {
            min_replicas: a.min_replicas.max(0) as u32,
            max_replicas: a.max_replicas.max(0) as u32,
            target_cpu_percent: a.target_cpu_percent,
        }
    });

    let update = match raw.update {
        None => UpdatePolicy::default(),
        Some(u) => {
            let max_parallel = u.max_parallel.unwrap_or(1);
            if max_parallel < 1 || max_parallel > u32::MAX as i64 {
                out.push(ValidationError::new(
                    Code::InvalidResourceSpec,
                    "update.max_parallel",
                    "max_parallel must be >= 1",
                ));
                ok = false;
            }
            UpdatePolicy {
                max_parallel: max_parallel.max(1) as u32,
                min_healthy_time_seconds: duration_field(
                    "update.min_healthy_time",
                    u.min_healthy_time.as_deref(),
                    10,
                    out,
                ),
            }
        }
    };

    let restart = match raw.restart {
        None => RestartPolicy::default(),
        Some(r) => {
            let attempts = r.attempts.unwrap_or(3);
            if attempts < 0 || attempts > u32::MAX as i64 {
                out.push(ValidationError::new(
                    Code::InvalidResourceSpec,
                    "restart.attempts",
                    "attempts must be >= 0",
                ));
                ok = false;
            }
            RestartPolicy {
                attempts: attempts.max(0) as u32,
                interval_seconds: duration_field(
                    "restart.interval",
                    r.interval.as_deref(),
                    300,
                    out,
                ),
                delay_seconds: duration_field("restart.delay", r.delay.as_deref(), 5, out),
            }
        }
    };

    // VE-07: sharing rules only apply to writable host volumes.
    for (i, v) in raw.volumes.iter().enumerate() {
        if v.kind == "ephemeral" {
            for (set, field, why) in [
                (
                    v.read_only,
                    "read_only",
                    "an ephemeral volume starts empty, so read_only would leave it unusable",
                ),
                (
                    v.single_writer.is_some(),
                    "single_writer",
                    "an ephemeral volume belongs to one workload; there is nothing to share",
                ),
            ] {
                if set {
                    out.push(ValidationError::new(
                        Code::InvalidVolumeSpec,
                        &format!("volumes[{i}].{field}"),
                        why,
                    ));
                }
            }
            continue;
        }
        if v.read_only && v.single_writer == Some(true) {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].single_writer"),
                "single_writer applies to writable volumes; a read_only volume is always shared",
            ));
        }
    }

    let check = raw.check.map(|c| {
        let mut bad = |field: &str, msg: String| {
            out.push(ValidationError::new(
                Code::InvalidCheckSpec,
                &format!("check.{field}"),
                &msg,
            ));
            ok = false;
        };
        let http = c.kind == "http";
        let exec = c.kind == "exec";
        if !http && !exec && c.kind != "tcp" {
            bad(
                "type",
                format!("{:?}: use \"tcp\", \"http\" or \"exec\"", c.kind),
            );
        }
        match (&c.port, exec) {
            (Some(_), true) => bad(
                "port",
                "an exec check runs in the container and has no port".into(),
            ),
            (None, false) => bad("port", "a tcp or http check needs a port".into()),
            (Some(port), false) if !raw.ports.iter().any(|p| &p.name == port) => bad(
                "port",
                format!("{port:?} is not the name of a [[ports]] entry"),
            ),
            _ => {}
        }
        match (&c.command, exec) {
            (None, true) => bad(
                "command",
                "an exec check needs a command, e.g. [\"pg_isready\"]".into(),
            ),
            (Some(cmd), true) if cmd.is_empty() || cmd[0].trim().is_empty() => {
                bad("command", "the command must name a program".into())
            }
            (Some(_), false) => bad("command", "command applies to exec checks".into()),
            _ => {}
        }
        match (&c.path, http) {
            (Some(p), true) if !p.starts_with('/') => {
                bad("path", format!("{p:?} must start with \"/\""))
            }
            (Some(_), false) => bad("path", "path applies to http checks".into()),
            _ => {}
        }
        let failures = c.failures.unwrap_or(3);
        if !(1..=1000).contains(&failures) {
            bad("failures", "failures must be 1 to 1000".into());
        }
        CheckSpec {
            kind: c.kind,
            port: c.port.unwrap_or_default(),
            command: c.command.filter(|_| exec),
            path: if http {
                Some(c.path.unwrap_or_else(|| "/".into()))
            } else {
                None
            },
            interval_seconds: duration_field("check.interval", c.interval.as_deref(), 10, out)
                .max(1),
            timeout_seconds: duration_field("check.timeout", c.timeout.as_deref(), 2, out).max(1),
            failures: failures.clamp(1, 1000) as u32,
            grace_seconds: duration_field("check.grace", c.grace.as_deref(), 0, out),
        }
    });

    for (name, reference) in &raw.secrets {
        if let Err(msg) = check_secret(name, reference, &raw.env) {
            out.push(ValidationError::new(
                Code::InvalidSecretRef,
                &format!("secrets.{name}"),
                &msg,
            ));
            ok = false;
        }
    }

    // ${node.NAME} in a value must name a node value the agent knows.
    for (key, value) in &raw.env {
        if value.contains("${node.") {
            if let Err(e) = interpolate_node(value, |n| {
                crate::models::NODE_VALUES.contains(&n).then_some("")
            }) {
                out.push(ValidationError::new(
                    Code::InvalidResourceSpec,
                    &format!("env.{key}"),
                    &format!(
                        "{e}: use ${{node.NAME}} with NAME one of {}",
                        crate::models::NODE_VALUES.join(", ")
                    ),
                ));
                ok = false;
            }
        }
    }

    // ${secret:NAME} in a value must name one of [secrets].
    for (key, value) in &raw.env {
        if value.contains("${secret:") {
            if let Err(e) =
                interpolate_secrets(value, |n| raw.secrets.contains_key(n).then_some(""))
            {
                out.push(ValidationError::new(
                    Code::InvalidSecretRef,
                    &format!("env.{key}"),
                    &format!("{e}: a ${{secret:NAME}} reference must name an entry of [secrets]"),
                ));
                ok = false;
            }
        }
    }

    let service_type = match raw.service.kind.as_deref() {
        None | Some("service") => ServiceType::Service,
        Some("batch") => ServiceType::Batch,
        Some(other) => {
            out.push(ValidationError::new(
                Code::InvalidResourceSpec,
                "service.type",
                &format!("{other:?}: type is \"service\" or \"batch\""),
            ));
            ok = false;
            ServiceType::Service
        }
    };
    if service_type == ServiceType::Batch {
        for (present, field, why) in [
            (check.is_some(), "check", "a batch job has no health check"),
            (
                autoscale.is_some(),
                "autoscale",
                "a batch job is not autoscaled",
            ),
        ] {
            if present {
                out.push(ValidationError::new(Code::InvalidResourceSpec, field, why));
                ok = false;
            }
        }
    }

    for (field, list) in [
        ("service.entrypoint", &raw.service.entrypoint),
        ("service.command", &raw.service.command),
    ] {
        if list.as_ref().is_some_and(|l| l.is_empty()) {
            out.push(ValidationError::new(
                Code::InvalidResourceSpec,
                field,
                "must not be empty; leave it out to use the image's",
            ));
            ok = false;
        }
    }

    let tmpfs_targets: Vec<String> = raw.volumes.iter().map(|v| v.target.clone()).collect();
    let security = raw
        .security
        .map(|r| canonical_security(r, &tmpfs_targets, out, &mut ok))
        .filter(|s| !s.is_default());
    let logging = raw.logging.map(|l| canonical_logging(l, out, &mut ok));
    let sysctls = canonical_sysctls(raw.sysctls, out, &mut ok);

    let spec = ServiceSpec {
        id: raw.service.id,
        service_type,
        image: normalise_image(&raw.service.image),
        entrypoint: raw.service.entrypoint,
        command: raw.service.command,
        replicas: raw.service.replicas.max(0) as u32,
        kill_timeout_seconds: duration_field(
            "service.kill_timeout",
            raw.service.kill_timeout.as_deref(),
            10,
            out,
        ),
        cpu_millicores: (raw.resources.cpu.max(0.0) * 1000.0).ceil() as u32,
        memory_bytes: size_field("resources.memory", &raw.resources.memory, out),
        disk_bytes: match raw.resources.disk.as_deref() {
            Some(d) => size_field("resources.disk", d, out),
            None => 1 << 30,
        },
        required_class: raw
            .constraints
            .and_then(|c| c.required_class)
            .filter(|c| !c.is_empty()),
        env: raw.env,
        secrets: raw.secrets,
        ports,
        volumes: raw
            .volumes
            .into_iter()
            .map(|v| VolumeSpec {
                kind: v.kind,
                source: v.source,
                target: v.target,
                read_only: v.read_only,
                shared: v.single_writer == Some(false),
            })
            .collect(),
        autoscale,
        update,
        restart,
        check,
        security,
        logging,
        sysctls,
    };
    ok.then_some(spec)
}

/// Linux capabilities, without the `CAP_` prefix (capabilities(7)).
const CAPABILITIES: &[&str] = &[
    "AUDIT_CONTROL",
    "AUDIT_READ",
    "AUDIT_WRITE",
    "BLOCK_SUSPEND",
    "BPF",
    "CHECKPOINT_RESTORE",
    "CHOWN",
    "DAC_OVERRIDE",
    "DAC_READ_SEARCH",
    "FOWNER",
    "FSETID",
    "IPC_LOCK",
    "IPC_OWNER",
    "KILL",
    "LEASE",
    "LINUX_IMMUTABLE",
    "MAC_ADMIN",
    "MAC_OVERRIDE",
    "MKNOD",
    "NET_ADMIN",
    "NET_BIND_SERVICE",
    "NET_BROADCAST",
    "NET_RAW",
    "PERFMON",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_ADMIN",
    "SYS_BOOT",
    "SYS_CHROOT",
    "SYS_MODULE",
    "SYS_NICE",
    "SYS_PACCT",
    "SYS_PTRACE",
    "SYS_RAWIO",
    "SYS_RESOURCE",
    "SYS_TIME",
    "SYS_TTY_CONFIG",
    "SYSLOG",
    "WAKE_ALARM",
];

/// `[security]`: capability names upper case without `CAP_`, sorted and
/// deduplicated; tmpfs paths absolute and not where a volume or `/alloc` is.
fn canonical_security(
    r: RawSecurity,
    volume_targets: &[String],
    out: &mut Vec<ValidationError>,
    ok: &mut bool,
) -> SecuritySpec {
    let mut bad = |field: &str, msg: String| {
        out.push(ValidationError::new(
            Code::InvalidSecuritySpec,
            &format!("security.{field}"),
            &msg,
        ));
        *ok = false;
    };
    let caps = |field: &str, list: Vec<String>, bad: &mut dyn FnMut(&str, String)| {
        let mut names: Vec<String> = Vec::new();
        for (i, c) in list.iter().enumerate() {
            let up = c.trim().to_ascii_uppercase();
            let name = up.strip_prefix("CAP_").unwrap_or(&up).to_string();
            if name == "ALL" {
                if field == "cap_add" {
                    bad(
                        &format!("{field}[{i}]"),
                        "cap_add = [\"ALL\"] gives the container every capability; name the ones it needs".into(),
                    );
                }
            } else if !CAPABILITIES.contains(&name.as_str()) {
                bad(
                    &format!("{field}[{i}]"),
                    format!("{c:?} is not a Linux capability, e.g. \"NET_BIND_SERVICE\""),
                );
            }
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names.sort();
        names
    };
    let cap_drop = caps("cap_drop", r.cap_drop, &mut bad);
    let cap_add = caps("cap_add", r.cap_add, &mut bad);
    for c in &cap_add {
        if cap_drop.contains(c) {
            bad(
                "cap_add",
                format!(
                    "{c} is in both cap_drop and cap_add; drop \"ALL\" and add it back instead"
                ),
            );
        }
    }
    let mut tmpfs: Vec<String> = Vec::new();
    for (i, t) in r.tmpfs.iter().enumerate() {
        let field = format!("tmpfs[{i}]");
        let (path, opts) = t.split_once(':').unwrap_or((t.as_str(), ""));
        let path = path.trim_end_matches('/');
        if !path.starts_with('/') || path.contains(char::is_whitespace) {
            bad(
                &field,
                format!("{t:?}: the path must be absolute, e.g. \"/tmp\""),
            );
            continue;
        }
        if t.contains(':') && (opts.is_empty() || opts.contains(char::is_whitespace)) {
            bad(
                &field,
                format!("{t:?}: options follow the path, e.g. \"/tmp:size=64m,mode=1777\""),
            );
        }
        if path == "/alloc" || path.starts_with("/alloc/") {
            bad(
                &field,
                "/alloc is the workload's own scratch directory".into(),
            );
        }
        if volume_targets
            .iter()
            .any(|v| v.trim_end_matches('/') == path)
        {
            bad(&field, format!("{path} is also a volume's target"));
        }
        if tmpfs.iter().any(|x| x.split(':').next() == Some(path)) {
            bad(&field, format!("duplicate tmpfs {path}"));
        }
        tmpfs.push(if opts.is_empty() {
            path.to_string()
        } else {
            format!("{path}:{opts}")
        });
    }
    SecuritySpec {
        read_only_root: r.read_only_root,
        no_new_privileges: r.no_new_privileges,
        cap_drop,
        cap_add,
        tmpfs,
        instance_metadata: r.instance_metadata,
    }
}

/// The most `[sysctls]` one service may set.
const MAX_SYSCTLS: usize = 32;

/// `[sysctls]`: kernel parameters of the container's own network namespace
/// (`net.*`, which Docker sets per container); a value is a number, a word or
/// a few of them (`"4096 87380 6291456"`).
fn canonical_sysctls(
    raw: BTreeMap<String, String>,
    out: &mut Vec<ValidationError>,
    ok: &mut bool,
) -> BTreeMap<String, String> {
    static NAME: OnceLock<Regex> = OnceLock::new();
    static VALUE: OnceLock<Regex> = OnceLock::new();
    let name_re = NAME.get_or_init(|| Regex::new(r"^net(\.[a-z0-9_\-]+)+$").unwrap());
    let value_re =
        VALUE.get_or_init(|| Regex::new(r"^[A-Za-z0-9_.:\-]+( [A-Za-z0-9_.:\-]+)*$").unwrap());
    let mut bad = |field: &str, msg: String| {
        out.push(ValidationError::new(
            Code::InvalidSysctlSpec,
            &format!("sysctls.{field}"),
            &msg,
        ));
        *ok = false;
    };
    if raw.len() > MAX_SYSCTLS {
        bad("", format!("{} sysctls; at most {MAX_SYSCTLS}", raw.len()));
    }
    for (name, value) in &raw {
        if !name_re.is_match(name) {
            bad(
                name,
                format!("{name:?}: only the container's network namespace can be set, a net.* name, e.g. \"net.ipv4.tcp_retries2\""),
            );
        }
        if value.len() > 128 || !value_re.is_match(value) {
            bad(
                name,
                format!("{value:?}: a number or a word, or a few separated by single spaces"),
            );
        }
    }
    raw
}

/// Log drivers that send over the network: with Docker's default `blocking`
/// mode an unreachable destination can block the container's writes.
const REMOTE_LOG_DRIVERS: &[&str] = &[
    "awslogs",
    "fluentd",
    "gcplogs",
    "gelf",
    "logentries",
    "splunk",
    "syslog",
];

/// `[logging]`: a driver name, option keys Docker would accept, and in the
/// values only `${node.NAME}`, `${service.id}` and `${workload.id}`.
fn canonical_logging(l: RawLogging, out: &mut Vec<ValidationError>, ok: &mut bool) -> LoggingSpec {
    static DRIVER: OnceLock<Regex> = OnceLock::new();
    static KEY: OnceLock<Regex> = OnceLock::new();
    // Built-in names, or a plugin's "[registry/]name[:tag]".
    let driver_re = DRIVER.get_or_init(|| {
        Regex::new(r"^[a-z0-9][a-z0-9._\-]*(/[a-z0-9][a-z0-9._\-]*)*(:[A-Za-z0-9_.\-]+)?$").unwrap()
    });
    let key_re = KEY.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_.\-]*$").unwrap());
    let mut bad = |field: &str, msg: String| {
        out.push(ValidationError::new(
            Code::InvalidLoggingSpec,
            &format!("logging.{field}"),
            &msg,
        ));
        *ok = false;
    };
    let driver = l.driver.trim().to_string();
    if !driver_re.is_match(&driver) {
        bad(
            "driver",
            format!(
                "{:?} is not a log driver name, e.g. \"awslogs\" or \"json-file\"",
                l.driver
            ),
        );
    }
    if driver == "none" && !l.options.is_empty() {
        bad("options", "the \"none\" driver takes no options".into());
    }
    for (k, v) in &l.options {
        let field = format!("options.{k}");
        if !key_re.is_match(k) {
            bad(&field, format!("{k:?} is not a log option name"));
        }
        if v.contains("${secret:") {
            bad(
                &field,
                "log options can't hold secrets: they are visible in docker inspect".into(),
            );
            continue;
        }
        let checked =
            interpolate_node(v, |n| crate::models::NODE_VALUES.contains(&n).then_some(""))
                .and_then(|v| interpolate_workload(&v, "", ""));
        if let Err(e) = checked {
            bad(
                &field,
                format!(
                    "{e}: use ${{node.NAME}} with NAME one of {}, ${{service.id}} or ${{workload.id}}",
                    crate::models::NODE_VALUES.join(", ")
                ),
            );
        }
    }
    if REMOTE_LOG_DRIVERS.contains(&driver.as_str())
        && l.options.get("mode").map(String::as_str) != Some("non-blocking")
    {
        out.push(ValidationError::warning(
            Code::InvalidLoggingSpec,
            "logging.options.mode",
            &format!(
                "{driver} sends over the network; without mode = \"non-blocking\" an unreachable destination can block the container's writes"
            ),
        ));
    }
    LoggingSpec {
        driver,
        options: l.options,
    }
}

/// VE-08: a secret is an environment variable name not also set in [env],
/// with an `ssm:<parameter name>` or `file:<absolute path>` reference.
fn check_secret(name: &str, reference: &str, env: &BTreeMap<String, String>) -> Result<(), String> {
    static NAME: OnceLock<Regex> = OnceLock::new();
    static SSM: OnceLock<Regex> = OnceLock::new();
    let name_re = NAME.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap());
    // SSM parameter names: letters, digits and _.-/; a hierarchy starts with "/".
    let ssm_re =
        SSM.get_or_init(|| Regex::new(r"^/?[A-Za-z0-9_.\-]+(/[A-Za-z0-9_.\-]+)*$").unwrap());
    if !name_re.is_match(name) {
        return Err(format!("{name:?} is not a valid environment variable name"));
    }
    if env.contains_key(name) {
        return Err(format!("{name} is set in both [env] and [secrets]"));
    }
    if let Some(param) = reference.strip_prefix("ssm:") {
        if !ssm_re.is_match(param) {
            return Err(format!("{reference:?} is not a valid SSM parameter name"));
        }
    } else if let Some(path) = reference.strip_prefix("file:") {
        if !path.starts_with('/') {
            return Err(format!(
                "{reference:?}: a file reference needs an absolute path"
            ));
        }
    } else {
        return Err(format!(
            "{reference:?}: references start with \"ssm:\" or \"file:\""
        ));
    }
    Ok(())
}

/// Images without a tag or digest default to `:latest` (VE-02). The canonical
/// form makes this explicit so the hash is stable.
fn normalise_image(image: &str) -> String {
    let image = image.trim();
    if image.contains('@') || last_path_segment(image).contains(':') {
        image.to_string()
    } else {
        format!("{image}:latest")
    }
}

fn last_path_segment(image: &str) -> &str {
    image.rsplit('/').next().unwrap_or(image)
}

fn toml_error(content: &str, e: &toml::de::Error) -> ValidationError {
    let msg = e.message().to_string();
    // The table the error is in: the last `[header]` before the error span.
    let table = e
        .span()
        .and_then(|span| {
            content[..span.start.min(content.len())]
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| l.starts_with('['))
                .map(|h| h.trim_matches(|c| c == '[' || c == ']').trim().to_string())
        })
        .unwrap_or_default();
    let path = |field: &str| {
        if table.is_empty() {
            field.to_string()
        } else {
            format!("{table}.{field}")
        }
    };
    // serde reports unknown keys as "unknown field `x`, expected one of ...".
    if let Some(rest) = msg.strip_prefix("unknown field `") {
        let field = rest.split('`').next().unwrap_or("");
        // Unknown top-level tables are reported at their own header.
        let p = if table == field {
            field.to_string()
        } else {
            path(field)
        };
        return ValidationError::new(Code::UnknownField, &p, &msg);
    }
    if msg.starts_with("missing field") {
        let field = msg.split('`').nth(1).unwrap_or("");
        return ValidationError::new(Code::UnknownField, &path(field), &msg);
    }
    ValidationError::new(
        Code::UnknownField,
        &table,
        &format!("TOML parse error: {e}"),
    )
}

// ============================================================================
// Validation rules
// ============================================================================

fn service_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9]([-a-z0-9]{0,61}[a-z0-9])?$").unwrap())
}

fn image_re() -> &'static Regex {
    // [registry[:port]/]path[:tag][@sha256:digest]
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^([a-zA-Z0-9.-]+(:[0-9]+)?/)?[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-]+[a-z0-9]+)*)*(:[A-Za-z0-9_][A-Za-z0-9_.-]{0,127})?(@sha256:[a-f0-9]{64})?$",
        )
        .unwrap()
    })
}

fn validate_service_spec_into(spec: &ServiceSpec, out: &mut Vec<ValidationError>) {
    // VE-03
    if !service_id_re().is_match(&spec.id) {
        out.push(ValidationError::new(
            Code::InvalidServiceId,
            "service.id",
            &format!(
                "{:?} must match ^[a-z0-9]([-a-z0-9]{{0,61}}[a-z0-9])?$",
                spec.id
            ),
        ));
    }

    // VE-01
    if spec.cpu_millicores == 0 {
        out.push(ValidationError::new(
            Code::InvalidResourceSpec,
            "resources.cpu",
            "cpu must be > 0",
        ));
    }
    if spec.memory_bytes < 4 << 20 {
        out.push(ValidationError::new(
            Code::InvalidResourceSpec,
            "resources.memory",
            "memory must be >= 4MiB",
        ));
    }
    if spec.disk_bytes == 0 {
        out.push(ValidationError::new(
            Code::InvalidResourceSpec,
            "resources.disk",
            "disk must be > 0",
        ));
    }

    // VE-02
    if !image_re().is_match(&spec.image) {
        out.push(ValidationError::new(
            Code::InvalidImageRef,
            "service.image",
            &format!("{:?} is not a valid OCI reference", spec.image),
        ));
    } else if spec.image.ends_with(":latest") {
        out.push(ValidationError::warning(
            Code::InvalidImageRef,
            "service.image",
            "image uses :latest; re-applying it will not pull a newer image. Pin a tag or digest.",
        ));
    }

    // VE-04
    if let Some(a) = &spec.autoscale {
        if a.min_replicas < 1 || a.max_replicas < a.min_replicas {
            out.push(ValidationError::new(
                Code::InvalidAutoscaleSpec,
                "autoscale.min_replicas",
                "require 1 <= min_replicas <= max_replicas",
            ));
        }
        if !(a.target_cpu_percent > 0.0 && a.target_cpu_percent < 100.0) {
            out.push(ValidationError::new(
                Code::InvalidAutoscaleSpec,
                "autoscale.target_cpu_percent",
                "target_cpu_percent must be > 0 and < 100",
            ));
        }
    }

    // VE-05
    let mut seen = HashSet::new();
    for (i, p) in spec.ports.iter().enumerate() {
        if p.container_port == 0 {
            out.push(ValidationError::new(
                Code::InvalidPortSpec,
                &format!("ports[{i}].container_port"),
                "port must be in 1-65535",
            ));
        }
        if p.host_port == 0 {
            out.push(ValidationError::new(
                Code::InvalidPortSpec,
                &format!("ports[{i}].host_port"),
                "port must be in 1-65535",
            ));
        } else if !seen.insert(p.host_port) {
            out.push(ValidationError::new(
                Code::InvalidPortSpec,
                &format!("ports[{i}].host_port"),
                &format!("duplicate host_port {}", p.host_port),
            ));
        } else if p.host_port < 1024 {
            out.push(ValidationError::warning(
                Code::InvalidPortSpec,
                &format!("ports[{i}].host_port"),
                &format!("host_port {} is a privileged port (< 1024)", p.host_port),
            ));
        }
    }

    // VE-07
    let mut targets = HashSet::new();
    for (i, v) in spec.volumes.iter().enumerate() {
        if v.kind != "host" && v.kind != "ephemeral" {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].type"),
                &format!(
                    "unknown volume type {:?}; use \"host\" or \"ephemeral\"",
                    v.kind
                ),
            ));
        }
        // An ephemeral volume's source names its directory on the node.
        if v.kind == "ephemeral"
            && !v.source.is_empty()
            && !(v.source.len() <= 63
                && v.source.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                })
                && !v.source.starts_with(['-', '_']))
        {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].source"),
                "an ephemeral volume's source is a name: lowercase letters, digits, - and _, up to 63",
            ));
        }
        if v.target == "/alloc" || v.target.starts_with("/alloc/") {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].target"),
                "/alloc is the workload's own scratch directory; mount elsewhere",
            ));
        }
        if v.source.is_empty() {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].source"),
                "source is required",
            ));
        }
        if !v.target.starts_with('/') {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].target"),
                "target must be an absolute path",
            ));
        } else if !targets.insert(v.target.as_str()) {
            out.push(ValidationError::new(
                Code::InvalidVolumeSpec,
                &format!("volumes[{i}].target"),
                &format!("duplicate target {}", v.target),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
[service]
id       = "frontend-web"
image    = "docker.io/library/nginx:1.27.2-alpine-slim"
replicas = 3

[resources]
cpu    = 0.5
memory = "512MiB"

[constraints]
required_class = "general"

[[ports]]
name           = "http"
container_port = 80
host_port      = 8080

[env]
A = "1"

[autoscale]
min_replicas       = 3
max_replicas       = 10
target_cpu_percent = 75
"#;

    fn err_code(toml: &str) -> (Code, String) {
        let e = parse_manifest_str(toml).unwrap_err();
        (e[0].code.clone(), e[0].field_path.clone())
    }

    #[test]
    fn parses_spec_example() {
        let p = parse_manifest_str(VALID).unwrap();
        assert_eq!(p.spec.id, "frontend-web");
        assert_eq!(p.spec.cpu_millicores, 500);
        assert_eq!(p.spec.memory_bytes, 512 << 20);
        assert_eq!(p.spec.disk_bytes, 1 << 30);
        assert_eq!(p.spec.required_class.as_deref(), Some("general"));
        assert_eq!(p.spec.ports[0].host_port, 8080);
        assert_eq!(p.spec.autoscale.as_ref().unwrap().max_replicas, 10);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn cpu_rounds_up() {
        let m = VALID.replace("cpu    = 0.5", "cpu    = 0.0001");
        assert_eq!(parse_manifest_str(&m).unwrap().spec.cpu_millicores, 1);
    }

    #[test]
    fn missing_tag_warns_and_normalises() {
        let m = VALID.replace("nginx:1.27.2-alpine-slim", "nginx");
        let p = parse_manifest_str(&m).unwrap();
        assert_eq!(p.spec.image, "docker.io/library/nginx:latest");
        assert_eq!(p.warnings[0].code, Code::InvalidImageRef);
    }

    #[test]
    fn registry_with_port_is_valid() {
        let m = VALID.replace(
            "docker.io/library/nginx:1.27.2-alpine-slim",
            "registry.local:5000/team/app:1.0",
        );
        assert!(parse_manifest_str(&m).is_ok());
    }

    #[test]
    fn ve01_resources() {
        assert_eq!(
            err_code(&VALID.replace("\"512MiB\"", "\"1MiB\"")).0,
            Code::InvalidResourceSpec
        );
        assert_eq!(
            err_code(&VALID.replace("cpu    = 0.5", "cpu    = 0.0")).0,
            Code::InvalidResourceSpec
        );
        assert_eq!(
            err_code(&VALID.replace("replicas = 3", "replicas = -1")).0,
            Code::InvalidResourceSpec
        );
    }

    #[test]
    fn ve02_image() {
        assert_eq!(
            err_code(&VALID.replace("docker.io/library/nginx:1.27.2-alpine-slim", "Bad Image!")).0,
            Code::InvalidImageRef
        );
    }

    #[test]
    fn ve03_service_id() {
        assert_eq!(
            err_code(&VALID.replace("frontend-web", "Frontend_Web")),
            (Code::InvalidServiceId, "service.id".into())
        );
    }

    #[test]
    fn ve04_autoscale() {
        assert_eq!(
            err_code(&VALID.replace("max_replicas       = 10", "max_replicas       = 1")).0,
            Code::InvalidAutoscaleSpec
        );
        assert_eq!(
            err_code(&VALID.replace("target_cpu_percent = 75", "target_cpu_percent = 100")).0,
            Code::InvalidAutoscaleSpec
        );
    }

    #[test]
    fn ve05_ports() {
        let dup =
            format!("{VALID}\n[[ports]]\nname = \"x\"\ncontainer_port = 81\nhost_port = 8080\n");
        assert_eq!(
            err_code(&dup),
            (Code::InvalidPortSpec, "ports[1].host_port".into())
        );
        assert_eq!(
            err_code(&VALID.replace("host_port      = 8080", "host_port      = 70000")).0,
            Code::InvalidPortSpec
        );
        let low =
            parse_manifest_str(&VALID.replace("host_port      = 8080", "host_port      = 80"))
                .unwrap();
        assert_eq!(low.warnings[0].code, Code::InvalidPortSpec);
    }

    #[test]
    fn ve06_unknown_field() {
        let (code, path) = err_code(&VALID.replace("replicas = 3", "replicas = 3\nreplica = 3"));
        assert_eq!(code, Code::UnknownField);
        assert_eq!(path, "service.replica");
        assert_eq!(
            err_code(&format!("{VALID}\n[bogus]\nx = 1\n")),
            (Code::UnknownField, "bogus".into())
        );
        let port_typo = VALID.replace("host_port      = 8080", "hostport = 8080");
        assert_eq!(
            err_code(&port_typo),
            (Code::UnknownField, "ports.hostport".into())
        );
    }

    #[test]
    fn ve07_volumes() {
        let bad =
            format!("{VALID}\n[[volumes]]\ntype = \"nfs\"\nsource = \"a\"\ntarget = \"rel\"\n");
        let errs = parse_manifest_str(&bad).unwrap_err();
        assert!(errs.iter().all(|e| e.code == Code::InvalidVolumeSpec));
        assert_eq!(errs.len(), 2);
    }

    #[test]
    fn volumes_default_to_single_writer_when_writable() {
        let vol = |extra: &str| {
            let m = format!(
                "{VALID}\n[[volumes]]\ntype = \"host\"\nsource = \"a\"\ntarget = \"/a\"\n{extra}"
            );
            parse_manifest_str(&m).map(|p| p.spec.volumes[0].clone())
        };
        let writable = vol("").unwrap();
        assert!(writable.single_writer() && !writable.shared);
        let shared = vol("single_writer = false\n").unwrap();
        assert!(!shared.single_writer() && shared.shared);
        assert!(vol("single_writer = true\n").unwrap().single_writer());
        assert!(!vol("read_only = true\n").unwrap().single_writer());
        let errs = vol("read_only = true\nsingle_writer = true\n").unwrap_err();
        assert_eq!(errs[0].field_path, "volumes[0].single_writer");
    }

    /// Every example manifest parses without errors or warnings.
    #[test]
    fn examples_are_valid() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "toml") {
                let text = std::fs::read_to_string(&path).unwrap();
                let parsed = parse_manifest_str(&text)
                    .unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
                assert!(
                    parsed.warnings.is_empty(),
                    "{}: {:?}",
                    path.display(),
                    parsed.warnings
                );
                seen += 1;
            }
        }
        assert!(seen >= 7);
        let pg = parse_manifest_str(&std::fs::read_to_string(dir.join("postgres.toml")).unwrap())
            .unwrap();
        assert!(pg.spec.stop_first());
        let logs =
            parse_manifest_str(&std::fs::read_to_string(dir.join("shared-logs.toml")).unwrap())
                .unwrap();
        assert!(!logs.spec.stop_first());
    }

    #[test]
    fn command_entrypoint_and_bind_address() {
        let m = VALID
            .replace(
                "replicas = 3\n",
                "replicas = 3\nentrypoint = [\"/bin/sh\", \"-c\"]\ncommand = [\"postgres\", \"-c\", \"wal_level=logical\"]\n",
            )
            .replace("host_port      = 8080", "host_port      = 8080\nhost_ip        = \"advertise\"");
        let p = parse_manifest_str(&m).unwrap();
        assert_eq!(
            p.spec.entrypoint.as_deref(),
            Some(&["/bin/sh".to_string(), "-c".to_string()][..])
        );
        assert_eq!(p.spec.command.as_ref().unwrap()[2], "wal_level=logical");
        assert_eq!(p.spec.ports[0].host_ip.as_deref(), Some("advertise"));

        let ip = VALID.replace(
            "host_port      = 8080",
            "host_port      = 8080\nhost_ip = \"127.0.0.1\"",
        );
        assert_eq!(
            parse_manifest_str(&ip).unwrap().spec.ports[0]
                .host_ip
                .as_deref(),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn rejects_bad_command_and_bind_address() {
        let empty = VALID.replace("replicas = 3\n", "replicas = 3\ncommand = []\n");
        assert_eq!(
            err_code(&empty),
            (Code::InvalidResourceSpec, "service.command".into())
        );
        let bad = VALID.replace(
            "host_port      = 8080",
            "host_port      = 8080\nhost_ip = \"localhost\"",
        );
        assert_eq!(
            err_code(&bad),
            (Code::InvalidPortSpec, "ports[0].host_ip".into())
        );
    }

    /// Specs without the new fields serialise as before, so their hashes do
    /// not change and re-applying them after an upgrade is `unchanged`.
    #[test]
    fn new_fields_keep_old_hashes() {
        let json = parse_manifest_str(VALID).unwrap().spec.canonical_json();
        for key in [
            "\"command\"",
            "\"entrypoint\"",
            "\"host_ip\"",
            "\"secrets\"",
            "\"check\"",
            "\"security\"",
            "\"logging\"",
        ] {
            assert!(!json.contains(key), "{key} in {json}");
        }
    }

    #[test]
    fn secrets() {
        let ok = format!(
            "{VALID}\n[secrets]\nPOSTGRES_PASSWORD = \"ssm:/valkv/dev/workloads/POSTGRES_PASSWORD\"\nTOKEN = \"file:/run/secrets/token\"\nPLAIN = \"ssm:plain-name\"\n"
        );
        let spec = parse_manifest_str(&ok).unwrap().spec;
        assert_eq!(spec.secrets.len(), 3);
        assert_eq!(spec.secrets["TOKEN"], "file:/run/secrets/token");

        for (secret, why) in [
            ("1BAD = \"ssm:/x\"", "name"),
            ("A = \"ssm:/x\"", "also in [env]"),
            ("X = \"vault:secret/x\"", "scheme"),
            ("X = \"file:relative/path\"", "relative path"),
            ("X = \"ssm:/has space\"", "SSM name"),
            ("X = \"ssm:\"", "empty SSM name"),
        ] {
            let m = format!("{VALID}\n[secrets]\n{secret}\n");
            let (code, _) = err_code(&m);
            assert_eq!(code, Code::InvalidSecretRef, "{why}: {secret}");
        }
    }

    #[test]
    fn health_checks() {
        let tcp = format!("{VALID}\n[check]\ntype = \"tcp\"\nport = \"http\"\n");
        let c = parse_manifest_str(&tcp).unwrap().spec.check.unwrap();
        assert_eq!((c.kind.as_str(), c.path.as_deref()), ("tcp", None));
        assert_eq!(
            (
                c.interval_seconds,
                c.timeout_seconds,
                c.failures,
                c.grace_seconds
            ),
            (10, 2, 3, 0)
        );

        let http = format!(
            "{VALID}\n[check]\ntype = \"http\"\nport = \"http\"\npath = \"/healthz\"\ninterval = \"5s\"\nfailures = 2\ngrace = \"1m\"\n"
        );
        let c = parse_manifest_str(&http).unwrap().spec.check.unwrap();
        assert_eq!(
            (
                c.path.as_deref(),
                c.interval_seconds,
                c.failures,
                c.grace_seconds
            ),
            (Some("/healthz"), 5, 2, 60)
        );
        let root = format!("{VALID}\n[check]\ntype = \"http\"\nport = \"http\"\n");
        assert_eq!(
            parse_manifest_str(&root)
                .unwrap()
                .spec
                .check
                .unwrap()
                .path
                .as_deref(),
            Some("/")
        );

        for (check, field) in [
            ("type = \"grpc\"\nport = \"http\"", "check.type"),
            ("type = \"tcp\"\nport = \"nope\"", "check.port"),
            (
                "type = \"tcp\"\nport = \"http\"\npath = \"/x\"",
                "check.path",
            ),
            (
                "type = \"http\"\nport = \"http\"\npath = \"healthz\"",
                "check.path",
            ),
            (
                "type = \"tcp\"\nport = \"http\"\nfailures = 0",
                "check.failures",
            ),
            ("type = \"tcp\"", "check.port"),
            ("type = \"exec\"", "check.command"),
            ("type = \"exec\"\ncommand = []", "check.command"),
            (
                "type = \"exec\"\ncommand = [\"true\"]\nport = \"http\"",
                "check.port",
            ),
            (
                "type = \"tcp\"\nport = \"http\"\ncommand = [\"true\"]",
                "check.command",
            ),
        ] {
            let m = format!("{VALID}\n[check]\n{check}\n");
            assert_eq!(
                err_code(&m),
                (Code::InvalidCheckSpec, field.into()),
                "{check}"
            );
        }
    }

    #[test]
    fn exec_health_check() {
        let m = format!(
            "{VALID}\n[check]\ntype = \"exec\"\ncommand = [\"pg_isready\", \"-U\", \"valkv\"]\ntimeout = \"5s\"\n"
        );
        let spec = parse_manifest_str(&m).unwrap().spec;
        let c = spec.check.clone().unwrap();
        assert_eq!(c.kind, "exec");
        assert_eq!(c.port, "");
        assert_eq!(
            c.command.as_deref(),
            Some(&["pg_isready".to_string(), "-U".into(), "valkv".into()][..])
        );
        assert_eq!(c.timeout_seconds, 5);
        // Older agents would ignore the command: only agents with exec_checks.
        let needs = spec.required_features();
        assert!(needs.contains(&"health_checks") && needs.contains(&"exec_checks"));
    }

    /// Adding `command` and making `port` skippable must not change the
    /// stored form of existing tcp/http checks (spec_hash relies on it).
    #[test]
    fn tcp_check_serialises_as_before() {
        let tcp = format!("{VALID}\n[check]\ntype = \"tcp\"\nport = \"http\"\n");
        let c = parse_manifest_str(&tcp).unwrap().spec.check.unwrap();
        assert_eq!(
            serde_json::to_string(&c).unwrap(),
            r#"{"type":"tcp","port":"http","interval_seconds":10,"timeout_seconds":2,"failures":3,"grace_seconds":0}"#
        );
    }

    #[test]
    fn batch_type() {
        let batch = VALID.replace("replicas = 3", "replicas = 3\ntype     = \"batch\"");
        // VALID has [autoscale]: not for a batch job.
        assert_eq!(
            err_code(&batch),
            (Code::InvalidResourceSpec, "autoscale".into())
        );
        let no_autoscale = batch[..batch.find("[autoscale]").unwrap()].to_string();
        let p = parse_manifest_str(&no_autoscale).unwrap();
        assert!(p.spec.is_batch());
        let checked = format!("{no_autoscale}\n[check]\ntype = \"tcp\"\nport = \"http\"\n");
        assert_eq!(
            err_code(&checked),
            (Code::InvalidResourceSpec, "check".into())
        );
        let odd = VALID.replace("replicas = 3", "replicas = 3\ntype     = \"cron\"");
        assert_eq!(
            err_code(&odd),
            (Code::InvalidResourceSpec, "service.type".into())
        );
        assert!(!parse_manifest_str(VALID).unwrap().spec.is_batch());
    }

    #[test]
    fn secrets_inside_values() {
        let ok = VALID.replace(
            "A = \"1\"",
            "A = \"1\"\nURL = \"postgresql://u:${secret:PW}@db/x\"\n\n[secrets]\nPW = \"ssm:/p/PW\"",
        );
        let p = parse_manifest_str(&ok).unwrap();
        assert_eq!(p.spec.env["URL"], "postgresql://u:${secret:PW}@db/x");
        assert!(p.spec.required_features().contains(&"secret_values"));
        let unknown = VALID.replace("A = \"1\"", "URL = \"x${secret:NOPE}\"");
        assert_eq!(
            err_code(&unknown),
            (Code::InvalidSecretRef, "env.URL".into())
        );
        let unclosed = ok.replace("${secret:PW}@", "${secret:PW@");
        assert_eq!(
            err_code(&unclosed),
            (Code::InvalidSecretRef, "env.URL".into())
        );
    }

    #[test]
    fn ephemeral_volumes() {
        let with = |v: &str| format!("{VALID}\n[[volumes]]\ntype = \"ephemeral\"\n{v}");
        let ok =
            parse_manifest_str(&with("source = \"cache\"\ntarget = \"/var/cache/app\"\n")).unwrap();
        let v = &ok.spec.volumes[0];
        assert!(
            !v.is_host() && !v.single_writer(),
            "per workload: nothing to share"
        );
        assert!(
            !ok.spec.stop_first(),
            "no stop-first update for an ephemeral volume"
        );
        assert!(ok.spec.required_features().contains(&"ephemeral_volumes"));
        for (extra, field) in [
            (
                "source = \"cache\"\ntarget = \"/c\"\nread_only = true\n",
                "volumes[0].read_only",
            ),
            (
                "source = \"cache\"\ntarget = \"/c\"\nsingle_writer = false\n",
                "volumes[0].single_writer",
            ),
            (
                "source = \"../etc\"\ntarget = \"/c\"\n",
                "volumes[0].source",
            ),
            (
                "source = \"cache\"\ntarget = \"/alloc/x\"\n",
                "volumes[0].target",
            ),
        ] {
            assert_eq!(
                err_code(&with(extra)),
                (Code::InvalidVolumeSpec, field.into()),
                "{extra}"
            );
        }
        let odd =
            format!("{VALID}\n[[volumes]]\ntype = \"tmpfs\"\nsource = \"x\"\ntarget = \"/x\"\n");
        assert_eq!(
            err_code(&odd),
            (Code::InvalidVolumeSpec, "volumes[0].type".into())
        );
    }

    #[test]
    fn node_values() {
        let ok = VALID.replace(
            "A = \"1\"",
            "A = \"1\"\nURL = \"http://${node.advertise_ip}:4000\"\nID = \"${node.id}\"",
        );
        let p = parse_manifest_str(&ok).unwrap();
        assert!(p.spec.required_features().contains(&"node_values"));
        let bad = VALID.replace("A = \"1\"", "X = \"${node.instance_id}\"");
        assert_eq!(err_code(&bad), (Code::InvalidResourceSpec, "env.X".into()));
    }

    #[test]
    fn security() {
        let m = format!(
            "{VALID}\n[security]\nread_only_root = true\nno_new_privileges = true\ncap_drop = [\"all\"]\ncap_add = [\"CAP_NET_BIND_SERVICE\", \"chown\", \"CHOWN\"]\ntmpfs = [\"/tmp\", \"/run/:size=16m,mode=1777\"]\n"
        );
        let p = parse_manifest_str(&m).unwrap();
        let s = p.spec.security.clone().unwrap();
        assert!(s.read_only_root && s.no_new_privileges);
        assert_eq!(s.cap_drop, ["ALL"]);
        assert_eq!(s.cap_add, ["CHOWN", "NET_BIND_SERVICE"]);
        assert_eq!(s.tmpfs, ["/tmp", "/run:size=16m,mode=1777"]);
        assert!(p.spec.required_features().contains(&"security"));

        // instance_metadata alone needs no hardening feature from the agent.
        let im = format!("{VALID}\n[security]\ninstance_metadata = true\n");
        let p = parse_manifest_str(&im).unwrap();
        assert!(p.spec.security.as_ref().unwrap().instance_metadata);
        assert!(!p.spec.required_features().contains(&"security"));
        assert_ne!(p.spec, parse_manifest_str(VALID).unwrap().spec);

        // Every setting off is the same spec, and hash, as no [security].
        let off = format!("{VALID}\n[security]\nread_only_root = false\ncap_drop = []\n");
        assert_eq!(
            parse_manifest_str(&off).unwrap().spec,
            parse_manifest_str(VALID).unwrap().spec
        );

        for (sec, field) in [
            ("cap_drop = [\"NET_TELEPATHY\"]", "security.cap_drop[0]"),
            ("cap_add = [\"ALL\"]", "security.cap_add[0]"),
            (
                "cap_drop = [\"CHOWN\"]\ncap_add = [\"CHOWN\"]",
                "security.cap_add",
            ),
            ("tmpfs = [\"tmp\"]", "security.tmpfs[0]"),
            ("tmpfs = [\"/tmp:\"]", "security.tmpfs[0]"),
            ("tmpfs = [\"/alloc/x\"]", "security.tmpfs[0]"),
            ("tmpfs = [\"/tmp\", \"/tmp:size=1m\"]", "security.tmpfs[1]"),
        ] {
            let m = format!("{VALID}\n[security]\n{sec}\n");
            assert_eq!(
                err_code(&m),
                (Code::InvalidSecuritySpec, field.into()),
                "{sec}"
            );
        }
        let clash = format!(
            "{VALID}\n[[volumes]]\ntype = \"ephemeral\"\nsource = \"scratch\"\ntarget = \"/tmp\"\n\n[security]\ntmpfs = [\"/tmp\"]\n"
        );
        assert_eq!(
            err_code(&clash),
            (Code::InvalidSecuritySpec, "security.tmpfs[0]".into())
        );
        let typo = format!("{VALID}\n[security]\nreadonly_root = true\n");
        assert_eq!(err_code(&typo).0, Code::UnknownField);
    }

    #[test]
    fn sysctls() {
        let m = format!(
            "{VALID}\n[sysctls]\n\"net.ipv4.tcp_retries2\" = \"6\"\n\"net.ipv4.tcp_rmem\" = \"4096 87380 6291456\"\n"
        );
        let p = parse_manifest_str(&m).unwrap();
        assert_eq!(p.spec.sysctls["net.ipv4.tcp_retries2"], "6");
        assert_eq!(p.spec.sysctls["net.ipv4.tcp_rmem"], "4096 87380 6291456");
        assert!(p.spec.required_features().contains(&"sysctls"));

        // None is the same spec, and hash, as before the field existed.
        let none = parse_manifest_str(VALID).unwrap().spec;
        assert!(none.sysctls.is_empty() && !none.required_features().contains(&"sysctls"));
        assert!(!serde_json::to_string(&none).unwrap().contains("sysctls"));

        for (line, field) in [
            ("\"kernel.shmmax\" = \"1\"", "sysctls.kernel.shmmax"),
            ("\"vm.swappiness\" = \"1\"", "sysctls.vm.swappiness"),
            ("\"net\" = \"1\"", "sysctls.net"),
            (
                "\"net.ipv4.tcp_retries2\" = \"\"",
                "sysctls.net.ipv4.tcp_retries2",
            ),
            (
                "\"net.ipv4.tcp_retries2\" = \"6; reboot\"",
                "sysctls.net.ipv4.tcp_retries2",
            ),
            (
                "\"net.ipv4.tcp_retries2\" = \"6  7\"",
                "sysctls.net.ipv4.tcp_retries2",
            ),
        ] {
            let m = format!("{VALID}\n[sysctls]\n{line}\n");
            assert_eq!(
                err_code(&m),
                (Code::InvalidSysctlSpec, field.into()),
                "{line}"
            );
        }
    }

    #[test]
    fn logging() {
        let m = format!(
            "{VALID}\n[logging]\ndriver = \"awslogs\"\noptions = {{ awslogs-group = \"/valkv/dev\", awslogs-stream = \"${{service.id}}-${{node.id}}\", mode = \"non-blocking\" }}\n"
        );
        let p = parse_manifest_str(&m).unwrap();
        let l = p.spec.logging.clone().unwrap();
        assert_eq!(l.driver, "awslogs");
        assert_eq!(l.options["awslogs-stream"], "${service.id}-${node.id}");
        assert!(p.spec.required_features().contains(&"logging"));
        assert!(!p
            .warnings
            .iter()
            .any(|w| w.field_path.starts_with("logging")));

        // A network driver without non-blocking mode: accepted, with a warning.
        let blocking = format!("{VALID}\n[logging]\ndriver = \"awslogs\"\n");
        let p = parse_manifest_str(&blocking).unwrap();
        assert!(p
            .warnings
            .iter()
            .any(|w| w.field_path == "logging.options.mode"));
        let local = format!(
            "{VALID}\n[logging]\ndriver = \"json-file\"\n[logging.options]\nmax-size = \"10m\"\n"
        );
        assert!(parse_manifest_str(&local)
            .unwrap()
            .warnings
            .iter()
            .all(|w| !w.field_path.starts_with("logging")));

        for (log, field) in [
            ("driver = \"Aws Logs\"", "logging.driver"),
            (
                "driver = \"none\"\noptions = { max-size = \"1m\" }",
                "logging.options",
            ),
            (
                "driver = \"awslogs\"\noptions = { awslogs-stream = \"${node.instance_id}\" }",
                "logging.options.awslogs-stream",
            ),
            (
                "driver = \"awslogs\"\noptions = { awslogs-stream = \"${service.name}\" }",
                "logging.options.awslogs-stream",
            ),
            (
                "driver = \"splunk\"\noptions = { splunk-token = \"${secret:T}\" }",
                "logging.options.splunk-token",
            ),
        ] {
            let m = format!("{VALID}\n[logging]\n{log}\n");
            assert_eq!(
                err_code(&m),
                (Code::InvalidLoggingSpec, field.into()),
                "{log}"
            );
        }
        let no_driver = format!("{VALID}\n[logging]\noptions = {{ mode = \"non-blocking\" }}\n");
        assert_eq!(err_code(&no_driver).0, Code::UnknownField);
    }
}
