//! `valkvork` — operator CLI for the Admin API (FR-CON-10).
//!
//! Exit codes: 0 success, 1 server or transport error, 2 validation error.

use clap::{Parser, Subcommand};
use pb::admin_service_client::AdminServiceClient;
use serde::Serialize;
use std::path::PathBuf;
use std::process::ExitCode;
use tonic::codegen::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Channel, Endpoint};
use valkvork_common::models::ChangeKind;
use valkvork_common::tls::client_tls_config;
use valkvork_common::validation::parse_manifest_str;
use valkvork_proto::v1 as pb;

#[derive(Parser)]
#[command(
    name = "valkvork",
    version,
    about = "Valkvork workload orchestrator CLI"
)]
struct Cli {
    /// Controller Admin API endpoint
    #[arg(
        long,
        global = true,
        env = "VALKVORK_ADDR",
        default_value = "https://controlplane.valkvork.internal:50052"
    )]
    endpoint: String,
    /// Admin bearer token
    #[arg(
        long,
        global = true,
        env = "VALKVORK_ADMIN_TOKEN",
        hide_env_values = true
    )]
    token: Option<String>,
    /// CA bundle used to verify the controller certificate
    #[arg(long, global = true, env = "VALKVORK_CA_FILE")]
    ca_file: Option<PathBuf>,
    /// Output format
    #[arg(long, short, global = true, default_value = "table", value_parser = ["table", "json"])]
    output: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or update a service from a manifest
    Apply {
        #[arg(short, long)]
        file: PathBuf,
    },
    /// Show what `apply` would change, and whether it can be placed
    Plan {
        #[arg(short, long)]
        file: PathBuf,
    },
    /// Delete a service
    Delete {
        #[command(subcommand)]
        what: ServiceRef,
    },
    /// Set a service's replica count
    Scale {
        #[command(subcommand)]
        what: ScaleRef,
    },
    /// Cluster overview, or one service in detail
    Status { service: Option<String> },
    /// List workloads
    Workloads {
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        status: Option<String>,
    },
    /// Cluster events, newest first
    Events {
        /// NODE, WORKLOAD, DEPLOY, SCALE or AUDIT
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    /// Node operations
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
}

#[derive(Subcommand)]
enum ServiceRef {
    Service { id: String },
}

#[derive(Subcommand)]
enum ScaleRef {
    Service { id: String, replicas: u32 },
}

#[derive(Subcommand)]
enum NodeCmd {
    List {
        #[arg(long)]
        class: Option<String>,
    },
    Drain {
        node_id: String,
    },
    Undrain {
        node_id: String,
    },
}

/// Failure classes mapped to exit codes.
enum Failure {
    Validation(String),
    Other(String),
}

impl From<tonic::Status> for Failure {
    fn from(s: tonic::Status) -> Self {
        match s.code() {
            tonic::Code::InvalidArgument => Failure::Validation(s.message().to_string()),
            tonic::Code::Unavailable => {
                Failure::Other(format!("controller unavailable: {}", s.message()))
            }
            _ => Failure::Other(format!("{:?}: {}", s.code(), s.message())),
        }
    }
}

#[derive(Clone)]
struct Auth(Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>);

impl Interceptor for Auth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(v) = &self.0 {
            req.metadata_mut().insert("authorization", v.clone());
        }
        Ok(req)
    }
}

type Client = AdminServiceClient<InterceptedService<Channel, Auth>>;

async fn connect(cli: &Cli) -> Result<Client, Failure> {
    let mut ep = Endpoint::from_shared(cli.endpoint.clone())
        .map_err(|e| Failure::Other(format!("bad endpoint: {e}")))?;
    ep = ep.connect_timeout(std::time::Duration::from_secs(5));
    if cli.endpoint.starts_with("https://") {
        let tls = client_tls_config(cli.ca_file.as_deref(), None)
            .map_err(|e| Failure::Other(e.to_string()))?;
        ep = ep
            .tls_config(tls)
            .map_err(|e| Failure::Other(e.to_string()))?;
    }
    let channel = ep
        .connect()
        .await
        .map_err(|e| Failure::Other(format!("cannot reach {}: {e}", cli.endpoint)))?;
    let token = match &cli.token {
        Some(t) => Some(
            format!("Bearer {t}")
                .parse()
                .map_err(|_| Failure::Other("token contains invalid characters".into()))?,
        ),
        None => None,
    };
    Ok(AdminServiceClient::with_interceptor(channel, Auth(token)))
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Validation(m)) => {
            eprintln!("validation failed: {m}");
            ExitCode::from(2)
        }
        Err(Failure::Other(m)) => {
            eprintln!("error: {m}");
            ExitCode::from(1)
        }
    }
}

fn read_manifest(path: &PathBuf) -> Result<String, Failure> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Failure::Other(format!("cannot read {}: {e}", path.display())))?;
    // Validate locally first for fast, precise feedback; the server re-validates.
    if let Err(errors) = parse_manifest_str(&content) {
        let lines: Vec<String> = errors.iter().map(|e| format!("\n  {e}")).collect();
        return Err(Failure::Validation(lines.concat()));
    }
    Ok(content)
}

