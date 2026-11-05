//! The agent's single outbound stream to the controller (FR-ARC-01).
//!
//! Each session: connect → `Register` with the full inventory → wait for
//! `RegisterAck` → heartbeat every interval while executing commands. When
//! the stream breaks the agent only reconnects, with exponential backoff; it
//! never touches its containers because of a lost controller (FR-AGE-05).

use crate::supervisor::Supervisor;
use crate::telemetry::{node_used_memory, NodeCpu};
use pb::agent_service_client::AgentServiceClient;
use rand::Rng;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tonic::transport::Endpoint;
use valkvork_common::config::AgentConfig;
use valkvork_common::tls::client_tls_config;
use valkvork_common::ValkvorkError;
use valkvork_proto::v1 as pb;

pub struct NodeInfo {
    pub node_id: String,
    pub hostname: String,
    pub ip_address: String,
    pub allocatable_cpu_millicores: u32,
    pub allocatable_memory_bytes: u64,
    pub runtime: String,
}

/// 1 s, doubling, capped at 15 s, ±20 % jitter (FR-AGE-05).
#[derive(Debug, Default)]
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub const BASE: Duration = Duration::from_secs(1);
    pub const CAP: Duration = Duration::from_secs(15);

    pub fn next_delay(&mut self) -> Duration {
        let base = Self::BASE
            .saturating_mul(1u32 << self.attempt.min(10))
            .min(Self::CAP);
        self.attempt = self.attempt.saturating_add(1);
        let jitter = rand::thread_rng().gen_range(-0.2..=0.2);
        base.mul_f64(1.0 + jitter)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

pub async fn run(
    cfg: AgentConfig,
    join_token: String,
    node: NodeInfo,
    supervisor: Arc<Supervisor>,
) {
    let mut backoff = Backoff::default();
    loop {
        match session(&cfg, &join_token, &node, &supervisor, &mut backoff).await {
            Ok(()) => tracing::warn!("controller closed the stream"),
            Err(e) => tracing::warn!(error = %e, "controller stream failed"),
        }
        crate::metrics::CONNECTED.store(false, std::sync::atomic::Ordering::Relaxed);
        crate::metrics::RECONNECTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let delay = backoff.next_delay();
        tracing::info!(
            delay_ms = delay.as_millis() as u64,
            "reconnecting (containers untouched)"
        );
        tokio::time::sleep(delay).await;
    }
}

fn net(e: impl std::fmt::Display) -> ValkvorkError {
    ValkvorkError::Network(e.to_string())
}

async fn session(
    cfg: &AgentConfig,
    join_token: &str,
    node: &NodeInfo,
    supervisor: &Arc<Supervisor>,
    backoff: &mut Backoff,
) -> Result<(), ValkvorkError> {
    let mut endpoint = Endpoint::from_shared(cfg.controller_endpoint.clone())
        .map_err(net)?
        .connect_timeout(Duration::from_secs(5))
        .http2_keep_alive_interval(Duration::from_secs(10))
        .keep_alive_timeout(Duration::from_secs(5))
        .keep_alive_while_idle(true);
    if cfg.controller_endpoint.starts_with("https://") {
        let tls = client_tls_config(cfg.ca_file.as_deref(), cfg.tls_domain.as_deref())?;
        endpoint = endpoint.tls_config(tls).map_err(net)?;
    } else {
        tracing::warn!(
            "controller_endpoint is not https: stream is unencrypted (development only)"
        );
    }
    let channel = endpoint.connect().await.map_err(net)?;

    let (tx, rx) = mpsc::channel::<pb::AgentMessage>(256);
    let register = pb::Register {
        node_id: node.node_id.clone(),
        join_token: join_token.to_string(),
        hostname: node.hostname.clone(),
        ip_address: node.ip_address.clone(),
        node_class: cfg.node_class.clone(),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        allocatable_cpu_millicores: node.allocatable_cpu_millicores,
        allocatable_memory_bytes: node.allocatable_memory_bytes,
        host_volumes: cfg.host_volumes.keys().cloned().collect(),
        inventory: supervisor.report(),
        runtime: node.runtime.clone(),
    };
    let inventory = register.inventory.len();
    tx.send(pb::AgentMessage {
        msg: Some(pb::agent_message::Msg::Register(register)),
    })
    .await
    .map_err(net)?;

    let mut client = AgentServiceClient::new(channel);
    let mut inbound = client
        .connect_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(net)?
        .into_inner();

    let first = tokio::time::timeout(Duration::from_secs(15), inbound.message())
        .await
        .map_err(|_| net("no RegisterAck within 15 s"))?
        .map_err(net)?;
    let interval_ms = match first.and_then(|m| m.msg) {
        Some(pb::controller_message::Msg::RegisterAck(ack)) if ack.accepted => {
            ack.heartbeat_interval_ms.max(500)
        }
        Some(pb::controller_message::Msg::RegisterAck(ack)) => {
            return Err(ValkvorkError::Config(format!(
                "registration rejected: {}",
                ack.reject_reason
            )));
        }
        _ => return Err(net("expected RegisterAck as the first message")),
    };
    backoff.reset();
    crate::metrics::CONNECTED.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(node = %node.node_id, inventory, interval_ms, "registered with controller");

    let heartbeat = {
        let tx = tx.clone();
        let supervisor = supervisor.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(interval_ms as u64));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut cpu = NodeCpu::default();
            let mut seq = 0u64;
            loop {
                tick.tick().await;
                seq += 1;
                let hb = pb::Heartbeat {
                    seq,
                    node_cpu_usage_percent: cpu.sample(),
                    node_used_memory_bytes: node_used_memory(),
                    workloads: supervisor.report(),
                };
                if tx
                    .send(pb::AgentMessage {
                        msg: Some(pb::agent_message::Msg::Heartbeat(hb)),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                crate::metrics::HEARTBEATS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
    };

    let result = loop {
        let msg = match inbound.message().await {
            Ok(Some(m)) => m,
            Ok(None) => break Ok(()),
            Err(e) => break Err(net(e)),
        };
        let is_start = matches!(msg.msg, Some(pb::controller_message::Msg::StartWorkload(_)));
        let (command_id, workload_id, outcome) = match msg.msg {
            Some(pb::controller_message::Msg::StartWorkload(s)) => {
                tracing::info!(workload = %s.workload_id, service = %s.service_id, image = %s.image, "StartWorkload");
                let ids = (s.command_id.clone(), s.workload_id.clone());
                (ids.0, ids.1, supervisor.start(s).await)
            }
            Some(pb::controller_message::Msg::StopWorkload(s)) => {
                tracing::info!(workload = %s.workload_id, "StopWorkload");
                let ids = (s.command_id.clone(), s.workload_id.clone());
                (ids.0, ids.1, supervisor.stop(s).await)
            }
            _ => continue,
        };
        crate::metrics::command(is_start, outcome.is_ok());
        let result = pb::CommandResult {
            command_id,
            workload_id,
            accepted: outcome.is_ok(),
            error: outcome.err().unwrap_or_default(),
        };
        if tx
            .send(pb::AgentMessage {
                msg: Some(pb::agent_message::Msg::CommandResult(result)),
            })
            .await
            .is_err()
        {
            break Ok(());
        }
    };
    heartbeat.abort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_caps_and_resets() {
        let mut b = Backoff::default();
        let delays: Vec<f64> = (0..7).map(|_| b.next_delay().as_secs_f64()).collect();
        for (d, base) in delays.iter().zip([1.0, 2.0, 4.0, 8.0, 15.0, 15.0, 15.0]) {
            assert!(
                *d >= base * 0.8 - 1e-9 && *d <= base * 1.2 + 1e-9,
                "{d} vs {base}"
            );
        }
        b.reset();
        assert!(b.next_delay() <= Duration::from_millis(1200));
    }
}
