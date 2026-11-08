//! Admin operations: one internal service layer shared by the gRPC
//! `AdminService` (and, later, the web UI per FR-UI-05).

use super::Core;
use crate::model::Service;
use std::collections::HashSet;
use tokio::sync::oneshot;
use valkvork_common::errors::{ValidationError, ValidationErrorCode};
use valkvork_common::models::{
    now_ms, ChangeKind, DesiredState, EventKind, NodeStatus, RolloutStatus, ServiceStatus,
    SubjectType, WorkloadStatus,
};
use valkvork_common::validation::parse_manifest_str;
use valkvork_common::ValkvorkError;
use valkvork_proto::v1 as pb;

type Reply<T> = oneshot::Sender<Result<T, ValkvorkError>>;

#[derive(Debug, Default, Clone)]
pub struct WorkloadFilter {
    pub service_id: Option<String>,
    pub node_id: Option<String>,
    pub status: Option<String>,
}

pub enum AdminOp {
    Apply {
        manifest: String,
        dry_run: bool,
        actor: String,
        reply: Reply<ApplyOutcome>,
    },
    Delete {
        service_id: String,
        actor: String,
        reply: Reply<()>,
    },
    Scale {
        service_id: String,
        replicas: u32,
        actor: String,
        reply: Reply<(u32, u32)>,
    },
    SetDraining {
        node_id: String,
        draining: bool,
        actor: String,
        reply: Reply<()>,
    },
    /// Forget an UNHEALTHY node; returns how many of its LOST workloads
    /// were marked STOPPED.
    RemoveNode {
        node_id: String,
        actor: String,
        reply: Reply<u32>,
    },
    GetService {
        service_id: String,
        reply: Reply<pb::ServiceDetail>,
    },
    ListServices {
        service_id: Option<String>,
        reply: Reply<Vec<pb::ServiceSummary>>,
    },
    ListWorkloads {
        filter: WorkloadFilter,
        reply: Reply<Vec<pb::WorkloadSummary>>,
    },
    ListNodes {
        node_id: Option<String>,
        node_class: Option<String>,
        reply: Reply<Vec<pb::NodeSummary>>,
    },
    /// UI overview page (FR-UI-03).
    Overview { reply: Reply<super::Overview> },
    /// Last 60 one-minute CPU samples of a service (FR-UI-07).
    ServiceSeries {
        service_id: String,
        reply: Reply<Vec<super::SeriesPoint>>,
    },
    /// Prometheus exposition text (NFR-07).
    Metrics { reply: Reply<String> },
}

#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub service_id: String,
    pub version: u64,
    pub change: ChangeKind,
    pub warnings: Vec<String>,
    /// Plan only.
    pub diff: Vec<String>,
    pub to_place: u32,
    pub placeable: u32,
    pub target_replicas: u32,
    pub max_parallel: u32,
}

impl Core {
    pub(crate) fn on_admin(&mut self, op: AdminOp) {
        match op {
            AdminOp::Apply {
                manifest,
                dry_run,
                actor,
                reply,
            } => {
                let res = self.apply(&manifest, dry_run, &actor);
                if dry_run || res.is_err() {
                    let _ = reply.send(res);
                } else {
                    self.reconcile();
                    self.after_commit(move || {
                        let _ = reply.send(res);
                    });
                }
            }
            AdminOp::Delete {
                service_id,
                actor,
                reply,
            } => {
                let res = self.delete_service(&service_id, &actor);
                self.reconcile();
                self.after_commit(move || {
                    let _ = reply.send(res);
                });
            }
            AdminOp::Scale {
                service_id,
                replicas,
                actor,
                reply,
            } => {
                let res = self.scale_service(&service_id, replicas, &actor);
                self.reconcile();
                self.after_commit(move || {
                    let _ = reply.send(res);
                });
            }
            AdminOp::SetDraining {
                node_id,
                draining,
                actor,
                reply,
            } => {
                let res = self.set_draining(&node_id, draining, &actor);
                self.reconcile();
                self.after_commit(move || {
                    let _ = reply.send(res);
                });
            }
            AdminOp::RemoveNode {
                node_id,
                actor,
                reply,
            } => {
                let res = self.remove_node(&node_id, &actor);
                self.reconcile();
                self.after_commit(move || {
                    let _ = reply.send(res);
                });
            }
            AdminOp::GetService { service_id, reply } => {
                let _ = reply.send(self.service_detail(&service_id));
            }
            AdminOp::ListServices { service_id, reply } => {
                let _ = reply.send(Ok(self.service_summaries(service_id.as_deref())));
            }
            AdminOp::ListWorkloads { filter, reply } => {
                let _ = reply.send(Ok(self.workload_summaries(&filter)));
            }
            AdminOp::ListNodes {
                node_id,
                node_class,
                reply,
            } => {
                let _ = reply.send(Ok(
                    self.node_summaries(node_id.as_deref(), node_class.as_deref())
                ));
            }
            AdminOp::Overview { reply } => {
                let _ = reply.send(Ok(self.overview()));
            }
            AdminOp::ServiceSeries { service_id, reply } => {
                let res = if self.services.contains_key(&service_id) {
                    Ok(self.service_series(&service_id))
                } else {
                    Err(ValkvorkError::NotFound(format!("service {service_id}")))
                };
                let _ = reply.send(res);
            }
            AdminOp::Metrics { reply } => {
                let _ = reply.send(Ok(self.render_metrics()));
            }
        }
    }