fn json<T: Serialize>(v: &T) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

async fn run(cli: &Cli) -> Result<(), Failure> {
    let as_json = cli.output == "json";
    // Validate manifests before connecting, so exit code 2 needs no controller.
    let manifest = match &cli.command {
        Command::Apply { file } | Command::Plan { file } => Some(read_manifest(file)?),
        _ => None,
    };
    let mut c = connect(cli).await?;
    match &cli.command {
        Command::Apply { .. } => {
            let r = c
                .apply(pb::ApplyRequest {
                    manifest_toml: manifest.unwrap(),
                })
                .await?
                .into_inner();
            if as_json {
                {
                    json(&r);
                    return Ok(());
                }
            }
            println!(
                "service {:?}: {} (version {})",
                r.service_id, r.change, r.version
            );
            for w in &r.warnings {
                println!("  warning: {w}");
            }
        }
        Command::Plan { .. } => {
            let r = c
                .plan(pb::ApplyRequest {
                    manifest_toml: manifest.unwrap(),
                })
                .await?
                .into_inner();
            if as_json {
                {
                    json(&r);
                    return Ok(());
                }
            }
            print_plan(&r);
        }
        Command::Delete {
            what: ServiceRef::Service { id },
        } => {
            c.delete_service(pb::DeleteServiceRequest {
                service_id: id.clone(),
            })
            .await?;
            println!("service {id:?} is being deleted; workloads are stopping");
        }
        Command::Scale {
            what: ScaleRef::Service { id, replicas },
        } => {
            let r = c
                .scale_service(pb::ScaleServiceRequest {
                    service_id: id.clone(),
                    replicas: *replicas,
                })
                .await?
                .into_inner();
            if as_json {
                {
                    json(&r);
                    return Ok(());
                }
            }
            println!(
                "service {id:?}: replicas {} → {}",
                r.old_replicas, r.new_replicas
            );
            if r.new_replicas != *replicas {
                println!("  note: clamped to the autoscale range");
            }
        }
        Command::Status { service: Some(id) } => {
            let s = c
                .get_service(pb::GetServiceRequest {
                    service_id: id.clone(),
                })
                .await?
                .into_inner();
            if as_json {
                {
                    json(&s);
                    return Ok(());
                }
            }
            print_service(&s);
        }
        Command::Status { service: None } => {
            let services = c
                .list_services(pb::ListServicesRequest::default())
                .await?
                .into_inner()
                .services;
            let nodes = c
                .list_nodes(pb::ListNodesRequest::default())
                .await?
                .into_inner()
                .nodes;
            if as_json {
                #[derive(Serialize)]
                struct Overview<'a> {
                    services: &'a [pb::ServiceSummary],
                    nodes: &'a [pb::NodeSummary],
                }
                {
                    json(&Overview {
                        services: &services,
                        nodes: &nodes,
                    });
                    return Ok(());
                }
            }
            let healthy = nodes.iter().filter(|n| n.status == "HEALTHY").count();
            let draining = nodes.iter().filter(|n| n.draining).count();
            println!(
                "nodes: {} total, {healthy} healthy, {} unhealthy, {draining} draining\n",
                nodes.len(),
                nodes.len() - healthy
            );
            table(
                &[
                    "SERVICE",
                    "VERSION",
                    "RUNNING/TARGET",
                    "CPU",
                    "STATUS",
                    "IMAGE",
                ],
                services
                    .iter()
                    .map(|s| {
                        vec![
                            s.service_id.clone(),
                            s.version.to_string(),
                            format!("{}/{}", s.running_replicas, s.target_replicas),
                            format!("{:.0}%", s.cpu_utilization),
                            s.status.clone(),
                            s.image.clone(),
                        ]
                    })
                    .collect(),
            );
        }
        Command::Workloads {
            service,
            node,
            status,
        } => {
            let req = pb::ListWorkloadsRequest {
                service_id: service.clone().unwrap_or_default(),
                node_id: node.clone().unwrap_or_default(),
                status: status.clone().unwrap_or_default(),
            };
            let ws = c.list_workloads(req).await?.into_inner().workloads;
            if as_json {
                {
                    json(&ws);
                    return Ok(());
                }
            }
            print_workloads(&ws, true);
        }
        Command::Events { kind, limit } => {
            let req = pb::ListEventsRequest {
                kind: kind.clone().unwrap_or_default(),
                since: 0,
                limit: *limit,
            };
            let events = c.list_events(req).await?.into_inner().events;
            if as_json {
                {
                    json(&events);
                    return Ok(());
                }
            }
            table(
                &["TIME", "KIND", "ACTOR", "MESSAGE"],
                events
                    .iter()
                    .map(|e| {
                        vec![
                            time(e.created_at),
                            e.kind.clone(),
                            e.actor.clone(),
                            e.message.clone(),
                        ]
                    })
                    .collect(),
            );
        }
        Command::Node {
            cmd: NodeCmd::List { class },
        } => {
            let req = pb::ListNodesRequest {
                node_id: String::new(),
                node_class: class.clone().unwrap_or_default(),
            };
            let nodes = c.list_nodes(req).await?.into_inner().nodes;
            if as_json {
                {
                    json(&nodes);
                    return Ok(());
                }
            }
            let now = chrono::Utc::now().timestamp_millis() as u64;
            table(
                &[
                    "NODE",
                    "HOSTNAME",
                    "CLASS",
                    "STATUS",
                    "CPU ALLOC",
                    "MEM ALLOC",
                    "WORKLOADS",
                    "HEARTBEAT",
                    "RUNTIME",
                ],
                nodes
                    .iter()
                    .map(|n| {
                        let status = if n.draining {
                            format!("{} (draining)", n.status)
                        } else {
                            n.status.clone()
                        };
                        vec![
                            n.node_id.clone(),
                            n.hostname.clone(),
                            n.node_class.clone(),
                            status,
                            format!(
                                "{}/{}m",
                                n.used_cpu_millicores, n.allocatable_cpu_millicores
                            ),
                            format!(
                                "{}/{}",
                                mib(n.used_memory_bytes),
                                mib(n.allocatable_memory_bytes)
                            ),
                            n.workload_count.to_string(),
                            format!("{}s ago", now.saturating_sub(n.last_heartbeat_at) / 1000),
                            n.runtime.clone(),
                        ]
                    })
                    .collect(),
            );
        }
        Command::Node {
            cmd: NodeCmd::Drain { node_id },
        } => {
            c.drain_node(pb::DrainNodeRequest {
                node_id: node_id.clone(),
            })
            .await?;
            println!("node {node_id} draining: workloads migrate start-new-then-stop-old");
        }
        Command::Node {
            cmd: NodeCmd::Undrain { node_id },
        } => {
            c.undrain_node(pb::UndrainNodeRequest {
                node_id: node_id.clone(),
            })
            .await?;
            println!("node {node_id} accepts placements again");
        }
    }
    Ok(())
}

