//! Observability inside the core: Prometheus metrics (NFR-07), the
//! 60-minute chart ring buffers (FR-UI-07) and the UI overview snapshot.

use super::Core;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write;
use std::time::{Duration, Instant};
use valkvork_common::models::{
    now_ms, DesiredState, NodeStatus, RolloutStatus, ServiceStatus, WorkloadStatus,
};

/// One-minute samples kept for the UI charts.
const SERIES_POINTS: usize = 60;
/// Histogram buckets for `valkvork_schedule_duration_seconds`.
const SCHEDULE_BUCKETS: [f64; 8] = [0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.05, 0.1];

/// Counters that cannot be derived from current state.
#[derive(Debug, Default)]
pub struct CoreMetrics {
    /// (service, direction) -> count
    scaling_actions: BTreeMap<(String, &'static str), u64>,
    /// Recent scaling actions, for the overview's "last hour" figure.
    recent_scaling: VecDeque<Instant>,
    schedule_buckets: [u64; SCHEDULE_BUCKETS.len()],
    schedule_count: u64,
    schedule_sum: f64,
    registrations: u64,
}

impl CoreMetrics {
    pub fn observe_schedule(&mut self, d: Duration) {
        let secs = d.as_secs_f64();
        for (i, b) in SCHEDULE_BUCKETS.iter().enumerate() {
            if secs <= *b {
                self.schedule_buckets[i] += 1;
            }
        }
        self.schedule_count += 1;
        self.schedule_sum += secs;
    }

    pub fn scaling_action(&mut self, service: &str, direction: &'static str) {
        *self
            .scaling_actions
            .entry((service.to_string(), direction))
            .or_default() += 1;
        self.recent_scaling.push_back(Instant::now());
    }

    pub fn registration(&mut self) {
        self.registrations += 1;
    }