    // ------------------------------------------------------------------
    // Apply / plan (FR-MAN-02, FR-MAN-03, FR-MAN-04)
    // ------------------------------------------------------------------

    pub(crate) fn apply(
        &mut self,
        manifest: &str,
        dry_run: bool,
        actor: &str,
    ) -> Result<ApplyOutcome, ValkvorkError> {
        let parsed = parse_manifest_str(manifest).map_err(ValkvorkError::Validation)?;
        let spec = parsed.spec;
        let mut warnings: Vec<String> = parsed.warnings.iter().map(|w| w.to_string()).collect();
        warnings.extend(self.unsatisfiable_warning(&spec).map(|w| w.to_string()));
        let hash = spec.spec_hash();
        let id = spec.id.clone();

        let Some(existing) = self.services.get(&id).cloned() else {
            // Create at version 1.
            let target = match &spec.autoscale {
                Some(a) => spec.replicas.clamp(a.min_replicas, a.max_replicas),
                None => spec.replicas,
            };
            let (to_place, placeable) = self.feasibility(&spec, target, &HashSet::new());
            let outcome = ApplyOutcome {
                service_id: id.clone(),
                version: 1,
                change: ChangeKind::Created,
                warnings,
                diff: vec![
                    format!("image: {}", spec.image),
                    format!("replicas: {target}"),
                ],
                to_place,
                placeable,
                target_replicas: target,
                max_parallel: spec.update.max_parallel,
            };
            if dry_run {
                return Ok(outcome);
            }
            let now = now_ms();
            let svc = Service {
                service_id: id.clone(),
                version: 1,
                spec,
                spec_hash: hash,
                target_replicas: target,
                status: ServiceStatus::Active,
                rollout_status: RolloutStatus::None,
                last_scaled_at: None,
                created_at: now,
                updated_at: now,
            };
            self.services.insert(id.clone(), svc);
            self.svc_rt.insert(id.clone(), Default::default());
            self.touch_service(&id);
            self.event(
                EventKind::Deploy,
                SubjectType::Service,
                &id,
                format!("{id} created at v1 with {target} replicas"),
                actor,
            );
            // FR-UI-06: every operator action is audited, whichever client made it.
            self.event(
                EventKind::Audit,
                SubjectType::Service,
                &id,
                format!("apply service {id}: created"),
                actor,
            );
            return Ok(outcome);
        };

        if existing.status == ServiceStatus::Deleting {
            return Err(ValkvorkError::FailedPrecondition(format!(
                "service {id} is being deleted"
            )));
        }

        let autoscaled = spec.autoscale.is_some();
        let new_target = match &spec.autoscale {
            // FR-MAN-04: never reset to `replicas`, only clamp.
            Some(a) => existing
                .target_replicas
                .clamp(a.min_replicas, a.max_replicas),
            None => spec.replicas,
        };
        let mut change = existing.spec.classify_change(&spec);
        if change == ChangeKind::Unchanged && new_target != existing.target_replicas {
            change = ChangeKind::ScaleOnly; // manual-scale drift is restored
        }
        if change == ChangeKind::ScaleOnly && autoscaled && new_target == existing.target_replicas {
            change = ChangeKind::Unchanged; // `replicas` is ignored once autoscaled
        }
        // A finished batch run applied again runs again, as a new version.
        if change == ChangeKind::Unchanged
            && spec.is_batch()
            && !self
                .workloads
                .values()
                .any(|w| w.service_id == id && w.is_live())
        {
            change = ChangeKind::Rerun;
        }
        let halted = existing.rollout_status == RolloutStatus::Halted;
        let version = if matches!(change, ChangeKind::Replacing | ChangeKind::Rerun) {
            existing.version + 1
        } else {
            existing.version
        };

        let mut diff = existing.spec.diff_lines(&spec);
        if change == ChangeKind::Rerun {
            diff.push("batch run: the previous run has finished; this runs it again".into());
        }
        if new_target != existing.target_replicas && !diff.iter().any(|d| d.starts_with("replicas"))
        {
            diff.push(format!(
                "replicas: {} → {} (restoring manifest value)",
                existing.target_replicas, new_target
            ));
        }
        let live = self
            .workloads
            .values()
            .filter(|w| w.service_id == id && w.is_live())
            .count() as u32;
        let to_place = match change {
            ChangeKind::Replacing => live.max(new_target),
            ChangeKind::Rerun => new_target,
            _ => new_target.saturating_sub(live),
        };
        // A stop-first rollout frees the old workloads' volumes and ports
        // before placing their replacements.
        let freed: HashSet<String> = if change == ChangeKind::Replacing && spec.stop_first() {
            self.workloads
                .values()
                .filter(|w| w.service_id == id && w.is_live())
                .map(|w| w.workload_id.clone())
                .collect()
        } else {
            HashSet::new()
        };
        let (to_place, placeable) = self.feasibility(&spec, to_place, &freed);
        let outcome = ApplyOutcome {
            service_id: id.clone(),
            version,
            change,
            warnings,
            diff,
            to_place,
            placeable,
            target_replicas: new_target,
            max_parallel: spec.update.max_parallel,
        };

        let nothing_to_write = hash == existing.spec_hash
            && new_target == existing.target_replicas
            && !halted
            && change != ChangeKind::Rerun;
        if dry_run || nothing_to_write {
            // AT-01: an identical re-apply writes nothing and sends nothing.
            return Ok(outcome);
        }

        let svc = self.services.get_mut(&id).unwrap();
        svc.spec = spec;
        svc.spec_hash = hash;
        svc.version = version;
        svc.target_replicas = new_target;
        if halted {
            // FR-CON-07: the next apply resumes or supersedes a halted rollout.
            svc.rollout_status = RolloutStatus::None;
        }
        if svc.status == ServiceStatus::Degraded {
            svc.status = ServiceStatus::Active;
        }
        self.touch_service(&id);
        if let Some(rt) = self.svc_rt.get_mut(&id) {
            rt.fail_streak = 0;
            rt.last_failure = None;
        }
        let msg = match change {
            ChangeKind::Replacing => {
                format!("{id} v{} → v{version} (rolling update)", existing.version)
            }
            ChangeKind::ScaleOnly => {
                format!("{id} replicas {} → {new_target}", existing.target_replicas)
            }
            ChangeKind::PolicyOnly => format!("{id} policy updated"),
            ChangeKind::Rerun => {
                format!("{id} v{} → v{version} (batch: run again)", existing.version)
            }
            _ => format!("{id} re-applied"),
        };
        self.event(EventKind::Deploy, SubjectType::Service, &id, msg, actor);
        self.event(
            EventKind::Audit,
            SubjectType::Service,
            &id,
            format!("apply service {id}: {change}"),
            actor,
        );
        Ok(outcome)
    }

