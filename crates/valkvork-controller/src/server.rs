//! Wires the core, the listeners (agent gRPC, admin gRPC + web UI, metrics)
//! and TLS together.

use crate::core::Core;
use crate::db::{Db, EventReader};
use crate::grpc::{admin_auth, AdminApi, AgentApi};
use crate::http::{serve_metrics, UiLayer};
use crate::ui::UiState;
use std::sync::Arc;
use tonic::transport::{Certificate, Server, ServerTlsConfig};
use valkvork_common::config::{ControllerConfig, NodeCertificateMode};
use valkvork_common::tls::server_tls_config;
use valkvork_common::ValkvorkError;
use valkvork_proto::v1::admin_service_server::AdminServiceServer;
use valkvork_proto::v1::agent_service_server::AgentServiceServer;

pub async fn run(cfg: ControllerConfig) -> Result<(), ValkvorkError> {
    let join_token = cfg.resolve_join_token()?;
    let admin_token = cfg.resolve_admin_token()?;
    if admin_token.len() < 32 {
        tracing::warn!("admin token is shorter than 32 characters; FR-SEC-03 requires at least 32 random bytes");
    }

    let db = Db::open(&cfg.database_path)?;
    let reader = Arc::new(EventReader::open(&cfg.database_path)?);
    let mut core = Core::new(cfg.clone(), db, join_token)?;

    let tls = match (&cfg.tls_cert_file, &cfg.tls_key_file) {
        (Some(cert), Some(key)) => Some(server_tls_config(cert, key)?),
        (None, None) => {
            tracing::warn!(
                "tls_cert_file/tls_key_file not set: serving PLAINTEXT gRPC (development only)"
            );
            None
        }
        _ => {
            return Err(ValkvorkError::Config(
                "set both tls_cert_file and tls_key_file, or neither".into(),
            ))
        }
    };
    // FR-SEC-02: the agent listener also asks for a client certificate from
    // the node CA. It is optional at the TLS layer, so a new node can enrol
    // with the join token; the core decides what each Register needs.
    let mode = cfg.node_certificates.mode;
    let agent_tls = match (&tls, mode) {
        (None, NodeCertificateMode::Require) => {
            return Err(ValkvorkError::Config(
                "node_certificates.mode = \"require\" needs tls_cert_file and tls_key_file".into(),
            ))
        }
        (Some(t), NodeCertificateMode::Issue | NodeCertificateMode::Require) => {
            let ca = core.enable_node_certificates()?;
            tracing::info!(mode = ?mode, "node client certificates enabled");
            Some(
                t.clone()
                    .client_ca_root(Certificate::from_pem(ca))
                    .client_auth_optional(true),
            )
        }
        (t, _) => t.clone(),
    };
    // Block volumes: the driver, and a manager that runs its calls outside
    // the core (docs/block-volumes.md).
    let volume_driver: Option<std::sync::Arc<dyn crate::volumes::VolumeDriver>> =
        match cfg.block_volumes.driver.as_str() {
            "" => None,
            "local" => {
                let root = cfg.block_volumes.local_root.clone().ok_or_else(|| {
                    ValkvorkError::Config(
                        "block_volumes.driver = \"local\" needs local_root".into(),
                    )
                })?;
                let d = crate::volumes::LocalDriver::new(root)
                    .map_err(|e| ValkvorkError::Config(format!("block_volumes.local_root: {e}")))?;
                tracing::warn!("block volumes use the local driver (development and tests only)");
                Some(std::sync::Arc::new(d))
            }
            other => {
                return Err(ValkvorkError::Config(format!(
                    "block_volumes.driver {other:?}: use \"local\" (aws-ebs comes in stage 3)"
                )))
            }
        };
    let volume_rx = volume_driver.as_ref().map(|d| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        core.enable_volumes(d.clone(), tx);
        rx
    });
    let (handle, core_task) = core.spawn();
    if let (Some(d), Some(rx)) = (volume_driver, volume_rx) {
        crate::volumes::spawn_manager(d, rx, handle.clone());
    }

    let builder = |tls: &Option<ServerTlsConfig>| -> Result<Server, ValkvorkError> {
        let b = Server::builder()
            .http2_keepalive_interval(Some(std::time::Duration::from_secs(10)))
            .http2_keepalive_timeout(Some(std::time::Duration::from_secs(5)));
        match tls {
            Some(t) => b
                .tls_config(t.clone())
                .map_err(|e| ValkvorkError::Tls(e.to_string())),
            None => Ok(b),
        }
    };

    let agent_srv = builder(&agent_tls)?
        .add_service(AgentServiceServer::new(AgentApi::new(handle.clone())))
        .serve(cfg.agent_listen_addr);
    // The UI shares the admin listener (FR-UI-02). Browsers speak HTTP/1.1
    // over plaintext, so HTTP/1 is accepted next to gRPC's HTTP/2.
    let ui = cfg.ui.enabled.then(|| {
        UiState::new(
            handle.clone(),
            reader.clone(),
            admin_token.clone(),
            std::time::Duration::from_secs(cfg.ui.session_ttl_seconds),
            tls.is_some(),
            cfg.database_path.clone(),
        )
    });
    let admin_srv = builder(&tls)?
        .accept_http1(true)
        .layer(UiLayer::new(ui))
        .add_service(AdminServiceServer::with_interceptor(
            AdminApi::new(handle.clone(), reader),
            admin_auth(admin_token),
        ))
        .serve(cfg.admin_listen_addr);

    let metrics = {
        let (addr, handle, db) = (
            cfg.metrics_listen_addr,
            handle.clone(),
            cfg.database_path.clone(),
        );
        async move {
            match addr {
                Some(addr) => serve_metrics(addr, handle, db).await,
                None => std::future::pending().await,
            }
        }
    };

    tracing::info!(
        agent = %cfg.agent_listen_addr,
        admin = %cfg.admin_listen_addr,
        ui = cfg.ui.enabled,
        metrics = ?cfg.metrics_listen_addr,
        "controller listening"
    );
    let net = |e: tonic::transport::Error| ValkvorkError::Network(e.to_string());
    tokio::select! {
        r = agent_srv => r.map_err(net),
        r = admin_srv => r.map_err(net),
        r = metrics => r.map_err(|e| ValkvorkError::Network(format!("metrics listener: {e}"))),
        r = core_task => match r {
            Ok(r) => r.and(Err(ValkvorkError::Internal("controller core stopped".into()))),
            Err(e) => Err(ValkvorkError::Internal(format!("controller core panicked: {e}"))),
        },
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
