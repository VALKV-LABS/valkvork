//! `valkvork-agent --config /etc/valkvork/agent.toml`

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use valkvork_agent::connection::{self, Backoff, NodeInfo};
use valkvork_agent::identity;
use valkvork_agent::runtime::{ContainerRuntime, DockerRuntime};
use valkvork_agent::supervisor::{Supervisor, SupervisorConfig};
use valkvork_common::config::AgentConfig;
use valkvork_common::units::parse_size;
use valkvork_common::ValkvorkError;

#[derive(Parser)]
#[command(name = "valkvork-agent", version, about = "Valkvork worker agent")]
struct Args {
    /// Path to agent.toml
    #[arg(
        long,
        env = "VALKVORK_AGENT_CONFIG",
        default_value = "/etc/valkvork/agent.toml"
    )]
    config: PathBuf,
    /// Validate the configuration (including the join token file) and exit
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() {
    valkvork_common::init_tracing();
    let args = Args::parse();
    if let Err(e) = run(args).await {
        tracing::error!(error = %e, "agent exiting");
        eprintln!("valkvork-agent: {e}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), ValkvorkError> {
    let cfg = AgentConfig::load(&args.config)?;
    let join_token = std::fs::read_to_string(&cfg.join_token_file)
        .map_err(|e| {
            ValkvorkError::Config(format!(
                "cannot read join token {}: {e}",
                cfg.join_token_file.display()
            ))
        })?
        .trim()
        .to_string();
    if args.check_config {
        if join_token.is_empty() {
            return Err(ValkvorkError::Config("join token file is empty".into()));
        }
        if let Some(ca) = &cfg.ca_file {
            valkvork_common::tls::client_tls_config(Some(ca), None)?;
        }
        println!("configuration OK");
        return Ok(());
    }
    let node_id = identity::load_or_create_node_id(&cfg.data_dir)?;
    let rt: Arc<dyn ContainerRuntime> = Arc::new(DockerRuntime::connect(&cfg.docker_socket)?);

    // Wait for the runtime (dockerd may still be starting at boot).
    let mut backoff = Backoff::default();
    let runtime = loop {
        match rt.version().await {
            Ok(v) => break v,
            Err(e) => {
                let d = backoff.next_delay();
                tracing::warn!(error = %e, "container runtime unavailable; retrying in {d:?}");
                tokio::time::sleep(d).await;
            }
        }
    };
    let (total_cpu, total_mem) = rt.host_resources().await?;
    let reserved_cpu = (cfg.reserved.cpu * 1000.0).ceil() as u32;
    let reserved_mem = parse_size(&cfg.reserved.memory)
        .map_err(|e| ValkvorkError::Config(e.to_string()))?
        .as_u64();
    let (zone, instance_id) =
        identity::topology(cfg.zone.as_deref(), cfg.instance_id.as_deref()).await;
    let node = NodeInfo {
        zone,
        instance_id,
        max_block_volumes: cfg.block_volumes.max,
        node_id: node_id.clone(),
        hostname: identity::hostname(),
        ip_address: cfg
            .advertise_ip
            .clone()
            .or_else(|| identity::local_ip_towards(&cfg.controller_endpoint))
            .unwrap_or_default(),
        allocatable_cpu_millicores: total_cpu.saturating_sub(reserved_cpu).max(1),
        allocatable_memory_bytes: total_mem.saturating_sub(reserved_mem).max(1),
        runtime: runtime.clone(),
    };
    tracing::info!(
        node_id = %node.node_id,
        hostname = %node.hostname,
        ip = %node.ip_address,
        class = %cfg.node_class,
        %runtime,
        cpu_millicores = node.allocatable_cpu_millicores,
        memory_bytes = node.allocatable_memory_bytes,
        "starting agent"
    );

    let protected_paths = rt.data_root().await.into_iter().collect();
    let endpoints = valkvork_agent::discovery::Endpoints::load(&cfg.data_dir);
    let dns = start_discovery(&cfg, &rt, &endpoints).await?;
    let secret_files = Arc::new(
        valkvork_agent::secret_files::SecretFiles::prepare(&cfg.data_dir, cfg.secrets.files_tmpfs)
            .map_err(ValkvorkError::Config)?,
    );
    if !secret_files.in_memory {
        tracing::warn!(
            "secrets.files_tmpfs is off: file secrets are kept on disk (development only)"
        );
    }
    let metadata = if cfg.instance_metadata.block {
        let im = &cfg.instance_metadata;
        let g = valkvork_agent::metadata::MetadataGuard::install(&im.iptables, &im.bridge)
            .await
            .map_err(ValkvorkError::Config)?;
        tracing::info!(bridge = %im.bridge, "containers are blocked from the instance metadata service unless their manifest allows it");
        Some(Arc::new(g))
    } else {
        tracing::warn!(
            "[instance_metadata] block is off: every container can use the node's cloud role"
        );
        None
    };
    let supervisor = Supervisor::new(
        rt,
        SupervisorConfig {
            data_dir: cfg.data_dir.clone(),
            registry_auth_file: cfg.registry_auth_file.clone(),
            docker_socket: cfg.docker_socket.clone(),
            protected_paths,
            host_volumes: cfg.host_volumes.clone(),
            advertise_ip: node.ip_address.clone(),
            // "0.0.0.0" keeps Docker's own default: every address.
            default_host_ip: match cfg.default_host_ip.as_str() {
                "0.0.0.0" => String::new(),
                other => other.to_string(),
            },
            metadata,
            default_pids_limit: cfg.default_pids_limit,
            secret_files: Some(secret_files),
            node_id: node.node_id.clone(),
            node_class: cfg.node_class.clone(),
            hostname: node.hostname.clone(),
            secrets: Arc::new(valkvork_agent::secrets::SecretResolver::new(
                cfg.secrets.clone(),
            )),
            endpoints,
            dns,
        },
    );
    supervisor
        .prepare_host_volumes()
        .map_err(ValkvorkError::Config)?;
    // FR-AGE-07: adopt existing containers before the first Register.
    loop {
        match supervisor.adopt().await {
            Ok(n) => {
                tracing::info!(containers = n, "adopted existing containers");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "adoption failed; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    tokio::spawn(supervisor.clone().run_events());
    tokio::spawn(supervisor.clone().run_resync());
    tokio::spawn(supervisor.clone().run_disk_checks());
    tokio::spawn(supervisor.clone().run_health_checks());
    tokio::spawn(supervisor.clone().run_secret_refresh());
    if let Some(addr) = cfg.metrics_listen_addr {
        tokio::spawn(valkvork_agent::metrics::serve(addr, supervisor.clone()));
    }

    tokio::select! {
        _ = connection::run(cfg, join_token, node, supervisor) => Ok(()),
        _ = tokio::signal::ctrl_c() => {
            // Exiting leaves every container running (AT-04).
            tracing::info!("shutting down; containers keep running");
            Ok(())
        }
    }
}

/// With `[discovery] enabled`, serve DNS for services and return the
/// nameserver containers should use. A failure to listen is fatal: a node
/// whose containers expect the names but cannot resolve them is worse than
/// an agent that does not start, which systemd retries and the journal shows.
async fn start_discovery(
    cfg: &AgentConfig,
    rt: &Arc<dyn ContainerRuntime>,
    endpoints: &Arc<valkvork_agent::discovery::Endpoints>,
) -> Result<Vec<String>, ValkvorkError> {
    use valkvork_agent::discovery::{resolv_conf_nameservers, DnsServer};
    let d = &cfg.discovery;
    if !d.enabled {
        return Ok(Vec::new());
    }
    let ip = match d.listen_ip {
        Some(ip) => ip,
        None => rt.bridge_gateway().await.ok_or_else(|| {
            ValkvorkError::Config(
                "discovery: no Docker bridge gateway found; set discovery.listen_ip".into(),
            )
        })?,
    };
    let upstream = if d.upstream.is_empty() {
        let text = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
        resolv_conf_nameservers(&text, ip)
    } else {
        d.upstream.clone()
    };
    if upstream.is_empty() {
        tracing::warn!("discovery: no upstream nameservers; only service names will resolve");
    }
    DnsServer::new(endpoints.clone(), &d.domain, upstream, d.ttl_seconds)
        .bind_and_serve(ip)
        .await
        .map_err(|e| ValkvorkError::Config(format!("discovery: listen on {ip}:53: {e}")))?;
    Ok(vec![ip.to_string()])
}
