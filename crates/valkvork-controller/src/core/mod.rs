//! The controller core: one Tokio task that owns all cluster state and the
//! SQLite write connection (FR-CON-01's "writer task").
//!
//! Every input — agent registrations, heartbeats, command results, admin
//! requests and timer ticks — arrives as a message. A handler mutates the
//! in-memory tables and marks rows dirty; [`Core::finish`] then commits all
//! dirty rows in one fsync'd transaction and only afterwards sends commands
//! to agents and replies to admin callers. So:
//!
//! * an acknowledged `Apply` is durable (NFR-06);
//! * an agent is never told to start a workload the database does not know;
//! * there is a single writer, so no `SQLITE_BUSY` (FR-CON-01).

mod admin;
mod agents;
mod autoscale;
mod observe;
mod reconcile;
#[cfg(test)]
mod tests;

use crate::db::{Batch, Db, NewEvent, NewScalingEvent};
use crate::model::{AgentSender, CpuSamples, NodeEntry, Service, ServiceRuntime, Workload};
use crate::scheduler::{NodeCapacity, Snapshot};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot};
use valkvork_common::config::ControllerConfig;
use valkvork_common::models::{now_ms, DesiredState, EventKind, SubjectType, WorkloadStatus};
use valkvork_common::ValkvorkError;
use valkvork_proto::v1 as pb;

pub use admin::{AdminOp, ApplyOutcome, WorkloadFilter};
pub use observe::{ClassCapacity, ClassPoint, Overview, SeriesPoint};

/// Messages accepted by the core.
pub enum CoreMsg {
    AgentConnected {
        register: pb::Register,
        conn_id: u64,
        tx: AgentSender,
    },
    AgentMsg {
        node_id: String,
        conn_id: u64,
        msg: pb::agent_message::Msg,
    },
    AgentDisconnected {
        node_id: String,
        conn_id: u64,
    },
    Admin(AdminOp),
}

/// Cloneable handle used by the gRPC adapters.
#[derive(Clone)]
pub struct CoreHandle {
    tx: mpsc::Sender<CoreMsg>,
    changes: broadcast::Sender<u64>,
}

impl CoreHandle {
    /// Notified after every commit that changed state (UI live updates,
    /// FR-UI-04). Each subscriber has its own bounded buffer (FR-UI-09).
    pub fn subscribe(&self) -> broadcast::Receiver<u64> {
        self.changes.subscribe()
    }

    pub async fn send(&self, msg: CoreMsg) -> Result<(), ValkvorkError> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| ValkvorkError::Internal("controller core stopped".into()))
    }

    /// Send an admin op built around a reply channel and wait for the answer.
    pub async fn ask<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, ValkvorkError>>) -> AdminOp,
    ) -> Result<T, ValkvorkError> {
        let (tx, rx) = oneshot::channel();
        self.send(CoreMsg::Admin(build(tx))).await?;
        rx.await
            .map_err(|_| ValkvorkError::Internal("controller core dropped the request".into()))?
    }
}

/// A command sent (or waiting to be sent) to an agent (FR-CON-09).
#[derive(Debug, Clone)]
pub(crate) struct Outstanding {
    pub node_id: String,
    pub command_id: String,
    pub msg: pb::ControllerMessage,
    pub is_start: bool,
    pub sent_at: Option<Instant>,
    pub resent: bool,
}

type AfterCommit = Box<dyn FnOnce() + Send>;

pub struct Core {
    pub(crate) cfg: ControllerConfig,
    pub(crate) started: Instant,
    changes: broadcast::Sender<u64>,
    change_seq: u64,
    pub(crate) metrics: observe::CoreMetrics,
    pub(crate) series: observe::Series,
    pub(crate) db: Db,
    pub(crate) join_token: String,
    pub(crate) grace_until: Instant,
    pub(crate) grace_logged: bool,

    pub(crate) nodes: BTreeMap<String, NodeEntry>,
    pub(crate) services: BTreeMap<String, Service>,
    pub(crate) svc_rt: HashMap<String, ServiceRuntime>,
    pub(crate) workloads: HashMap<String, Workload>,
    /// When each ASSIGNED workload was bound (start timeout, FR-CON-09).
    pub(crate) assigned_at: HashMap<String, Instant>,
    /// Outstanding commands keyed by workload id.
    pub(crate) outstanding: HashMap<String, Outstanding>,
    pub(crate) cpu_samples: CpuSamples,