    fn scaling_last_hour(&mut self) -> u32 {
        while self
            .recent_scaling
            .front()
            .is_some_and(|t| t.elapsed() > Duration::from_secs(3600))
        {
            self.recent_scaling.pop_front();
        }
        self.recent_scaling.len() as u32
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct SeriesPoint {
    /// Unix ms at the end of the minute.
    pub t: i64,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClassPoint {
    pub t: i64,
    pub cpu: f64,
    pub memory: f64,
}

/// FR-UI-07: in-memory, not persisted.
#[derive(Debug, Default)]
pub struct Series {
    services: HashMap<String, VecDeque<SeriesPoint>>,
    classes: HashMap<String, VecDeque<ClassPoint>>,
}

fn push<T>(q: &mut VecDeque<T>, v: T) {
    q.push_back(v);
    while q.len() > SERIES_POINTS {
        q.pop_front();
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ClassCapacity {
    pub class: String,
    pub nodes: u32,
    pub healthy_nodes: u32,
    pub cpu_allocated: u64,
    pub cpu_allocatable: u64,
    pub memory_allocated: u64,
    pub memory_allocatable: u64,
    pub series: Vec<ClassPoint>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Overview {
    pub uptime_seconds: u64,
    pub recovery_grace_remaining_seconds: u64,
    pub nodes_total: u32,
    pub nodes_healthy: u32,
    pub nodes_unhealthy: u32,
    pub nodes_draining: u32,
    pub workloads: BTreeMap<String, u32>,
    pub unplaced_pending: u32,
    pub services_total: u32,
    pub services_degraded: u32,
    pub rollouts_in_progress: u32,
    pub scaling_actions_last_hour: u32,
    pub classes: Vec<ClassCapacity>,
}

/// Per-class allocation over healthy nodes: (nodes, healthy, cpu alloc, cpu cap, mem alloc, mem cap).
type ClassTotals = BTreeMap<String, (u32, u32, u64, u64, u64, u64)>;

impl Core {
    fn class_totals(&self) -> ClassTotals {
        let snap = self.snapshot();
        let mut out: ClassTotals = BTreeMap::new();
        for e in self.nodes.values() {
            let n = &e.node;
            let t = out.entry(n.node_class.clone()).or_default();
            t.0 += 1;
            if n.status == NodeStatus::Healthy {
                t.1 += 1;
                let cap = snap.nodes.get(&n.node_id);
                t.2 += cap.map(|c| c.alloc_cpu).unwrap_or(0);
                t.3 += n.allocatable_cpu_millicores as u64;
                t.4 += cap.map(|c| c.alloc_mem).unwrap_or(0);
                t.5 += n.allocatable_memory_bytes;
            }
        }
        out
    }

    fn unplaced_by_class(&self) -> BTreeMap<String, u32> {
        let mut out = BTreeMap::new();
        for w in self.workloads.values() {
            if w.status == WorkloadStatus::Pending && w.desired_state == DesiredState::Run {
                let class = self
                    .services
                    .get(&w.service_id)
                    .and_then(|s| s.spec.required_class.clone())
                    .unwrap_or_else(|| "any".into());
                *out.entry(class).or_default() += 1;
            }
        }
        out
    }

    /// Take the one-minute chart samples (called every 60 s).
    pub(crate) fn sample_series(&mut self) {
        let t = now_ms();
        let utils: Vec<(String, Option<f64>)> = self
            .services
            .keys()
            .map(|id| (id.clone(), self.service_utilisation(id)))
            .collect();
        self.series
            .services
            .retain(|id, _| self.services.contains_key(id));
        for (id, util) in utils {
            let q = self.series.services.entry(id).or_default();
            push(
                q,
                SeriesPoint {
                    t,
                    value: util.unwrap_or(0.0),
                },
            );
        }
        let totals = self.class_totals();
        self.series.classes.retain(|c, _| totals.contains_key(c));
        for (class, (_, _, ca, cc, ma, mc)) in totals {
            let ratio = |a: u64, c: u64| if c == 0 { 0.0 } else { a as f64 / c as f64 };
            let q = self.series.classes.entry(class).or_default();
            push(
                q,
                ClassPoint {
                    t,
                    cpu: ratio(ca, cc),
                    memory: ratio(ma, mc),
                },
            );
        }
    }

    pub(crate) fn service_series(&self, id: &str) -> Vec<SeriesPoint> {
        self.series
            .services
            .get(id)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default()
    }

    pub(crate) fn overview(&mut self) -> Overview {
        let mut workloads: BTreeMap<String, u32> = BTreeMap::new();
        for s in [
            "PENDING", "ASSIGNED", "RUNNING", "STOPPING", "LOST", "STOPPED", "FAILED",
        ] {
            workloads.insert(s.into(), 0);
        }
        for w in self.workloads.values() {
            *workloads.entry(w.status.to_string()).or_default() += 1;
        }
        let classes = self
            .class_totals()
            .into_iter()
            .map(|(class, (nodes, healthy, ca, cc, ma, mc))| ClassCapacity {
                series: self
                    .series
                    .classes
                    .get(&class)
                    .map(|q| q.iter().copied().collect())
                    .unwrap_or_default(),
                class,
                nodes,
                healthy_nodes: healthy,
                cpu_allocated: ca,
                cpu_allocatable: cc,
                memory_allocated: ma,
                memory_allocatable: mc,
            })
            .collect();
        let nodes = || self.nodes.values().map(|e| &e.node);
        Overview {
            uptime_seconds: self.started.elapsed().as_secs(),
            recovery_grace_remaining_seconds: self
                .grace_until
                .saturating_duration_since(Instant::now())
                .as_secs(),
            nodes_total: self.nodes.len() as u32,
            nodes_healthy: nodes().filter(|n| n.status == NodeStatus::Healthy).count() as u32,
            nodes_unhealthy: nodes()
                .filter(|n| n.status == NodeStatus::Unhealthy)
                .count() as u32,
            nodes_draining: nodes().filter(|n| n.draining).count() as u32,
            workloads,
            unplaced_pending: self.unplaced_by_class().values().sum(),
            services_total: self.services.len() as u32,
            services_degraded: self
                .services
                .values()
                .filter(|s| s.status == ServiceStatus::Degraded)
                .count() as u32,
            rollouts_in_progress: self
                .services
                .values()
                .filter(|s| s.rollout_status == RolloutStatus::InProgress)
                .count() as u32,
            scaling_actions_last_hour: self.metrics.scaling_last_hour(),
            classes,
        }
    }

    /// Prometheus text exposition (NFR-07).
    pub(crate) fn render_metrics(&self) -> String {
        let mut o = String::new();

        // valkvork_nodes{status,class}
        header(
            &mut o,
            "valkvork_nodes",
            "gauge",
            "Registered nodes by status and class.",
        );
        let mut nodes: BTreeMap<(String, String), u32> = BTreeMap::new();
        for e in self.nodes.values() {
            *nodes
                .entry((e.node.status.to_string(), e.node.node_class.clone()))
                .or_default() += 1;
        }
        for ((status, class), n) in &nodes {
            line(
                &mut o,
                "valkvork_nodes",
                &[("status", status), ("class", class)],
                *n as f64,
            );
        }

        header(
            &mut o,
            "valkvork_nodes_draining",
            "gauge",
            "Draining nodes by class.",
        );
        let mut draining: BTreeMap<String, u32> = BTreeMap::new();
        for e in self.nodes.values() {
            *draining.entry(e.node.node_class.clone()).or_default() += e.node.draining as u32;
        }
        for (class, n) in &draining {
            line(
                &mut o,
                "valkvork_nodes_draining",
                &[("class", class)],
                *n as f64,
            );
        }

        // valkvork_workloads{status}
        header(
            &mut o,
            "valkvork_workloads",
            "gauge",
            "Workloads by status.",
        );
        let mut by_status: BTreeMap<&str, u32> = [
            "PENDING", "ASSIGNED", "RUNNING", "STOPPING", "LOST", "STOPPED", "FAILED",
        ]
        .into_iter()
        .map(|s| (s, 0))
        .collect();
        for w in self.workloads.values() {
            *by_status.entry(w.status.as_str()).or_default() += 1;
        }
        for (status, n) in &by_status {
            line(
                &mut o,
                "valkvork_workloads",
                &[("status", status)],
                *n as f64,
            );
        }

        // valkvork_workload_health{health}: workloads with a health check.
        header(
            &mut o,
            "valkvork_workload_health",
            "gauge",
            "Live workloads with a health check, by reported health.",
        );
        let mut by_health: BTreeMap<&str, u32> = ["STARTING", "HEALTHY", "UNHEALTHY"]
            .into_iter()
            .map(|s| (s, 0))
            .collect();
        for w in self.workloads.values().filter(|w| w.is_live()) {
            if let Some(n) = by_health.get_mut(w.health.as_str()) {
                *n += 1;
            }
        }
        for (health, n) in &by_health {
            line(
                &mut o,
                "valkvork_workload_health",
                &[("health", health)],
                *n as f64,
            );
        }

        // valkvork_pending_unplaced_workloads{class}: drives infrastructure scale-out (§10.4).
        header(
            &mut o,
            "valkvork_pending_unplaced_workloads",
            "gauge",
            "PENDING workloads that could not be placed, by required class (\"any\" when unconstrained).",
        );
        let classes: Vec<String> = self.class_totals().keys().cloned().collect();
        let mut unplaced = self.unplaced_by_class();
        for c in &classes {
            unplaced.entry(c.clone()).or_default();
        }
        for (class, n) in &unplaced {
            line(
                &mut o,
                "valkvork_pending_unplaced_workloads",
                &[("class", class)],
                *n as f64,
            );
        }

        // valkvork_allocated_ratio{class,resource}
        header(
            &mut o,
            "valkvork_allocated_ratio",
            "gauge",
            "Requested over allocatable resources on healthy nodes, by class.",
        );
        for (class, (_, _, ca, cc, ma, mc)) in self.class_totals() {
            let ratio = |a: u64, c: u64| if c == 0 { 0.0 } else { a as f64 / c as f64 };
            line(
                &mut o,
                "valkvork_allocated_ratio",
                &[("class", &class), ("resource", "cpu")],
                ratio(ca, cc),
            );
            line(
                &mut o,
                "valkvork_allocated_ratio",
                &[("class", &class), ("resource", "memory")],
                ratio(ma, mc),
            );
        }

        header(
            &mut o,
            "valkvork_service_cpu_utilization",
            "gauge",
            "Mean CPU utilisation (%) of a service's RUNNING workloads over the last 60 s.",
        );
        for id in self.services.keys() {
            if let Some(u) = self.service_utilisation(id) {
                line(
                    &mut o,
                    "valkvork_service_cpu_utilization",
                    &[("service", id)],
                    u,
                );
            }
        }

        header(
            &mut o,
            "valkvork_service_target_replicas",
            "gauge",
            "Target replicas per service.",
        );
        for (id, s) in &self.services {
            line(
                &mut o,
                "valkvork_service_target_replicas",
                &[("service", id)],
                s.target_replicas as f64,
            );
        }

        header(
            &mut o,
            "valkvork_scaling_actions_total",
            "counter",
            "Autoscaler actions.",
        );
        for ((service, direction), n) in &self.metrics.scaling_actions {
            line(
                &mut o,
                "valkvork_scaling_actions_total",
                &[("service", service), ("direction", direction)],
                *n as f64,
            );
        }

        header(
            &mut o,
            "valkvork_schedule_duration_seconds",
            "histogram",
            "Time to place one workload.",
        );
        for (i, b) in SCHEDULE_BUCKETS.iter().enumerate() {
            line(
                &mut o,
                "valkvork_schedule_duration_seconds_bucket",
                &[("le", &b.to_string())],
                self.metrics.schedule_buckets[i] as f64,
            );
        }
        line(
            &mut o,
            "valkvork_schedule_duration_seconds_bucket",
            &[("le", "+Inf")],
            self.metrics.schedule_count as f64,
        );
        line(
            &mut o,
            "valkvork_schedule_duration_seconds_sum",
            &[],
            self.metrics.schedule_sum,
        );
        line(
            &mut o,
            "valkvork_schedule_duration_seconds_count",
            &[],
            self.metrics.schedule_count as f64,
        );

        header(
            &mut o,
            "valkvork_agent_registrations_total",
            "counter",
            "Accepted agent Register messages.",
        );
        line(
            &mut o,
            "valkvork_agent_registrations_total",
            &[],
            self.metrics.registrations as f64,
        );

        header(
            &mut o,
            "valkvork_agents_connected",
            "gauge",
            "Agents with an open stream.",
        );
        line(
            &mut o,
            "valkvork_agents_connected",
            &[],
            self.nodes.values().filter(|n| n.conn.is_some()).count() as f64,
        );

        header(
            &mut o,
            "valkvork_commands_outstanding",
            "gauge",
            "Commands sent to agents and not yet acknowledged.",
        );
        line(
            &mut o,
            "valkvork_commands_outstanding",
            &[],
            self.outstanding.len() as f64,
        );

        header(
            &mut o,
            "valkvork_controller_recovery_grace",
            "gauge",
            "1 while the recovery grace period is active (FR-CON-04).",
        );
        line(
            &mut o,
            "valkvork_controller_recovery_grace",
            &[],
            self.in_grace() as u8 as f64,
        );

        header(
            &mut o,
            "valkvork_controller_uptime_seconds",
            "gauge",
            "Seconds since the controller started.",
        );
        line(
            &mut o,
            "valkvork_controller_uptime_seconds",
            &[],
            self.started.elapsed().as_secs_f64(),
        );
        o
    }
}

fn header(o: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// One sample line with escaped label values.
pub fn line(o: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    o.push_str(name);
    if !labels.is_empty() {
        o.push('{');
        for (i, (k, v)) in labels.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            let v = v
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n");
            let _ = write!(o, "{k}=\"{v}\"");
        }
        o.push('}');
    }
    let _ = writeln!(o, " {value}");
}
