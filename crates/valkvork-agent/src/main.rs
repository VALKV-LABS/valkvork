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
    let node = NodeInfo {
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
    let supervisor = Supervisor::new(
        rt,
        SupervisorConfig {
            data_dir: cfg.data_dir.clone(),
            registry_auth_file: cfg.registry_auth_file.clone(),
            docker_socket: cfg.docker_socket.clone(),
            protected_paths,
            host_volumes: cfg.host_volumes.clone(),
        },
    );
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
