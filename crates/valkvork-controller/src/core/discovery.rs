//! Service discovery: each service's reachable addresses, sent to every
//! agent with the `service_discovery` feature, which answers DNS for them on
//! its node.
//!
//! The set is recomputed after every step and sent, whole, only when it
//! changes, and to an agent after it registers. During the recovery grace
//! period nothing is sent: health is not reported yet, and an empty set would
//! make agents forget addresses that still work. Agents keep their last set
//! while the controller is away.

use super::Core;
use std::collections::BTreeMap;
use valkvork_common::models::{DesiredState, NodeStatus, WorkloadStatus};
use valkvork_proto::v1 as pb;

pub(crate) const FEATURE: &str = "service_discovery";

/// Service id → sorted, distinct node addresses.
pub(crate) type EndpointSet = BTreeMap<String, Vec<String>>;

#[derive(Debug, Default)]
pub(crate) struct DiscoveryState {
    /// The set last sent; None before the first computation.
    sent: Option<EndpointSet>,
    version: u64,
    /// Agents that registered since and still need the current set.
    pending: Vec<String>,
}

impl DiscoveryState {
    pub(crate) fn needs_set(&mut self, node_id: &str) {
        if !self.pending.iter().any(|n| n == node_id) {
            self.pending.push(node_id.to_string());
        }
    }
}

impl Core {
    /// The addresses a client should use for each service: the node address
    /// of every RUNNING workload whose desired state is RUN, on a HEALTHY
    /// node, and, if the service has a health check, reported HEALTHY. Every
    /// long-running service is listed, with no addresses when none qualify,
    /// so its name exists. Batch jobs are not listed.
    pub(crate) fn endpoint_set(&self) -> EndpointSet {
        let mut out: EndpointSet = self
            .services
            .values()
            .filter(|s| !s.spec.is_batch())
            .map(|s| (s.service_id.clone(), Vec::new()))
            .collect();
        for w in self.workloads.values() {
            if w.status != WorkloadStatus::Running || w.desired_state != DesiredState::Run {
                continue;
            }
            let Some(addrs) = out.get_mut(&w.service_id) else {
                continue;
            };
            let checked = self
                .services
                .get(&w.service_id)
                .is_some_and(|s| s.spec.check.is_some());
            if checked && w.health != crate::model::Health::Healthy {
                continue;
            }
            let Some(node) = w.node_id.as_ref().and_then(|n| self.nodes.get(n)) else {
                continue;
            };
            if node.node.status != NodeStatus::Healthy || node.node.ip_address.is_empty() {
                continue;
            }
            addrs.push(node.node.ip_address.clone());
        }
        for addrs in out.values_mut() {
            addrs.sort();
            addrs.dedup();
        }
        out
    }

    /// Send the set to agents that need it: all of them when it changed,
    /// otherwise those that registered since. Runs after the step's commit
    /// and after RegisterAck, which must come first on a new stream.
    pub(crate) fn sync_endpoints(&mut self) {
        if self.in_grace() {
            return;
        }
        let set = self.endpoint_set();
        let changed = self.discovery.sent.as_ref() != Some(&set);
        if changed {
            self.discovery.version += 1;
            self.discovery.pending = self
                .nodes
                .iter()
                .filter(|(_, n)| n.conn.is_some())
                .map(|(id, _)| id.clone())
                .collect();
            tracing::debug!(
                version = self.discovery.version,
                services = set.len(),
                "service endpoints changed"
            );
        }
        if self.discovery.pending.is_empty() {
            self.discovery.sent = Some(set);
            return;
        }
        let msg = pb::ControllerMessage {
            msg: Some(pb::controller_message::Msg::Endpoints(pb::Endpoints {
                version: self.discovery.version,
                services: set
                    .iter()
                    .map(|(id, addrs)| pb::ServiceEndpoints {
                        service_id: id.clone(),
                        addresses: addrs.clone(),
                    })
                    .collect(),
            })),
        };
        let mut retry = Vec::new();
        for node_id in std::mem::take(&mut self.discovery.pending) {
            let Some(n) = self.nodes.get(&node_id) else {
                continue;
            };
            if !n.features.iter().any(|f| f == FEATURE) {
                continue; // an older agent: it would not know the message
            }
            let Some((_, tx)) = &n.conn else { continue };
            if let Err(e) = tx.try_send(Ok(msg.clone())) {
                tracing::warn!(node = %node_id, "agent stream busy, endpoints will be resent: {e}");
                retry.push(node_id);
            }
        }
        self.discovery.pending = retry;
        self.discovery.sent = Some(set);
    }
}
