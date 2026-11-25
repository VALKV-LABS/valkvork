//! Agent-facing handlers: Register (FR-CON-03), Heartbeat (FR-AGE-03),
//! CommandResult (FR-CON-09) and node health (FR-CON-02).

use super::Core;
use crate::db::NodeCertRecord;
use crate::model::{AgentSender, Node, NodeEntry};
use pb::WorkloadState;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use valkvork_common::config::NodeCertificateMode;
use valkvork_common::models::{
    now_ms, DesiredState, EventKind, NodeStatus, SubjectType, WorkloadStatus,
};
use valkvork_common::node_cert::CertInfo;
use valkvork_common::tls::tokens_equal;
use valkvork_common::ValkvorkError;
use valkvork_proto::v1 as pb;

const CONTROLLER: &str = "controller";

impl Core {
    pub(crate) fn on_register(
        &mut self,
        r: pb::Register,
        conn_id: u64,
        tx: AgentSender,
        client_cert: Option<CertInfo>,
    ) {
        let issues = self.issues_certificates();
        let ack = |accepted: bool, reason: &str, interval: u32| pb::ControllerMessage {
            msg: Some(pb::controller_message::Msg::RegisterAck(pb::RegisterAck {
                accepted,
                reject_reason: reason.to_string(),
                heartbeat_interval_ms: interval,
                issues_certificates: accepted && issues,
            })),
        };
        let interval = self.cfg.heartbeat_interval_ms;

        // FR-SEC-01 / FR-SEC-02: unauthenticated Register calls are rejected.
        let auth = match self.authenticate(&r, client_cert.as_ref()) {
            Ok(how) => how,
            Err(reason) => {
                tracing::warn!(node = %r.node_id, reason, "rejected Register");
                let _ = tx.try_send(Ok(ack(false, reason, interval)));
                return;
            }
        };
        tracing::debug!(node = %r.node_id, auth, "Register authenticated");
        if r.node_id.is_empty()
            || r.allocatable_cpu_millicores == 0
            || r.allocatable_memory_bytes == 0
        {
            let _ = tx.try_send(Ok(ack(
                false,
                "node_id and allocatable capacity are required",
                interval,
            )));
            return;
        }

        let now = now_ms();
        let node_id = r.node_id.clone();
        let previous = self.nodes.remove(&node_id);
        let was_unhealthy = previous
            .as_ref()
            .is_some_and(|p| p.node.status == NodeStatus::Unhealthy);
        let node = Node {
            node_id: node_id.clone(),
            hostname: r.hostname.clone(),
            ip_address: r.ip_address.clone(),
            node_class: r.node_class.clone(),
            agent_version: r.agent_version.clone(),
            runtime: r.runtime.clone(),
            allocatable_cpu_millicores: r.allocatable_cpu_millicores,
            allocatable_memory_bytes: r.allocatable_memory_bytes,
            host_volumes: r.host_volumes.clone(),
            zone: r.zone.clone(),
            instance_id: r.instance_id.clone(),
            max_block_volumes: r.max_block_volumes,
            status: NodeStatus::Healthy,
            draining: previous.as_ref().is_some_and(|p| p.node.draining),
            last_heartbeat_at: now,
            created_at: previous.as_ref().map(|p| p.node.created_at).unwrap_or(now),
            updated_at: now,
        };
        self.nodes.insert(
            node_id.clone(),
            NodeEntry {
                node,
                last_seen: Instant::now(),
                conn: Some((conn_id, tx.clone())),
                cpu_usage_percent: 0.0,
                used_memory_bytes: 0,
                features: r.features.clone(),
            },
        );
        self.touch_node(&node_id);
        self.metrics.registration();
        let msg = match (&previous, was_unhealthy) {
            (None, _) => format!(
                "{} ({}) joined, class {}",
                r.hostname, r.ip_address, r.node_class
            ),
            (Some(_), true) => format!("{} HEALTHY (re-registered)", r.hostname),
            (Some(_), false) => format!("{} reconnected", r.hostname),
        };
        self.event(
            EventKind::Node,
            SubjectType::Node,
            &node_id,
            msg,
            CONTROLLER,
        );

        // The ack must precede any command on the stream.
        self.direct.push((tx, ack(true, "", interval)));
        self.discovery.needs_set(&node_id);

        // The agent may have restarted and lost an accepted stop: its
        // inventory decides again.
        self.acked_stops.retain(|_, (n, _)| *n != node_id);
        self.reconcile_inventory(&node_id, &r.inventory);
        self.resend_for_node(&node_id);
        self.reconcile();
    }

