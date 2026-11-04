//! Wires the core, the two gRPC listeners and TLS together.

use crate::core::Core;
use crate::db::{Db, EventReader};
use crate::grpc::{admin_auth, AdminApi, AgentApi};
use std::sync::Arc;
use tonic::transport::Server;
use valkvork_common::config::ControllerConfig;
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
    let core = Core::new(cfg.clone(), db, join_token)?;
    let (handle, core_task) = core.spawn();

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
    let builder = || -> Result<Server, ValkvorkError> {
        let b = Server::builder()
            .http2_keepalive_interval(Some(std::time::Duration::from_secs(10)))
            .http2_keepalive_timeout(Some(std::time::Duration::from_secs(5)));
        match &tls {
            Some(t) => b
                .tls_config(t.clone())
                .map_err(|e| ValkvorkError::Tls(e.to_string())),
            None => Ok(b),
        }
    };

    let agent_srv = builder()?
        .add_service(AgentServiceServer::new(AgentApi::new(handle.clone())))
        .serve(cfg.agent_listen_addr);
    let admin_srv = builder()?
        .add_service(AdminServiceServer::with_interceptor(
            AdminApi::new(handle, reader),
            admin_auth(admin_token),
        ))
        .serve(cfg.admin_listen_addr);

    tracing::info!(agent = %cfg.agent_listen_addr, admin = %cfg.admin_listen_addr, "controller listening");
    let net = |e: tonic::transport::Error| ValkvorkError::Network(e.to_string());
    tokio::select! {
        r = agent_srv => r.map_err(net),
        r = admin_srv => r.map_err(net),
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