    // Pending writes and side effects for the current step.
    dirty_nodes: HashSet<String>,
    dirty_services: HashSet<String>,
    dirty_workloads: HashSet<String>,
    deleted_workloads: Vec<String>,
    deleted_services: Vec<String>,
    deleted_nodes: Vec<String>,
    events: Vec<NewEvent>,
    scaling_events: Vec<NewScalingEvent>,
    heartbeats: Vec<(String, i64)>,
    to_send: Vec<String>,
    direct: Vec<(AgentSender, pb::ControllerMessage)>,
    after_commit: Vec<AfterCommit>,
    command_seq: u64,
}

impl Core {
    /// Load desired state and enter the recovery grace period (FR-CON-04).
    pub fn new(cfg: ControllerConfig, db: Db, join_token: String) -> Result<Self, ValkvorkError> {
        let start = Instant::now();
        let grace_until = start + Duration::from_secs(cfg.recovery_grace_period_seconds);

        let nodes = db
            .load_nodes()?
            .into_iter()
            .map(|node| {
                // Heartbeat clocks start at the controller's start time.
                let entry = NodeEntry {
                    node,
                    last_seen: start,
                    conn: None,
                    cpu_usage_percent: 0.0,
                    used_memory_bytes: 0,
                    features: Vec::new(),
                };
                (entry.node.node_id.clone(), entry)
            })
            .collect();
        let services: BTreeMap<_, _> = db
            .load_services()?
            .into_iter()
            .map(|s| (s.service_id.clone(), s))
            .collect();
        let workloads: HashMap<_, _> = db
            .load_workloads()?
            .into_iter()
            .map(|w| (w.workload_id.clone(), w))
            .collect();
        let assigned_at = workloads
            .values()
            .filter(|w| w.status == WorkloadStatus::Assigned)
            .map(|w| (w.workload_id.clone(), start))
            .collect();

        tracing::info!(
            services = services.len(),
            workloads = workloads.len(),
            grace_seconds = cfg.recovery_grace_period_seconds,
            "loaded desired state; entering recovery grace period"
        );

        Ok(Self {
            cfg,
            started: start,
            changes: broadcast::channel(64).0,
            change_seq: 0,
            metrics: Default::default(),
            series: Default::default(),
            db,
            join_token,
            grace_until,
            grace_logged: false,
            nodes,
            svc_rt: services
                .keys()
                .map(|k| (k.clone(), ServiceRuntime::default()))
                .collect(),
            services,
            workloads,
            assigned_at,
            outstanding: HashMap::new(),
            cpu_samples: HashMap::new(),
            dirty_nodes: HashSet::new(),
            dirty_services: HashSet::new(),
            dirty_workloads: HashSet::new(),
            deleted_workloads: Vec::new(),
            deleted_services: Vec::new(),
            deleted_nodes: Vec::new(),
            events: Vec::new(),
            scaling_events: Vec::new(),
            heartbeats: Vec::new(),
            to_send: Vec::new(),
            direct: Vec::new(),
            after_commit: Vec::new(),
            command_seq: 0,
        })
    }

    pub fn spawn(
        self,
    ) -> (
        CoreHandle,
        tokio::task::JoinHandle<Result<(), ValkvorkError>>,
    ) {
        let (tx, rx) = mpsc::channel(1024);
        let changes = self.changes.clone();
        (CoreHandle { tx, changes }, tokio::spawn(self.run(rx)))
    }