    /// Simulate placing `count` workloads of `spec` (plan feasibility).
    fn feasibility(
        &self,
        spec: &valkvork_common::models::ServiceSpec,
        count: u32,
        freed: &HashSet<String>,
    ) -> (u32, u32) {
        let mut snap = self.snapshot_excluding(freed);
        let mut placed = 0;
        for _ in 0..count {
            match snap.place(spec) {
                Ok(n) => {
                    snap.reserve(&n, spec, true);
                    placed += 1;
                }
                Err(_) => break,
            }
        }
        (count, placed)
    }

    /// VW-01: no current node of the class could fit even one replica, or no
    /// node has a required host volume.
    fn unsatisfiable_warning(
        &self,
        spec: &valkvork_common::models::ServiceSpec,
    ) -> Option<ValidationError> {
        let candidates: Vec<_> = self
            .nodes
            .values()
            .map(|e| &e.node)
            .filter(|n| n.status == NodeStatus::Healthy)
            .filter(|n| {
                spec.required_class
                    .as_ref()
                    .is_none_or(|c| &n.node_class == c)
            })
            .collect();
        let fits = candidates.iter().any(|n| {
            n.allocatable_cpu_millicores >= spec.cpu_millicores
                && n.allocatable_memory_bytes >= spec.memory_bytes
                && spec
                    .volumes
                    .iter()
                    .all(|v| !v.is_host() || n.host_volumes.contains(&v.source))
        });
        (!fits).then(|| {
            let class = spec.required_class.as_deref().unwrap_or("any");
            ValidationError::warning(
                ValidationErrorCode::UnsatisfiableNow,
                "resources",
                &format!("no current healthy node (class {class}) can fit one replica or provide its host volumes"),
            )
        })
    }