    fn issues_certificates(&self) -> bool {
        self.node_ca.is_some() && self.cfg.node_certificates.mode != NodeCertificateMode::Off
    }

    /// How a Register proves who it is (FR-SEC-01, FR-SEC-02). A client
    /// certificate counts only if its name is the node id and its serial is
    /// one the controller issued to that node and has not revoked. Anything
    /// else falls back to the join token, which in `require` mode only
    /// enrols a node that holds no certificate yet.
    fn authenticate(
        &self,
        r: &pb::Register,
        cert: Option<&CertInfo>,
    ) -> Result<&'static str, &'static str> {
        let mode = self.cfg.node_certificates.mode;
        let record = self.node_certs.get(&r.node_id);
        if let (Some(c), true) = (cert, self.issues_certificates()) {
            let issued = record.is_some_and(|rec| {
                rec.serial == c.serial || rec.prev_serial.as_deref() == Some(c.serial.as_str())
            });
            if c.common_name == r.node_id && issued {
                return Ok("certificate");
            }
        }
        if !tokens_equal(&r.join_token, &self.join_token) {
            return Err("invalid join token");
        }
        if mode == NodeCertificateMode::Require && record.is_some() {
            return Err("this node was issued a client certificate; connect with it");
        }
        Ok("join token")
    }

    /// FR-SEC-02: sign the agent's CSR and send the certificate back.
    fn on_certificate_request(&mut self, node_id: &str, req: pb::CertificateRequest) {
        let Some(tx) = self
            .nodes
            .get(node_id)
            .and_then(|e| e.conn.as_ref().map(|(_, tx)| tx.clone()))
        else {
            return;
        };
        let reply = match self.issue_certificate(node_id, &req.csr_pem) {
            Ok(pem) => pb::NodeCertificate {
                certificate_pem: pem,
                error: String::new(),
            },
            Err(e) => {
                tracing::warn!(node = node_id, error = %e, "certificate request refused");
                pb::NodeCertificate {
                    certificate_pem: String::new(),
                    error: e.to_string(),
                }
            }
        };
        self.direct.push((
            tx,
            pb::ControllerMessage {
                msg: Some(pb::controller_message::Msg::NodeCertificate(reply)),
            },
        ));
    }

    fn issue_certificate(&mut self, node_id: &str, csr_pem: &str) -> Result<String, ValkvorkError> {
        if !self.issues_certificates() {
            return Err(ValkvorkError::FailedPrecondition(
                "this controller does not issue node certificates".into(),
            ));
        }
        let ca = self.node_ca.clone().expect("checked above");
        let lifetime = Duration::from_secs(
            u64::from(self.cfg.node_certificates.lifetime_days.max(1)) * 86_400,
        );
        let issued = ca.sign(csr_pem, node_id, lifetime)?;
        let rec = NodeCertRecord {
            serial: issued.serial,
            prev_serial: self.node_certs.get(node_id).map(|r| r.serial.clone()),
            not_after: issued.not_after_ms,
        };
        self.db.save_node_cert(node_id, &rec)?;
        self.node_certs.insert(node_id.to_string(), rec);
        let host = self
            .nodes
            .get(node_id)
            .map(|e| e.node.hostname.clone())
            .unwrap_or_else(|| node_id.to_string());
        let days = self.cfg.node_certificates.lifetime_days.max(1);
        self.event(
            EventKind::Node,
            SubjectType::Node,
            node_id,
            format!("{host} was issued a client certificate, valid for {days} days"),
            CONTROLLER,
        );
        Ok(issued.cert_pem)
    }