    async fn run(mut self, mut rx: mpsc::Receiver<CoreMsg>) -> Result<(), ValkvorkError> {
        use tokio::time::{interval, MissedTickBehavior};
        let mk = |secs: u64| {
            let mut i = interval(Duration::from_secs(secs.max(1)));
            i.set_missed_tick_behavior(MissedTickBehavior::Delay);
            i
        };
        let mut reconcile_tick = mk(self.cfg.reconcile_interval_seconds);
        let mut health_tick = mk(1);
        let mut flush_tick = mk(self.cfg.heartbeat_flush_interval_seconds);
        let mut autoscale_tick = mk(self.cfg.autoscaler.evaluation_interval_seconds);
        let mut prune_tick = mk(600);
        let mut series_tick = mk(60);
        series_tick.reset(); // first sample after a full minute

        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    None => return Ok(()),
                    Some(m) => self.handle(m),
                },
                _ = reconcile_tick.tick() => {
                    self.retry_commands();
                    self.reconcile();
                }
                _ = health_tick.tick() => self.check_health(),
                _ = flush_tick.tick() => self.flush_heartbeats(),
                _ = autoscale_tick.tick() => {
                    if self.cfg.autoscaler.enabled {
                        self.autoscale();
                        self.reconcile();
                    }
                }
                _ = prune_tick.tick() => self.prune_history(),
                _ = series_tick.tick() => self.sample_series(),
            }
            self.finish()?;
        }
    }

    pub(crate) fn handle(&mut self, msg: CoreMsg) {
        match msg {
            CoreMsg::AgentConnected {
                register,
                conn_id,
                tx,
            } => self.on_register(register, conn_id, tx),
            CoreMsg::AgentMsg {
                node_id,
                conn_id,
                msg,
            } => self.on_agent_msg(&node_id, conn_id, msg),
            CoreMsg::AgentDisconnected { node_id, conn_id } => {
                self.on_disconnect(&node_id, conn_id)
            }
            CoreMsg::Admin(op) => self.on_admin(op),
        }
    }

    // ------------------------------------------------------------------
    // Commit and side effects
    // ------------------------------------------------------------------

    /// Commit dirty state, then send queued commands and admin replies.
    /// A database failure is fatal: systemd restarts the controller, which
    /// recovers from SQLite (FR-CON-04).
    pub(crate) fn finish(&mut self) -> Result<(), ValkvorkError> {
        let batch = Batch {
            nodes: self
                .dirty_nodes
                .iter()
                .filter_map(|id| self.nodes.get(id).map(|e| &e.node))
                .collect(),
            services: self
                .dirty_services
                .iter()
                .filter_map(|id| self.services.get(id))
                .collect(),
            workloads: self
                .dirty_workloads
                .iter()
                .filter_map(|id| self.workloads.get(id))
                .collect(),
            deleted_workloads: std::mem::take(&mut self.deleted_workloads),
            deleted_services: std::mem::take(&mut self.deleted_services),
            deleted_nodes: std::mem::take(&mut self.deleted_nodes),
            events: std::mem::take(&mut self.events),
            scaling_events: std::mem::take(&mut self.scaling_events),
            heartbeats: std::mem::take(&mut self.heartbeats),
        };
        let changed = !batch.is_empty();
        if let Err(e) = self.db.commit(&batch) {
            tracing::error!(error = %e, "failed to commit state; exiting so the controller restarts from SQLite");
            return Err(e);
        }
        drop(batch);
        self.dirty_nodes.clear();
        self.dirty_services.clear();
        self.dirty_workloads.clear();

        for (tx, msg) in std::mem::take(&mut self.direct) {
            let _ = tx.try_send(Ok(msg));
        }
        let now = Instant::now();
        let mut seen = HashSet::new();
        for wid in std::mem::take(&mut self.to_send) {
            if seen.insert(wid.clone()) {
                self.transmit(&wid, now);
            }
        }
        for f in std::mem::take(&mut self.after_commit) {
            f();
        }
        if changed {
            self.change_seq += 1;
            // No subscribers is fine.
            let _ = self.changes.send(self.change_seq);
        }
        Ok(())
    }

    pub(crate) fn after_commit(&mut self, f: impl FnOnce() + Send + 'static) {
        self.after_commit.push(Box::new(f));
    }

    pub(crate) fn event(
        &mut self,
        kind: EventKind,
        subject_type: SubjectType,
        subject_id: &str,
        message: String,
        actor: &str,
    ) {
        tracing::info!(
            kind = kind.as_str(),
            subject = subject_id,
            actor,
            "{message}"
        );
        self.events.push(NewEvent {
            kind,
            subject_type,
            subject_id: subject_id.to_string(),
            message,
            actor: actor.to_string(),
        });
    }

    pub(crate) fn scaling_event(&mut self, e: NewScalingEvent) {
        self.scaling_events.push(e);
    }

    pub(crate) fn touch_node(&mut self, node_id: &str) {
        if let Some(e) = self.nodes.get_mut(node_id) {
            e.node.updated_at = now_ms();
            self.dirty_nodes.insert(node_id.to_string());
        }
    }

    pub(crate) fn touch_service(&mut self, service_id: &str) {
        if let Some(s) = self.services.get_mut(service_id) {
            s.updated_at = now_ms();
            self.dirty_services.insert(service_id.to_string());
        }
    }

    /// Mutable access to a workload, marking it dirty.
    pub(crate) fn wl(&mut self, workload_id: &str) -> Option<&mut Workload> {
        let w = self.workloads.get_mut(workload_id)?;
        w.updated_at = now_ms();
        self.dirty_workloads.insert(workload_id.to_string());
        Some(w)
    }

    pub(crate) fn insert_workload(&mut self, w: Workload) {
        self.dirty_workloads.insert(w.workload_id.clone());
        self.workloads.insert(w.workload_id.clone(), w);
    }

    pub(crate) fn remove_workload(&mut self, workload_id: &str) {
        self.workloads.remove(workload_id);
        self.dirty_workloads.remove(workload_id);
        self.assigned_at.remove(workload_id);
        self.outstanding.remove(workload_id);
        self.cpu_samples.remove(workload_id);
        self.deleted_workloads.push(workload_id.to_string());
    }

    pub(crate) fn remove_service(&mut self, service_id: &str) {
        let ids: Vec<_> = self
            .workloads
            .values()
            .filter(|w| w.service_id == service_id)
            .map(|w| w.workload_id.clone())
            .collect();
        for id in ids {
            // Rows go with ON DELETE CASCADE; only drop the memory copies.
            self.workloads.remove(&id);
            self.dirty_workloads.remove(&id);
            self.assigned_at.remove(&id);
            self.outstanding.remove(&id);
            self.cpu_samples.remove(&id);
        }
        self.services.remove(service_id);
        self.svc_rt.remove(service_id);
        self.dirty_services.remove(service_id);
        self.deleted_services.push(service_id.to_string());
    }

    /// Forget a node: drop its in-memory entry and pending commands, and
    /// delete its row in the next commit.
    pub(crate) fn remove_node_entry(&mut self, node_id: &str) {
        self.nodes.remove(node_id);
        self.dirty_nodes.remove(node_id);
        self.heartbeats.retain(|(id, _)| id != node_id);
        self.outstanding.retain(|_, o| o.node_id != node_id);
        self.deleted_nodes.push(node_id.to_string());
    }

    pub(crate) fn in_grace(&self) -> bool {
        Instant::now() < self.grace_until
    }

    // ------------------------------------------------------------------
    // Dispatcher (FR-CON-09)
    // ------------------------------------------------------------------

    fn next_command_id(&mut self) -> String {
        self.command_seq += 1;
        format!("{}-{}", now_ms(), self.command_seq)
    }

    /// Queue `StartWorkload` for an ASSIGNED workload.
    pub(crate) fn queue_start(&mut self, workload_id: &str) {
        let command_id = self.next_command_id();
        let Some(w) = self.workloads.get(workload_id) else {
            return;
        };
        let (Some(node_id), Some(svc)) = (w.node_id.clone(), self.services.get(&w.service_id))
        else {
            return;
        };
        let s = &svc.spec;
        let msg = pb::ControllerMessage {
            msg: Some(pb::controller_message::Msg::StartWorkload(
                pb::StartWorkload {
                    command_id: command_id.clone(),
                    workload_id: workload_id.to_string(),
                    service_id: s.id.clone(),
                    service_version: w.service_version,
                    image: s.image.clone(),
                    cpu_millicores: s.cpu_millicores,
                    memory_bytes: s.memory_bytes,
                    disk_bytes: s.disk_bytes,
                    env: s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    secrets: s
                        .secrets
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    check: s.check.as_ref().map(|c| pb::HealthCheck {
                        r#type: c.kind.clone(),
                        port_name: c.port.clone(),
                        path: c.path.clone().unwrap_or_default(),
                        interval_seconds: c.interval_seconds,
                        timeout_seconds: c.timeout_seconds,
                        failures: c.failures,
                        grace_seconds: c.grace_seconds,
                    }),
                    ports: s
                        .ports
                        .iter()
                        .map(|p| pb::PortMapping {
                            name: p.name.clone(),
                            container_port: p.container_port as u32,
                            host_port: p.host_port as u32,
                            host_ip: p.host_ip.clone().unwrap_or_default(),
                        })
                        .collect(),
                    volumes: s
                        .volumes
                        .iter()
                        .map(|v| pb::VolumeMount {
                            r#type: v.kind.clone(),
                            source: v.source.clone(),
                            target: v.target.clone(),
                            read_only: v.read_only,
                        })
                        .collect(),
                    restart: Some(pb::RestartPolicy {
                        attempts: s.restart.attempts,
                        interval_seconds: s.restart.interval_seconds,
                        delay_seconds: s.restart.delay_seconds,
                    }),
                    kill_timeout_seconds: s.kill_timeout_seconds,
                    entrypoint: s.entrypoint.clone().unwrap_or_default(),
                    command: s.command.clone().unwrap_or_default(),
                },
            )),
        };
        self.queue(
            workload_id,
            Outstanding {
                node_id,
                command_id,
                msg,
                is_start: true,
                sent_at: None,
                resent: false,
            },
        );
    }

    /// Queue `StopWorkload` to `node_id` (also used for orphans with no row).
    pub(crate) fn queue_stop(&mut self, workload_id: &str, node_id: &str) {
        let kill_timeout = self
            .workloads
            .get(workload_id)
            .and_then(|w| self.services.get(&w.service_id))
            .map(|s| s.spec.kill_timeout_seconds)
            .unwrap_or(10);
        let command_id = self.next_command_id();
        let msg = pb::ControllerMessage {
            msg: Some(pb::controller_message::Msg::StopWorkload(
                pb::StopWorkload {
                    command_id: command_id.clone(),
                    workload_id: workload_id.to_string(),
                    kill_timeout_seconds: kill_timeout,
                },
            )),
        };
        let o = Outstanding {
            node_id: node_id.to_string(),
            command_id,
            msg,
            is_start: false,
            sent_at: None,
            resent: false,
        };
        self.queue(workload_id, o);
    }

    fn queue(&mut self, workload_id: &str, o: Outstanding) {
        // A newer command for the same workload supersedes the older one.
        self.outstanding.insert(workload_id.to_string(), o);
        self.to_send.push(workload_id.to_string());
    }

    pub(crate) fn has_outstanding_stop(&self, workload_id: &str) -> bool {
        self.outstanding
            .get(workload_id)
            .is_some_and(|o| !o.is_start)
    }

    /// Send an outstanding command if its node has an open stream. Commands
    /// for disconnected nodes wait and are flushed on the next Register.
    fn transmit(&mut self, workload_id: &str, now: Instant) {
        let Some(o) = self.outstanding.get_mut(workload_id) else {
            return;
        };
        let Some((_, tx)) = self.nodes.get(&o.node_id).and_then(|n| n.conn.as_ref()) else {
            return;
        };
        match tx.try_send(Ok(o.msg.clone())) {
            Ok(()) => o.sent_at = Some(now),
            Err(e) => {
                tracing::warn!(node = %o.node_id, workload = workload_id, "agent stream busy, will retry: {e}")
            }
        }
    }

    /// Re-send unacknowledged commands once after the command timeout.
    pub(crate) fn retry_commands(&mut self) {
        let timeout = Duration::from_secs(self.cfg.command_timeout_seconds);
        let now = Instant::now();
        let mut resend = Vec::new();
        let mut give_up = Vec::new();
        for (wid, o) in &self.outstanding {
            match o.sent_at {
                Some(t) if now.duration_since(t) >= timeout => {
                    if o.resent {
                        give_up.push(wid.clone())
                    } else {
                        resend.push(wid.clone())
                    }
                }
                // Never sent (stream was busy or absent): try again.
                None => resend.push(wid.clone()),
                _ => {}
            }
        }
        for wid in give_up {
            tracing::warn!(workload = %wid, "command unacknowledged after retry; leaving it to reconcile");
            self.outstanding.remove(&wid);
        }
        for wid in resend {
            if let Some(o) = self.outstanding.get_mut(&wid) {
                if o.sent_at.is_some() {
                    o.resent = true;
                }
            }
            self.transmit(&wid, now);
        }
    }

    /// Re-send every unresolved command for a node after it reconnects.
    pub(crate) fn resend_for_node(&mut self, node_id: &str) {
        let ids: Vec<_> = self
            .outstanding
            .iter()
            .filter(|(_, o)| o.node_id == node_id)
            .map(|(k, _)| k.clone())
            .collect();
        self.to_send.extend(ids);
    }

    // ------------------------------------------------------------------
    // Shared helpers
    // ------------------------------------------------------------------

    /// Capacity snapshot for the scheduler.
    pub(crate) fn snapshot(&self) -> Snapshot {
        self.snapshot_excluding(&HashSet::new())
    }

    /// Snapshot as if the `excluded` workloads were already gone.
    pub(crate) fn snapshot_excluding(&self, excluded: &HashSet<String>) -> Snapshot {
        let mut snap = Snapshot::default();
        for e in self.nodes.values() {
            let n = &e.node;
            snap.nodes.insert(
                n.node_id.clone(),
                NodeCapacity::new(
                    &n.node_id,
                    &n.node_class,
                    n.status,
                    n.draining,
                    n.allocatable_cpu_millicores,
                    n.allocatable_memory_bytes,
                    &n.host_volumes,
                )
                .with_features(&e.features),
            );
        }
        for w in self.workloads.values() {
            let Some(node_id) = &w.node_id else { continue };
            if w.status.is_terminal()
                || w.status == WorkloadStatus::Lost
                || excluded.contains(&w.workload_id)
            {
                continue;
            }
            if let Some(svc) = self.services.get(&w.service_id) {
                snap.reserve(node_id, &svc.spec, w.holds_reservation());
            }
        }
        snap
    }

    pub(crate) fn new_workload(&self, service: &Service, replaces: Option<String>) -> Workload {
        let now = now_ms();
        Workload {
            workload_id: uuid::Uuid::now_v7().to_string(),
            service_id: service.service_id.clone(),
            service_version: service.version,
            node_id: None,
            container_id: None,
            desired_state: DesiredState::Run,
            status: WorkloadStatus::Pending,
            status_reason: None,
            replaces_workload_id: replaces,
            restart_count: 0,
            created_at: now,
            started_at: None,
            updated_at: now,
            health: Default::default(),
            healthy_since: None,
        }
    }

    /// Request a workload to stop. PENDING ones never reached a node and stop
    /// immediately; placed ones go STOPPING and get `StopWorkload`.
    pub(crate) fn stop_workload(&mut self, workload_id: &str, reason: &str) {
        let Some(w) = self.wl(workload_id) else {
            return;
        };
        w.desired_state = DesiredState::Stop;
        match w.status {
            WorkloadStatus::Pending => {
                w.status = WorkloadStatus::Stopped;
                w.status_reason = Some(reason.to_string());
            }
            WorkloadStatus::Assigned | WorkloadStatus::Running => {
                w.status = WorkloadStatus::Stopping;
                w.status_reason = Some(reason.to_string());
                let node = w.node_id.clone();
                self.assigned_at.remove(workload_id);
                if let Some(node) = node {
                    self.queue_stop(workload_id, &node);
                }
            }
            WorkloadStatus::Lost => {
                // The copy may still run behind a partition; FR-CON-03 stops
                // it when the node registers again.
                w.status_reason = Some(reason.to_string());
            }
            WorkloadStatus::Stopping | WorkloadStatus::Stopped | WorkloadStatus::Failed => {}
        }
    }

    /// Persist in-memory heartbeat times (FR-CON-01: every 30 s).
    pub(crate) fn flush_heartbeats(&mut self) {
        let now_wall = now_ms();
        let now = Instant::now();
        for (id, e) in &mut self.nodes {
            if e.node.status == valkvork_common::models::NodeStatus::Healthy {
                let at = now_wall - now.duration_since(e.last_seen).as_millis() as i64;
                e.node.last_heartbeat_at = at;
                self.heartbeats.push((id.clone(), at));
            }
        }
    }

    fn prune_history(&mut self) {
        match self.db.prune_history(now_ms()) {
            Ok(ids) => {
                for id in &ids {
                    self.workloads.remove(id);
                }
                if !ids.is_empty() {
                    tracing::info!(count = ids.len(), "pruned terminal workload history");
                }
            }
            Err(e) => tracing::warn!(error = %e, "history pruning failed"),
        }
    }
}