    // ------------------------------------------------------------------
    // Delete / scale / drain
    // ------------------------------------------------------------------

    fn delete_service(&mut self, id: &str, actor: &str) -> Result<(), ValkvorkError> {
        let svc = self
            .services
            .get_mut(id)
            .ok_or_else(|| ValkvorkError::NotFound(format!("service {id}")))?;
        if svc.status != ServiceStatus::Deleting {
            svc.status = ServiceStatus::Deleting;
            self.touch_service(id);
            self.event(
                EventKind::Audit,
                SubjectType::Service,
                id,
                format!("delete service {id}"),
                actor,
            );
        }
        Ok(())
    }

    fn scale_service(
        &mut self,
        id: &str,
        replicas: u32,
        actor: &str,
    ) -> Result<(u32, u32), ValkvorkError> {
        let svc = self
            .services
            .get_mut(id)
            .ok_or_else(|| ValkvorkError::NotFound(format!("service {id}")))?;
        if svc.status == ServiceStatus::Deleting {
            return Err(ValkvorkError::FailedPrecondition(format!(
                "service {id} is being deleted"
            )));
        }
        let old = svc.target_replicas;
        let new = match &svc.spec.autoscale {
            Some(a) => replicas.clamp(a.min_replicas, a.max_replicas),
            None => replicas,
        };
        if new != old {
            svc.target_replicas = new;
            self.touch_service(id);
            let clamped = if new != replicas {
                format!(" (clamped from {replicas})")
            } else {
                String::new()
            };
            self.event(
                EventKind::Audit,
                SubjectType::Service,
                id,
                format!("scale {id} {old} → {new}{clamped}"),
                actor,
            );
        }
        Ok((old, new))
    }

    fn set_draining(
        &mut self,
        node_id: &str,
        draining: bool,
        actor: &str,
    ) -> Result<(), ValkvorkError> {
        let e = self
            .nodes
            .get_mut(node_id)
            .ok_or_else(|| ValkvorkError::NotFound(format!("node {node_id}")))?;
        if e.node.draining != draining {
            e.node.draining = draining;
            let host = e.node.hostname.clone();
            self.touch_node(node_id);
            let verb = if draining { "drain" } else { "undrain" };
            self.event(
                EventKind::Audit,
                SubjectType::Node,
                node_id,
                format!("{verb} node {host}"),
                actor,
            );
        }
        Ok(())
    }