    /// FR-CON-03: compare the agent's inventory with the workload table.
    fn reconcile_inventory(&mut self, node_id: &str, inventory: &[pb::WorkloadStatus]) {
        let grace = self.in_grace();
        let reported: HashSet<&str> = inventory.iter().map(|s| s.workload_id.as_str()).collect();

        for st in inventory {
            let wid = st.workload_id.as_str();
            let Some(w) = self.workloads.get(wid).cloned() else {
                // Orphan: no row at all.
                if !grace {
                    self.event(
                        EventKind::Workload,
                        SubjectType::Workload,
                        wid,
                        format!("orphan container on {node_id}; stopping"),
                        CONTROLLER,
                    );
                    self.queue_stop(wid, node_id);
                }
                continue;
            };

            if w.node_id.as_deref() != Some(node_id)
                || w.desired_state == DesiredState::Stop
                || w.status.is_terminal()
            {
                // Wrong node, stop requested, or already terminal: stop it (again).
                if !self.has_outstanding_stop(wid) {
                    self.queue_stop(wid, node_id);
                }
                if matches!(
                    w.status,
                    WorkloadStatus::Lost | WorkloadStatus::Running | WorkloadStatus::Assigned
                ) {
                    if let Some(w) = self.wl(wid) {
                        w.status = WorkloadStatus::Stopping;
                    }
                }
                continue;
            }

            if w.status == WorkloadStatus::Lost {
                let replacement = self
                    .workloads
                    .values()
                    .find(|r| {
                        r.replaces_workload_id.as_deref() == Some(wid)
                            && r.desired_state == DesiredState::Run
                            && !r.status.is_terminal()
                    })
                    .map(|r| (r.workload_id.clone(), r.status));
                match replacement {
                    Some((rid, WorkloadStatus::Pending)) | Some((rid, WorkloadStatus::Lost)) => {
                        // Re-adopt: the replacement never started.
                        self.remove_workload(&rid);
                        self.adopt(wid, st);
                        self.event(
                            EventKind::Workload,
                            SubjectType::Workload,
                            wid,
                            format!("re-adopted on {node_id}; pending replacement {rid} cancelled"),
                            CONTROLLER,
                        );
                    }
                    Some((_, _)) => {
                        // Replacement is placed: stop the old copy.
                        if let Some(w) = self.wl(wid) {
                            w.desired_state = DesiredState::Stop;
                            w.status = WorkloadStatus::Stopping;
                            w.status_reason = Some("REPLACED_WHILE_LOST".into());
                        }
                        self.queue_stop(wid, node_id);
                        self.event(
                            EventKind::Workload,
                            SubjectType::Workload,
                            wid,
                            format!("lost copy on {node_id} superseded by replacement; stopping"),
                            CONTROLLER,
                        );
                    }
                    None => {
                        // No replacement exists (e.g. grace period): keep this one.
                        self.adopt(wid, st);
                        self.event(
                            EventKind::Workload,
                            SubjectType::Workload,
                            wid,
                            format!("re-adopted on {node_id}"),
                            CONTROLLER,
                        );
                    }
                }
                continue;
            }

            // ASSIGNED / RUNNING / STOPPING on this node: just update.
            self.apply_status(wid, st);
        }

        // Rows that claim to be on this node but the agent does not have.
        let missing: Vec<_> = self
            .workloads
            .values()
            .filter(|w| {
                w.node_id.as_deref() == Some(node_id) && !reported.contains(w.workload_id.as_str())
            })
            .map(|w| (w.workload_id.clone(), w.status))
            .collect();
        for (wid, status) in missing {
            match status {
                // The start was lost (agent restart or partition): re-send.
                WorkloadStatus::Assigned if !self.outstanding.contains_key(&wid) => {
                    self.queue_start(&wid);
                }
                WorkloadStatus::Running => self.fail_workload(&wid, "CONTAINER_MISSING"),
                WorkloadStatus::Stopping | WorkloadStatus::Lost => {
                    if let Some(w) = self.wl(&wid) {
                        w.status = WorkloadStatus::Stopped;
                        w.desired_state = DesiredState::Stop;
                    }
                    self.outstanding.remove(&wid);
                }
                _ => {}
            }
        }
    }

    fn adopt(&mut self, wid: &str, st: &pb::WorkloadStatus) {
        if let Some(w) = self.wl(wid) {
            w.status = WorkloadStatus::Assigned;
            w.status_reason = None;
        }
        self.assigned_at.insert(wid.to_string(), Instant::now());
        self.apply_status(wid, st);
    }

    pub(crate) fn on_agent_msg(
        &mut self,
        node_id: &str,
        conn_id: u64,
        msg: pb::agent_message::Msg,
    ) {
        // Ignore traffic from a superseded stream.
        let Some(entry) = self.nodes.get_mut(node_id) else {
            return;
        };
        if entry.conn.as_ref().map(|(id, _)| *id) != Some(conn_id) {
            return;
        }
        entry.last_seen = Instant::now();

        if entry.node.status == NodeStatus::Unhealthy {
            // Only a fresh Register brings a node back (FR-CON-02). Close the
            // stream so the agent reconnects and re-registers with inventory.
            entry.conn = None;
            return;
        }

        match msg {
            pb::agent_message::Msg::Register(_) => {
                tracing::warn!(
                    node = node_id,
                    "duplicate Register on an open stream ignored"
                );
            }
            pb::agent_message::Msg::Heartbeat(hb) => self.on_heartbeat(node_id, hb),
            pb::agent_message::Msg::CommandResult(res) => self.on_command_result(node_id, res),
            pb::agent_message::Msg::CertificateRequest(req) => {
                self.on_certificate_request(node_id, req)
            }
        }
    }