fn print_plan(r: &pb::PlanResponse) {
    let change: ChangeKind = r.change.parse().unwrap_or(ChangeKind::Unchanged);
    let (sigil, header) = match change {
        ChangeKind::Created => ("+", format!("version {}", r.version)),
        ChangeKind::Unchanged => ("=", "unchanged".to_string()),
        ChangeKind::Replacing => (
            "~",
            format!(
                "version {} → {}   [rolling update, max_parallel={}]",
                r.version.saturating_sub(1),
                r.version,
                r.max_parallel
            ),
        ),
        ChangeKind::ScaleOnly => ("~", format!("version {}   [scale-only]", r.version)),
        ChangeKind::PolicyOnly => ("~", format!("version {}   [policy-only]", r.version)),
    };
    println!("{sigil} service {:?}   {header}", r.service_id);
    for d in &r.diff {
        println!("    {d}");
    }
    if !r.diff.iter().any(|d| d.starts_with("replicas")) {
        println!("    replicas: {} (unchanged)", r.target_replicas);
    }
    if r.to_place > 0 {
        println!(
            "  placement: {}/{} workloads placeable",
            r.placeable, r.to_place
        );
    }
    if !r.reason.is_empty() {
        println!("  ! {}", r.reason);
    }
    for w in &r.warnings {
        println!("  warning: {w}");
    }
}

fn print_service(s: &pb::ServiceDetail) {
    println!("service:   {}", s.service_id);
    println!("version:   {}", s.version);
    println!("status:    {} (rollout {})", s.status, s.rollout_status);
    println!(
        "replicas:  {} running / {} target ({} pending, {} failed)",
        s.running_replicas, s.target_replicas, s.pending_replicas, s.failed_replicas
    );
    println!("\nworkloads:");
    print_workloads(&s.workloads, false);
    if !s.scaling_events.is_empty() {
        println!("\nscaling events:");
        for e in &s.scaling_events {
            println!(
                "  {}  {} → {}  {}",
                time(e.created_at),
                e.from_replicas,
                e.to_replicas,
                e.reason
            );
        }
    }
    println!("\nspec:\n{}", s.spec_toml);
}

fn print_workloads(ws: &[pb::WorkloadSummary], with_service: bool) {
    let mut headers = vec!["WORKLOAD"];
    if with_service {
        headers.push("SERVICE");
    }
    headers.extend(["VER", "NODE", "STATUS", "RESTARTS", "AGE", "REASON"]);
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let rows = ws
        .iter()
        .map(|w| {
            let mut r = vec![w.workload_id.clone()];
            if with_service {
                r.push(w.service_id.clone());
            }
            r.extend([
                w.service_version.to_string(),
                if w.node_id.is_empty() {
                    "-".into()
                } else {
                    short(&w.node_id)
                },
                w.status.clone(),
                w.restart_count.to_string(),
                age(now.saturating_sub(w.created_at)),
                w.status_reason.clone(),
            ]);
            r
        })
        .collect();
    table(&headers, rows);
}

fn table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let s: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(headers.to_vec());
    for r in &rows {
        line(r.iter().map(String::as_str).collect());
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn mib(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{:.1}GiB", b as f64 / (1u64 << 30) as f64)
    } else {
        format!("{}MiB", b >> 20)
    }
}

fn age(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn time(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}