    /// Forget a node that is gone for good, e.g. a VM its auto-scaling group
    /// replaced. Its workloads were already replaced when it went UNHEALTHY;
    /// the LOST copies become STOPPED so they are pruned like any history. If
    /// the node ever registers again it joins as a new node, and the
    /// inventory rules (FR-CON-03) stop any copies it still runs.
    fn remove_node(&mut self, node_id: &str, actor: &str) -> Result<u32, ValkvorkError> {
        let e = self
            .nodes
            .get(node_id)
            .ok_or_else(|| ValkvorkError::NotFound(format!("node {node_id}")))?;
        let host = e.node.hostname.clone();
        if e.node.status != NodeStatus::Unhealthy {
            return Err(ValkvorkError::FailedPrecondition(format!(
                "node {host} is {}; only an UNHEALTHY node can be removed (drain it, then stop its agent or terminate the VM)",
                e.node.status
            )));
        }
        let on_node: Vec<_> = self
            .workloads
            .values()
            .filter(|w| w.node_id.as_deref() == Some(node_id))
            .map(|w| (w.workload_id.clone(), w.status.is_terminal()))
            .collect();
        let mut stopped = 0;
        for (wid, terminal) in on_node {
            if terminal {
                // History: the database clears node_id on delete; mirror it
                // without touching updated_at, which drives pruning.
                if let Some(w) = self.workloads.get_mut(&wid) {
                    w.node_id = None;
                }
                continue;
            }
            if let Some(w) = self.wl(&wid) {
                w.node_id = None;
                w.desired_state = DesiredState::Stop;
                w.status = WorkloadStatus::Stopped;
                w.status_reason = Some("NODE_REMOVED".into());
                stopped += 1;
            }
            self.assigned_at.remove(&wid);
            self.cpu_samples.remove(&wid);
        }
        self.remove_node_entry(node_id);
        self.event(
            EventKind::Audit,
            SubjectType::Node,
            node_id,
            format!("remove node {host}, {stopped} lost workloads stopped"),
            actor,
        );
        Ok(stopped)
    }

    // ------------------------------------------------------------------
    // Views
    // ------------------------------------------------------------------

    fn workload_summary(w: &crate::model::Workload) -> pb::WorkloadSummary {
        pb::WorkloadSummary {
            workload_id: w.workload_id.clone(),
            node_id: w.node_id.clone().unwrap_or_default(),
            status: w.status.to_string(),
            status_reason: w.status_reason.clone().unwrap_or_default(),
            restart_count: w.restart_count,
            created_at: w.created_at as u64,
            started_at: w.started_at.unwrap_or(0) as u64,
            updated_at: w.updated_at as u64,
            service_id: w.service_id.clone(),
            service_version: w.service_version,
            // Only live workloads have a current health; a stopped one keeps
            // its last report otherwise.
            health: if w.is_live() {
                w.health.as_str().to_string()
            } else {
                String::new()
            },
        }
    }

    fn count(&self, service_id: &str, status: WorkloadStatus) -> u32 {
        self.workloads
            .values()
            .filter(|w| {
                w.service_id == service_id
                    && w.status == status
                    && (status.is_terminal() || w.is_live())
            })
            .count() as u32
    }

    fn service_detail(&self, id: &str) -> Result<pb::ServiceDetail, ValkvorkError> {
        let s = self
            .services
            .get(id)
            .ok_or_else(|| ValkvorkError::NotFound(format!("service {id}")))?;
        let mut workloads: Vec<_> = self
            .workloads
            .values()
            .filter(|w| w.service_id == id)
            .collect();
        workloads.sort_by_key(|w| std::cmp::Reverse(w.created_at));
        Ok(pb::ServiceDetail {
            service_id: s.service_id.clone(),
            version: s.version,
            spec_toml: spec_to_toml(&s.spec),
            status: s.status.to_string(),
            rollout_status: s.rollout_status.to_string(),
            target_replicas: s.target_replicas,
            running_replicas: self.count(id, WorkloadStatus::Running),
            pending_replicas: self.count(id, WorkloadStatus::Pending),
            failed_replicas: self.count(id, WorkloadStatus::Failed),
            workloads: workloads.into_iter().map(Self::workload_summary).collect(),
            scaling_events: vec![], // filled in by the adapter from the read connection
            created_at: s.created_at as u64,
            updated_at: s.updated_at as u64,
        })
    }