    fn on_heartbeat(&mut self, node_id: &str, hb: pb::Heartbeat) {
        if let Some(e) = self.nodes.get_mut(node_id) {
            e.cpu_usage_percent = hb.node_cpu_usage_percent;
            e.used_memory_bytes = hb.node_used_memory_bytes;
        }
        let grace = self.in_grace();
        let now = Instant::now();
        let mut reported = HashSet::new();
        for st in &hb.workloads {
            reported.insert(st.workload_id.clone());
            let wid = st.workload_id.as_str();
            match self.workloads.get(wid) {
                None => {
                    if !grace && !self.has_outstanding_stop(wid) {
                        self.event(
                            EventKind::Workload,
                            SubjectType::Workload,
                            wid,
                            format!("orphan container on {node_id}; stopping"),
                            CONTROLLER,
                        );
                        self.queue_stop(wid, node_id);
                    }
                }
                Some(w)
                    if w.node_id.as_deref() != Some(node_id)
                        || w.desired_state == DesiredState::Stop
                        || w.status.is_terminal() =>
                {
                    let (status, terminal) = (w.status, w.status.is_terminal());
                    if !self.has_outstanding_stop(wid) {
                        self.queue_stop(wid, node_id);
                    }
                    if !terminal && status != WorkloadStatus::Stopping {
                        if let Some(w) = self.wl(wid) {
                            w.status = WorkloadStatus::Stopping;
                        }
                    }
                }
                Some(_) => self.apply_status(wid, st),
            }
            if st.state == WorkloadState::Running as i32 {
                let samples = self.cpu_samples.entry(st.workload_id.clone()).or_default();
                samples.push_back((now, st.cpu_usage_millicores));
                let window = Duration::from_secs(self.cfg.autoscaler.window_seconds);
                while samples
                    .front()
                    .is_some_and(|(t, _)| now.duration_since(*t) > window)
                {
                    samples.pop_front();
                }
            }
        }

        // Rows on this node that the agent no longer reports.
        let missing: Vec<_> = self
            .workloads
            .values()
            .filter(|w| w.node_id.as_deref() == Some(node_id) && !reported.contains(&w.workload_id))
            .map(|w| (w.workload_id.clone(), w.status))
            .collect();
        for (wid, status) in missing {
            match status {
                WorkloadStatus::Stopping => {
                    // Container removed: stop confirmed.
                    if let Some(w) = self.wl(&wid) {
                        w.status = WorkloadStatus::Stopped;
                    }
                    self.outstanding.remove(&wid);
                }
                WorkloadStatus::Running => self.fail_workload(&wid, "CONTAINER_MISSING"),
                WorkloadStatus::Assigned => {
                    // Acked but not reported (e.g. agent restarted mid-pull):
                    // re-send the idempotent start after a short delay.
                    let stale = self
                        .assigned_at
                        .get(&wid)
                        .is_some_and(|t| now.duration_since(*t) > Duration::from_secs(15));
                    if stale && !self.outstanding.contains_key(&wid) {
                        self.assigned_at.insert(wid.clone(), now);
                        self.queue_start(&wid);
                    }
                }
                _ => {}
            }
        }
        self.reconcile();
    }