    fn service_summaries(&self, filter: Option<&str>) -> Vec<pb::ServiceSummary> {
        self.services
            .values()
            .filter(|s| filter.is_none_or(|f| s.service_id == f))
            .map(|s| pb::ServiceSummary {
                service_id: s.service_id.clone(),
                version: s.version,
                image: s.spec.image.clone(),
                status: s.status.to_string(),
                target_replicas: s.target_replicas,
                running_replicas: self.count(&s.service_id, WorkloadStatus::Running),
                cpu_utilization: self.service_utilisation(&s.service_id).unwrap_or(0.0),
                created_at: s.created_at as u64,
                rollout_status: s.rollout_status.to_string(),
            })
            .collect()
    }

    fn workload_summaries(&self, f: &WorkloadFilter) -> Vec<pb::WorkloadSummary> {
        let mut out: Vec<_> = self
            .workloads
            .values()
            .filter(|w| f.service_id.as_ref().is_none_or(|s| &w.service_id == s))
            .filter(|w| {
                f.node_id
                    .as_ref()
                    .is_none_or(|n| w.node_id.as_ref() == Some(n))
            })
            .filter(|w| {
                f.status
                    .as_ref()
                    .is_none_or(|s| w.status.as_str().eq_ignore_ascii_case(s))
            })
            .collect();
        out.sort_by_key(|w| (w.service_id.clone(), std::cmp::Reverse(w.created_at)));
        out.into_iter().map(Self::workload_summary).collect()
    }

    fn node_summaries(&self, node_id: Option<&str>, class: Option<&str>) -> Vec<pb::NodeSummary> {
        let snap = self.snapshot();
        self.nodes
            .values()
            .filter(|e| node_id.is_none_or(|id| e.node.node_id == id))
            .filter(|e| class.is_none_or(|c| e.node.node_class == c))
            .map(|e| {
                let n = &e.node;
                let cap = snap.nodes.get(&n.node_id);
                let last = if n.status == NodeStatus::Healthy {
                    now_ms() - e.last_seen.elapsed().as_millis() as i64
                } else {
                    n.last_heartbeat_at
                };
                pb::NodeSummary {
                    node_id: n.node_id.clone(),
                    hostname: n.hostname.clone(),
                    ip_address: n.ip_address.clone(),
                    node_class: n.node_class.clone(),
                    status: n.status.to_string(),
                    draining: n.draining,
                    allocatable_cpu_millicores: n.allocatable_cpu_millicores,
                    allocatable_memory_bytes: n.allocatable_memory_bytes,
                    used_cpu_millicores: cap.map(|c| c.alloc_cpu as u32).unwrap_or(0),
                    used_memory_bytes: cap.map(|c| c.alloc_mem).unwrap_or(0),
                    workload_count: self
                        .workloads
                        .values()
                        .filter(|w| {
                            w.node_id.as_deref() == Some(&n.node_id) && !w.status.is_terminal()
                        })
                        .count() as u32,
                    last_heartbeat_at: last as u64,
                    agent_version: n.agent_version.clone(),
                    runtime: n.runtime.clone(),
                    host_volumes: n.host_volumes.clone(),
                }
            })
            .collect()
    }
}

/// Render the canonical spec back as a manifest (service detail view).
pub fn spec_to_toml(s: &valkvork_common::models::ServiceSpec) -> String {
    use std::fmt::Write;
    let mut o = String::new();
    let _ = writeln!(
        o,
        "[service]\nid = {:?}\nimage = {:?}\nreplicas = {}\nkill_timeout = \"{}s\"",
        s.id, s.image, s.replicas, s.kill_timeout_seconds
    );
    if s.is_batch() {
        let _ = writeln!(o, "type = \"batch\"");
    }
    for (key, list) in [("entrypoint", &s.entrypoint), ("command", &s.command)] {
        if let Some(l) = list {
            let _ = writeln!(o, "{key} = {}", toml_list(l));
        }
    }
    o.push('\n');
    let _ = writeln!(
        o,
        "[resources]\ncpu = {}\nmemory = \"{}\"\ndisk = \"{}\"\n",
        s.cpu_millicores as f64 / 1000.0,
        human_size(s.memory_bytes),
        human_size(s.disk_bytes)
    );
    if let Some(c) = &s.required_class {
        let _ = writeln!(o, "[constraints]\nrequired_class = {c:?}\n");
    }
    if !s.env.is_empty() {
        let _ = writeln!(o, "[env]");
        for (k, v) in &s.env {
            let _ = writeln!(o, "{k} = {v:?}");
        }
        o.push('\n');
    }
    if !s.secrets.is_empty() {
        let _ = writeln!(o, "[secrets]");
        for (k, v) in &s.secrets {
            let _ = writeln!(o, "{k} = {v:?}");
        }
        o.push('\n');
    }
    for p in &s.ports {
        let _ = writeln!(
            o,
            "[[ports]]\nname = {:?}\ncontainer_port = {}\nhost_port = {}",
            p.name, p.container_port, p.host_port
        );
        if let Some(ip) = &p.host_ip {
            let _ = writeln!(o, "host_ip = {ip:?}");
        }
        o.push('\n');
    }
    for v in &s.volumes {
        let _ = writeln!(
            o,
            "[[volumes]]\ntype = {:?}\nsource = {:?}\ntarget = {:?}\nread_only = {}\n{}",
            v.kind,
            v.source,
            v.target,
            v.read_only,
            if v.shared {
                "single_writer = false\n"
            } else {
                ""
            }
        );
    }
    if let Some(a) = &s.autoscale {
        let _ = writeln!(
            o,
            "[autoscale]\nmin_replicas = {}\nmax_replicas = {}\ntarget_cpu_percent = {}\n",
            a.min_replicas, a.max_replicas, a.target_cpu_percent
        );
    }
    if let Some(c) = &s.check {
        let _ = writeln!(o, "[check]\ntype = {:?}\nport = {:?}", c.kind, c.port);
        if let Some(p) = &c.path {
            let _ = writeln!(o, "path = {p:?}");
        }
        let _ = writeln!(
            o,
            "interval = \"{}s\"\ntimeout = \"{}s\"\nfailures = {}\ngrace = \"{}s\"\n",
            c.interval_seconds, c.timeout_seconds, c.failures, c.grace_seconds
        );
    }
    let _ = writeln!(
        o,
        "[update]\nmax_parallel = {}\nmin_healthy_time = \"{}s\"\n",
        s.update.max_parallel, s.update.min_healthy_time_seconds
    );
    let _ = write!(
        o,
        "[restart]\nattempts = {}\ninterval = \"{}s\"\ndelay = \"{}s\"\n",
        s.restart.attempts, s.restart.interval_seconds, s.restart.delay_seconds
    );
    o
}

fn toml_list(l: &[String]) -> String {
    let items: Vec<String> = l.iter().map(|x| format!("{x:?}")).collect();
    format!("[{}]", items.join(", "))
}

fn human_size(b: u64) -> String {
    for (unit, shift) in [("GiB", 30), ("MiB", 20), ("KiB", 10)] {
        if b >= 1 << shift && b.is_multiple_of(1 << shift) {
            return format!("{}{unit}", b >> shift);
        }
    }
    format!("{b}")
}

#[cfg(test)]
mod spec_toml_tests {
    use super::spec_to_toml;
    use valkvork_common::validation::parse_manifest_str;

    /// The detail view's manifest parses back to the same spec.
    #[test]
    fn round_trips() {
        let m = r#"
[service]
id = "pg"
image = "docker.io/library/postgres:16.4-alpine"
replicas = 1
entrypoint = ["docker-entrypoint.sh"]
command = ["postgres", "-c", "wal_level=logical"]

[resources]
cpu = 1.0
memory = "1GiB"

[[ports]]
name = "postgres"
container_port = 5432
host_port = 5432
host_ip = "advertise"

[secrets]
POSTGRES_PASSWORD = "ssm:/valkv/dev/workloads/POSTGRES_PASSWORD"

[check]
type = "tcp"
port = "postgres"
interval = "5s"
grace = "30s"

[[ports]]
name = "admin"
container_port = 9000
host_port = 9000
"#;
        let spec = parse_manifest_str(m).unwrap().spec;
        let again = parse_manifest_str(&spec_to_toml(&spec)).unwrap().spec;
        assert_eq!(spec, again);
    }
}