    /// Apply an agent-reported state to a workload that belongs on this node.
    pub(crate) fn apply_status(&mut self, wid: &str, st: &pb::WorkloadStatus) {
        let Some(w) = self.workloads.get(wid) else {
            return;
        };
        let state = WorkloadState::try_from(st.state).unwrap_or(WorkloadState::Unspecified);
        let (status, svc_id, version) = (w.status, w.service_id.clone(), w.service_version);
        let changed_meta = w.restart_count != st.restart_count
            || (!st.container_id.is_empty()
                && w.container_id.as_deref() != Some(st.container_id.as_str()));
        if changed_meta {
            if let Some(w) = self.wl(wid) {
                w.restart_count = st.restart_count;
                if !st.container_id.is_empty() {
                    w.container_id = Some(st.container_id.clone());
                }
            }
        }

        self.apply_health(wid, st);

        match state {
            WorkloadState::Running if status == WorkloadStatus::Assigned => {
                if let Some(w) = self.wl(wid) {
                    w.status = WorkloadStatus::Running;
                    w.status_reason = None;
                    if w.started_at.is_none() {
                        w.started_at = Some(now_ms());
                    }
                }
                self.assigned_at.remove(wid);
                // A healthy start of the current version clears the failure streak.
                let current = self
                    .services
                    .get(&svc_id)
                    .is_some_and(|s| s.version == version);
                if current {
                    self.clear_failures(&svc_id);
                }
            }
            WorkloadState::Failed => {
                let reason = if st.reason.is_empty() {
                    "FAILED".to_string()
                } else {
                    st.reason.clone()
                };
                if matches!(status, WorkloadStatus::Assigned | WorkloadStatus::Running) {
                    self.fail_workload(wid, &reason);
                }
            }
            WorkloadState::Completed => {
                if matches!(status, WorkloadStatus::Assigned | WorkloadStatus::Running) {
                    self.complete_workload(wid);
                }
            }
            WorkloadState::Stopped => match status {
                WorkloadStatus::Stopping => {
                    if let Some(w) = self.wl(wid) {
                        w.status = WorkloadStatus::Stopped;
                    }
                }
                WorkloadStatus::Assigned | WorkloadStatus::Running => {
                    self.fail_workload(wid, "STOPPED_UNEXPECTEDLY")
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// Track the reported health. Not persisted: agents report it every
    /// heartbeat, so it is back within seconds of a controller restart.
    fn apply_health(&mut self, wid: &str, st: &pb::WorkloadStatus) {
        use crate::model::Health;
        let health = match pb::HealthState::try_from(st.health).unwrap_or_default() {
            pb::HealthState::Unspecified => Health::Unknown,
            pb::HealthState::Starting => Health::Starting,
            pb::HealthState::Healthy => Health::Healthy,
            pb::HealthState::Unhealthy => Health::Unhealthy,
        };
        let Some(w) = self.workloads.get_mut(wid) else {
            return;
        };
        if w.health == health {
            return;
        }
        w.health = health;
        w.healthy_since = (health == Health::Healthy).then(now_ms);
        if health == Health::Unhealthy {
            let svc_id = w.service_id.clone();
            self.event(
                EventKind::Workload,
                SubjectType::Workload,
                wid,
                format!("{svc_id} workload UNHEALTHY: health check failed; the agent restarts it"),
                CONTROLLER,
            );
        }
    }

    /// A batch workload exited 0: COMPLETED, its container removed, and the
    /// service's failure streak cleared. Never replaced.
    pub(crate) fn complete_workload(&mut self, wid: &str) {
        let Some(w) = self.wl(wid) else { return };
        w.status = WorkloadStatus::Completed;
        w.status_reason = Some("COMPLETED".into());
        w.desired_state = DesiredState::Stop;
        if w.started_at.is_none() {
            w.started_at = Some(now_ms());
        }
        let (svc_id, version, node) = (w.service_id.clone(), w.service_version, w.node_id.clone());
        self.assigned_at.remove(wid);
        self.cpu_samples.remove(wid);
        if let Some(node) = node {
            self.queue_stop(wid, &node);
        }
        self.event(
            EventKind::Workload,
            SubjectType::Workload,
            wid,
            format!("{svc_id} workload COMPLETED"),
            CONTROLLER,
        );
        if self
            .services
            .get(&svc_id)
            .is_some_and(|s| s.version == version)
        {
            self.clear_failures(&svc_id);
        }
    }

    /// Mark a workload FAILED, clean up its container and count the failure
    /// against the service (FR-CON-05 backoff, FR-CON-07 halt).
    pub(crate) fn fail_workload(&mut self, wid: &str, reason: &str) {
        let Some(w) = self.wl(wid) else { return };
        let prior = w.status;
        w.status = WorkloadStatus::Failed;
        w.status_reason = Some(reason.to_string());
        w.desired_state = DesiredState::Stop;
        let (svc_id, version, node) = (w.service_id.clone(), w.service_version, w.node_id.clone());
        self.assigned_at.remove(wid);
        self.cpu_samples.remove(wid);
        // Have the agent remove the stopped container (it leaves FAILED ones in place).
        if let Some(node) = node.filter(|_| prior != WorkloadStatus::Lost) {
            self.queue_stop(wid, &node);
        }
        self.event(
            EventKind::Workload,
            SubjectType::Workload,
            wid,
            format!("{svc_id} workload FAILED: {reason}"),
            CONTROLLER,
        );
        self.record_failure(&svc_id, version);
    }

    fn on_command_result(&mut self, node_id: &str, res: pb::CommandResult) {
        let Some(o) = self.outstanding.get(&res.workload_id) else {
            return;
        };
        if o.command_id != res.command_id {
            return; // ack for a superseded command
        }
        let is_start = o.is_start;
        let node = o.node_id.clone();
        self.outstanding.remove(&res.workload_id);
        if res.accepted {
            if !is_start {
                self.acked_stops
                    .insert(res.workload_id.clone(), (node, Instant::now()));
            }
            return;
        }
        tracing::warn!(node = node_id, workload = %res.workload_id, error = %res.error, "agent rejected command");
        if is_start {
            // e.g. RUNTIME_UNAVAILABLE: unbind and let the scheduler retry.
            let still_assigned = self
                .workloads
                .get(&res.workload_id)
                .is_some_and(|w| w.status == WorkloadStatus::Assigned);
            if still_assigned {
                if let Some(w) = self.wl(&res.workload_id) {
                    w.status = WorkloadStatus::Pending;
                    w.node_id = None;
                    w.status_reason = Some(format!("REJECTED: {}", res.error));
                }
                self.assigned_at.remove(&res.workload_id);
            }
        }
        // Rejected stops are re-issued by reconcile.
        self.reconcile();
    }

    pub(crate) fn on_disconnect(&mut self, node_id: &str, conn_id: u64) {
        if let Some(e) = self.nodes.get_mut(node_id) {
            if e.conn.as_ref().map(|(id, _)| *id) == Some(conn_id) {
                e.conn = None;
                tracing::info!(node = node_id, "agent stream closed");
            }
        }
        // FR-CON-02: a broken stream does not shorten the health timer.
    }

    /// FR-CON-02: mark silent nodes UNHEALTHY and replace their workloads.
    pub(crate) fn check_health(&mut self) {
        if self.in_grace() {
            return;
        }
        if !self.grace_logged {
            self.grace_logged = true;
            tracing::info!("recovery grace period over; normal reconciliation resumed");
        }
        let timeout = Duration::from_secs(self.cfg.node_unhealthy_timeout_seconds);
        let now = Instant::now();
        let silent: Vec<String> = self
            .nodes
            .values()
            .filter(|e| {
                e.node.status == NodeStatus::Healthy && now.duration_since(e.last_seen) >= timeout
            })
            .map(|e| e.node.node_id.clone())
            .collect();
        for node_id in silent {
            self.mark_unhealthy(&node_id);
        }
    }

    pub(crate) fn mark_unhealthy(&mut self, node_id: &str) {
        let Some(e) = self.nodes.get_mut(node_id) else {
            return;
        };
        let silent_for = e.last_seen.elapsed().as_secs();
        e.node.status = NodeStatus::Unhealthy;
        e.node.last_heartbeat_at = now_ms() - e.last_seen.elapsed().as_millis() as i64;
        e.conn = None;
        let hostname = e.node.hostname.clone();
        self.touch_node(node_id);

        let affected: Vec<_> = self
            .workloads
            .values()
            .filter(|w| {
                w.node_id.as_deref() == Some(node_id)
                    && matches!(
                        w.status,
                        WorkloadStatus::Assigned
                            | WorkloadStatus::Running
                            | WorkloadStatus::Stopping
                    )
            })
            .map(|w| w.workload_id.clone())
            .collect();
        let mut replaced = 0;
        for wid in &affected {
            let Some(w) = self.wl(wid) else { continue };
            w.status = WorkloadStatus::Lost;
            w.status_reason = Some("NODE_UNHEALTHY".into());
            let (svc_id, run) = (w.service_id.clone(), w.desired_state == DesiredState::Run);
            self.outstanding.remove(wid);
            self.assigned_at.remove(wid);
            self.cpu_samples.remove(wid);
            if run {
                if let Some(svc) = self.services.get(&svc_id) {
                    let r = self.new_workload(svc, Some(wid.clone()));
                    self.insert_workload(r);
                    replaced += 1;
                }
            }
        }
        // Commands queued for this node can no longer be delivered.
        self.outstanding.retain(|_, o| o.node_id != node_id);
        self.event(
            EventKind::Node,
            SubjectType::Node,
            node_id,
            format!("{hostname} UNHEALTHY (no message for {silent_for} s), {} workloads lost, {replaced} replaced", affected.len()),
            CONTROLLER,
        );
        self.reconcile();
    }
}
