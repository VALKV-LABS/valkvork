//! Reconcile pass (FR-CON-05) with rolling updates (FR-CON-07), drains
//! (FR-CON-08), start timeouts (FR-CON-09) and deletes (FR-CON-10).

use super::Core;
use crate::model::Workload;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use valkvork_common::models::{
    now_ms, DesiredState, EventKind, RolloutStatus, ServiceStatus, SubjectType, WorkloadStatus,
};

const CONTROLLER: &str = "controller";
/// FR-CON-05: after this many consecutive failures the service is DEGRADED.
const DEGRADED_AFTER: u32 = 5;

impl Core {
    /// One full pass over every service. Cheap at the NFR-04 scale, so it
    /// runs after every state change as well as on the periodic tick.
    pub(crate) fn reconcile(&mut self) {
        self.enforce_start_timeouts();
        self.reissue_stops();

        let ids: Vec<String> = self.services.keys().cloned().collect();
        for id in &ids {
            if self
                .services
                .get(id)
                .is_some_and(|s| s.status == ServiceStatus::Deleting)
            {
                self.reconcile_delete(id);
            } else if self.services.get(id).is_some_and(|s| s.spec.is_batch()) {
                self.reconcile_batch(id);
            } else {
                self.reconcile_migrations(id);
                self.reconcile_count(id);
            }
        }
        self.schedule_pending();
    }

    fn reconcile_delete(&mut self, service_id: &str) {
        let ids: Vec<_> = self
            .workloads
            .values()
            .filter(|w| w.service_id == service_id && !w.status.is_terminal())
            .map(|w| (w.workload_id.clone(), w.status))
            .collect();
        for (wid, status) in &ids {
            if *status == WorkloadStatus::Lost {
                // Its node is gone; if the copy resurfaces it is an orphan and
                // gets stopped then (FR-CON-03).
                if let Some(w) = self.wl(wid) {
                    w.desired_state = DesiredState::Stop;
                    w.status = WorkloadStatus::Stopped;
                    w.status_reason = Some("SERVICE_DELETED".into());
                }
            } else {
                self.stop_workload(wid, "SERVICE_DELETED");
            }
        }
        let done = self
            .workloads
            .values()
            .all(|w| w.service_id != service_id || w.status.is_terminal());
        if done {
            self.remove_service(service_id);
            self.event(
                EventKind::Deploy,
                SubjectType::Service,
                service_id,
                format!("service {service_id} deleted"),
                CONTROLLER,
            );
        }
    }

    /// ASSIGNED workloads must reach RUNNING within `start_timeout`.
    fn enforce_start_timeouts(&mut self) {
        let timeout = Duration::from_secs(self.cfg.start_timeout_seconds);
        let now = Instant::now();
        let expired: Vec<String> = self
            .assigned_at
            .iter()
            .filter(|(_, t)| now.duration_since(**t) >= timeout)
            .map(|(id, _)| id.clone())
            .collect();
        for wid in expired {
            self.assigned_at.remove(&wid);
            if self
                .workloads
                .get(&wid)
                .is_some_and(|w| w.status == WorkloadStatus::Assigned)
            {
                self.fail_workload(&wid, "START_TIMEOUT");
            }
        }
    }

    /// STOPPING workloads on a connected node whose stop was never acked
    /// (rejected, or the retry gave up) get another `StopWorkload`.
    fn reissue_stops(&mut self) {
        let stuck: Vec<(String, String)> = self
            .workloads
            .values()
            .filter(|w| {
                w.status == WorkloadStatus::Stopping
                    && !self.outstanding.contains_key(&w.workload_id)
            })
            .filter_map(|w| {
                let node = w.node_id.as_ref()?;
                let connected = self.nodes.get(node).is_some_and(|n| n.conn.is_some());
                connected.then(|| (w.workload_id.clone(), node.clone()))
            })
            .collect();
        for (wid, node) in stuck {
            self.queue_stop(&wid, &node);
        }
    }

    /// Rolling update (version bump) and drain both migrate workloads with
    /// start-new-then-stop-old, `max_parallel` at a time. Services with a
    /// single-writer volume use stop-first instead.
    fn reconcile_migrations(&mut self, service_id: &str) {
        let Some(svc) = self.services.get(service_id) else {
            return;
        };
        if svc.spec.stop_first() {
            self.reconcile_migrations_stop_first(service_id);
            return;
        }
        let (version, max_parallel, min_healthy, rollout) = (
            svc.version,
            svc.spec.update.max_parallel.max(1) as usize,
            svc.spec.update.min_healthy_time_seconds as i64,
            svc.rollout_status,
        );
        let checked = svc.spec.check.is_some();
        let draining: HashSet<&str> = self
            .nodes
            .values()
            .filter(|n| n.node.draining)
            .map(|n| n.node.node_id.as_str())
            .collect();

        let live: Vec<&Workload> = self
            .workloads
            .values()
            .filter(|w| w.service_id == service_id && w.is_live())
            .collect();
        let live_ids: HashSet<&str> = live.iter().map(|w| w.workload_id.as_str()).collect();
        let outdated = |w: &Workload| w.service_version < version;
        let needs_move = |w: &Workload| {
            outdated(w) || w.node_id.as_deref().is_some_and(|n| draining.contains(n))
        };

        let any_outdated = live.iter().any(|w| outdated(w));
        let now = now_ms();

        // Replacement (new) -> the live workload it replaces.
        let mut replacing: HashMap<&str, &Workload> = HashMap::new();
        for w in &live {
            if let Some(old) = w
                .replaces_workload_id
                .as_deref()
                .filter(|o| live_ids.contains(o))
            {
                replacing.insert(old, w);
            }
        }

        // A replacement whose original no longer needs to move (the node was
        // undrained) is cancelled; otherwise the service would run one extra
        // replica for good.
        let cancelled: Vec<String> = live
            .iter()
            .filter(|old| !needs_move(old))
            .filter_map(|old| {
                replacing
                    .get(old.workload_id.as_str())
                    .map(|n| n.workload_id.clone())
            })
            .collect();

        // A replacement that can only be placed once its original is gone (a
        // static host port, or a volume, on the only eligible node) would
        // wait for ever: the original is stopped first instead, as in a
        // stop-first update. Checked against a snapshot without it.
        let spec = svc.spec.clone();
        let blocked_by = |old: &str| -> bool {
            if self.snapshot().place(&spec).is_ok() {
                return false;
            }
            let mut without = HashSet::new();
            without.insert(old.to_string());
            self.snapshot_excluding(&without).place(&spec).is_ok()
        };
        let mut stop_first_ids: Vec<String> = Vec::new();
        let mut to_stop = Vec::new();
        let mut in_flight = 0usize;
        let mut need_replacement = Vec::new();
        let halted = rollout == RolloutStatus::Halted;
        // Oldest first, so a rollout proceeds in a stable order.
        let mut movers: Vec<&&Workload> = live.iter().filter(|w| needs_move(w)).collect();
        movers.sort_by_key(|w| (w.created_at, w.workload_id.clone()));
        for old in movers {
            match replacing.get(old.workload_id.as_str()) {
                Some(new) => {
                    let healthy = settled(new, checked, now, min_healthy);
                    if healthy {
                        to_stop.push(old.workload_id.clone());
                    } else {
                        if new.status == WorkloadStatus::Pending && blocked_by(&old.workload_id) {
                            stop_first_ids.push(old.workload_id.clone());
                        }
                        in_flight += 1;
                    }
                }
                None => {
                    // During a halted rollout outdated workloads stay put; a
                    // drain still proceeds.
                    let draining_only = !outdated(old);
                    if !halted || draining_only {
                        need_replacement.push(old.workload_id.clone());
                    }
                }
            }
        }

        let svc = self
            .services
            .get(service_id)
            .cloned()
            .expect("service exists");
        let budget = max_parallel.saturating_sub(in_flight);
        let backoff = self.replacement_blocked(service_id);
        for old in need_replacement
            .into_iter()
            .take(if backoff { 0 } else { budget })
        {
            let w = self.new_workload(&svc, Some(old));
            self.insert_workload(w);
        }
        for wid in cancelled {
            self.stop_workload(&wid, "MIGRATION_CANCELLED");
        }
        for old in stop_first_ids {
            let first = self
                .svc_rt
                .entry(service_id.to_string())
                .or_default()
                .move_blocked
                .insert(format!("stop-first:{old}"));
            if first {
                self.event(
                    EventKind::Deploy,
                    SubjectType::Workload,
                    &old,
                    format!("{service_id}: its replacement can only run where {old} is (a static host port or a volume on the only eligible node), so {old} is stopped first"),
                    CONTROLLER,
                );
            }
            let reason = if self
                .workloads
                .get(&old)
                .is_some_and(|w| w.service_version < version)
            {
                "ROLLED_OUT"
            } else {
                "DRAINED"
            };
            self.stop_workload(&old, reason);
        }
        for old in to_stop {
            self.stop_workload(
                &old,
                if self
                    .workloads
                    .get(&old)
                    .is_some_and(|w| w.service_version < version)
                {
                    "ROLLED_OUT"
                } else {
                    "DRAINED"
                },
            );
        }

        self.set_rollout(service_id, version, rollout, any_outdated);
    }

    /// Stop-first migration for services with a single-writer volume: the
    /// old workload is stopped and its replacement created at once; the
    /// replacement stays PENDING until the old container is gone and the
    /// volume is free, then starts (usually on the same node). One
    /// replacement per `max_parallel` is coming up at any time. A workload
    /// is only stopped if its replacement could be placed once it is gone,
    /// so a drain never stops a workload that has nowhere else to run.
    fn reconcile_migrations_stop_first(&mut self, service_id: &str) {
        let Some(svc) = self.services.get(service_id).cloned() else {
            return;
        };
        let version = svc.version;
        let max_parallel = svc.spec.update.max_parallel.max(1) as usize;
        let min_healthy = svc.spec.update.min_healthy_time_seconds as i64;
        let rollout = svc.rollout_status;
        let halted = rollout == RolloutStatus::Halted;
        let now = now_ms();

        let draining: HashSet<&str> = self
            .nodes
            .values()
            .filter(|n| n.node.draining)
            .map(|n| n.node.node_id.as_str())
            .collect();
        let live: Vec<&Workload> = self
            .workloads
            .values()
            .filter(|w| w.service_id == service_id && w.is_live())
            .collect();
        let checked = svc.spec.check.is_some();
        let healthy = |w: &Workload| settled(w, checked, now, min_healthy);
        let outdated = |w: &Workload| w.service_version < version;
        let any_outdated = live.iter().any(|w| outdated(w));
        // Workloads still coming up hold the parallelism budget.
        let in_flight = live.iter().filter(|w| !healthy(w)).count();
        let mut movers: Vec<(i64, String, bool)> = live
            .iter()
            .filter(|w| outdated(w) || w.node_id.as_deref().is_some_and(|n| draining.contains(n)))
            // During a halted rollout outdated workloads stay put; a drain
            // still proceeds.
            .filter(|w| !(halted && outdated(w)))
            .map(|w| (w.created_at, w.workload_id.clone(), outdated(w)))
            .collect();
        movers.sort();
        let mover_ids: HashSet<String> = movers.iter().map(|m| m.1.clone()).collect();
        if let Some(rt) = self.svc_rt.get_mut(service_id) {
            rt.move_blocked.retain(|id| mover_ids.contains(id));
        }

        let budget = if self.replacement_blocked(service_id) {
            0
        } else {
            max_parallel.saturating_sub(in_flight)
        };
        let mut moving: Vec<(String, bool)> = Vec::new();
        let mut excluded = HashSet::new();
        for (_, old, is_outdated) in movers {
            if moving.len() >= budget {
                break;
            }
            // Would the replacements fit once these old workloads are gone?
            excluded.insert(old.clone());
            let mut snap = self.snapshot_excluding(&excluded);
            let mut fits = Ok(String::new());
            for _ in 0..=moving.len() {
                fits = snap.place(&svc.spec);
                match &fits {
                    Ok(node) => snap.reserve(node, &svc.spec, true),
                    Err(_) => break,
                }
            }
            match fits {
                Ok(_) => moving.push((old, is_outdated)),
                Err(reason) => {
                    excluded.remove(&old);
                    let first = self
                        .svc_rt
                        .entry(service_id.to_string())
                        .or_default()
                        .move_blocked
                        .insert(old.clone());
                    if first {
                        self.event(
                            EventKind::Workload,
                            SubjectType::Workload,
                            &old,
                            format!("{service_id}: not stopping {old} (single-writer volume) because its replacement could not be placed ({reason}); it keeps running"),
                            CONTROLLER,
                        );
                    }
                }
            }
        }
        for (old, is_outdated) in moving {
            self.stop_workload(&old, if is_outdated { "ROLLED_OUT" } else { "DRAINED" });
            let w = self.new_workload(&svc, Some(old));
            self.insert_workload(w);
        }

        // The rollout lasts until the last replacement is healthy, not just
        // until the last old workload is stopped.
        let rolling = any_outdated || (rollout != RolloutStatus::None && in_flight > 0);
        self.set_rollout(service_id, version, rollout, rolling);
    }

    fn set_rollout(
        &mut self,
        service_id: &str,
        version: u64,
        rollout: RolloutStatus,
        any_outdated: bool,
    ) {
        match (any_outdated, rollout) {
            (true, RolloutStatus::None) => {
                self.services.get_mut(service_id).unwrap().rollout_status =
                    RolloutStatus::InProgress;
                self.touch_service(service_id);
            }
            (false, RolloutStatus::InProgress | RolloutStatus::Halted) => {
                self.services.get_mut(service_id).unwrap().rollout_status = RolloutStatus::None;
                self.touch_service(service_id);
                self.event(
                    EventKind::Deploy,
                    SubjectType::Service,
                    service_id,
                    format!("{service_id} v{version} rollout complete"),
                    CONTROLLER,
                );
            }
            _ => {}
        }
    }

    /// A batch service runs `target_replicas` workloads of its current
    /// version to completion. COMPLETED ones count as done and are never
    /// replaced; FAILED ones are replaced under the failure backoff. A new
    /// version (a changed manifest, or a re-apply after the run finished)
    /// supersedes a run still in progress.
    fn reconcile_batch(&mut self, service_id: &str) {
        let Some(svc) = self.services.get(service_id).cloned() else {
            return;
        };
        let version = svc.version;
        let mine = |w: &&Workload| w.service_id == service_id;
        let superseded: Vec<String> = self
            .workloads
            .values()
            .filter(mine)
            .filter(|w| w.is_live() && w.service_version < version)
            .map(|w| w.workload_id.clone())
            .collect();
        for wid in superseded {
            self.stop_workload(&wid, "SUPERSEDED");
        }
        let mut live: Vec<(i64, String)> = self
            .workloads
            .values()
            .filter(mine)
            .filter(|w| w.is_live() && w.service_version == version)
            .map(|w| (w.created_at, w.workload_id.clone()))
            .collect();
        let done = self
            .workloads
            .values()
            .filter(mine)
            .filter(|w| w.status == WorkloadStatus::Completed && w.service_version == version)
            .count();
        let target = svc.target_replicas as usize;
        let have = live.len() + done;
        if have < target {
            if self.replacement_blocked(service_id) {
                return;
            }
            for _ in have..target {
                let w = self.new_workload(&svc, None);
                self.insert_workload(w);
            }
        } else if have > target {
            // Scaled down mid-run: stop the newest still running.
            live.sort();
            let excess = (have - target).min(live.len());
            for (_, wid) in live.into_iter().rev().take(excess) {
                self.stop_workload(&wid, "SCALED_DOWN");
            }
        }
    }

    /// Bring the number of replica slots to `target_replicas`. A slot is a
    /// live workload that is not an in-flight replacement for another live
    /// workload (so rollouts and drains temporarily run target + n).
    fn reconcile_count(&mut self, service_id: &str) {
        let Some(svc) = self.services.get(service_id).cloned() else {
            return;
        };
        let live: Vec<&Workload> = self
            .workloads
            .values()
            .filter(|w| w.service_id == service_id && w.is_live())
            .collect();
        let live_ids: HashSet<&str> = live.iter().map(|w| w.workload_id.as_str()).collect();
        let slots: Vec<&Workload> = live
            .iter()
            .copied()
            .filter(|w| {
                !w.replaces_workload_id
                    .as_deref()
                    .is_some_and(|o| live_ids.contains(o))
            })
            .collect();
        let target = svc.target_replicas as usize;

        if slots.len() < target {
            if self.replacement_blocked(service_id) {
                return;
            }
            for _ in slots.len()..target {
                let w = self.new_workload(&svc, None);
                self.insert_workload(w);
            }
        } else if slots.len() > target {
            // Excess order: PENDING, then ASSIGNED, then RUNNING on the
            // most-allocated node, newest first (FR-CON-05).
            let snap = self.snapshot();
            let rank = |w: &Workload| match w.status {
                WorkloadStatus::Pending => 0,
                WorkloadStatus::Assigned => 1,
                _ => 2,
            };
            let ratio = |w: &Workload| {
                w.node_id
                    .as_ref()
                    .and_then(|n| snap.nodes.get(n))
                    .map(|n| n.alloc_ratio())
                    .unwrap_or(0.0)
            };
            let mut ordered: Vec<&Workload> = slots.clone();
            ordered.sort_by(|a, b| {
                rank(a)
                    .cmp(&rank(b))
                    .then(
                        ratio(b)
                            .partial_cmp(&ratio(a))
                            .unwrap_or(std::cmp::Ordering::Equal),
                    )
                    .then(Reverse(a.created_at).cmp(&Reverse(b.created_at)))
            });
            let excess: Vec<String> = ordered
                .iter()
                .take(slots.len() - target)
                .map(|w| w.workload_id.clone())
                .collect();
            for wid in excess {
                // Stopping a slot that is mid-migration also cancels its replacement's role.
                self.stop_workload(&wid, "SCALED_DOWN");
            }
        }
    }

    /// Place every PENDING workload (FR-CON-06). Placements within the pass
    /// account for each other through the shared snapshot.
    fn schedule_pending(&mut self) {
        let mut pending: Vec<(i64, String, String)> = self
            .workloads
            .values()
            .filter(|w| w.status == WorkloadStatus::Pending && w.desired_state == DesiredState::Run)
            .map(|w| (w.created_at, w.workload_id.clone(), w.service_id.clone()))
            .collect();
        if pending.is_empty() {
            return;
        }
        pending.sort();
        let mut snap = self.snapshot();
        let now = Instant::now();
        for (_, wid, sid) in pending {
            let Some(spec) = self.services.get(&sid).map(|s| s.spec.clone()) else {
                continue;
            };
            let started = Instant::now();
            let placed = snap.place(&spec);
            self.metrics.observe_schedule(started.elapsed());
            match placed {
                Ok(node_id) => {
                    snap.reserve(&node_id, &spec, true);
                    if let Some(w) = self.wl(&wid) {
                        w.status = WorkloadStatus::Assigned;
                        w.node_id = Some(node_id.clone());
                        w.status_reason = None;
                    }
                    self.assigned_at.insert(wid.clone(), now);
                    self.queue_start(&wid);
                    tracing::info!(workload = %wid, service = %sid, node = %node_id, "workload assigned");
                }
                Err(reason) => {
                    let changed = self
                        .workloads
                        .get(&wid)
                        .is_some_and(|w| w.status_reason.as_deref() != Some(reason.as_str()));
                    if changed {
                        if let Some(w) = self.wl(&wid) {
                            w.status_reason = Some(reason);
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Failure backoff and DEGRADED (FR-CON-05), rollout halt (FR-CON-07)
    // ------------------------------------------------------------------

    pub(crate) fn record_failure(&mut self, service_id: &str, version: u64) {
        let Some(svc) = self.services.get(service_id) else {
            return;
        };
        if svc.version != version {
            return; // only failures of the current version count
        }
        let rollout_in_progress = svc.rollout_status == RolloutStatus::InProgress;
        let rt = self.svc_rt.entry(service_id.to_string()).or_default();
        rt.fail_streak += 1;
        rt.last_failure = Some(Instant::now());
        let streak = rt.fail_streak;

        if rollout_in_progress {
            self.services.get_mut(service_id).unwrap().rollout_status = RolloutStatus::Halted;
            self.touch_service(service_id);
            self.event(EventKind::Deploy, SubjectType::Service, service_id, format!("{service_id} v{version} rollout HALTED: new workload failed; old workloads keep running"), CONTROLLER);
        }
        if streak >= DEGRADED_AFTER
            && self
                .services
                .get(service_id)
                .is_some_and(|s| s.status == ServiceStatus::Active)
        {
            self.services.get_mut(service_id).unwrap().status = ServiceStatus::Degraded;
            self.touch_service(service_id);
            self.event(
                EventKind::Workload,
                SubjectType::Service,
                service_id,
                format!("{service_id} DEGRADED after {streak} consecutive failures"),
                CONTROLLER,
            );
        }
    }

    pub(crate) fn clear_failures(&mut self, service_id: &str) {
        if let Some(rt) = self.svc_rt.get_mut(service_id) {
            rt.fail_streak = 0;
            rt.last_failure = None;
        }
        if self
            .services
            .get(service_id)
            .is_some_and(|s| s.status == ServiceStatus::Degraded)
        {
            self.services.get_mut(service_id).unwrap().status = ServiceStatus::Active;
            self.touch_service(service_id);
            self.event(
                EventKind::Workload,
                SubjectType::Service,
                service_id,
                format!("{service_id} recovered (ACTIVE)"),
                CONTROLLER,
            );
        }
    }

    /// Exponential replacement backoff: 5 s, 10 s, 20 s … capped at 5 min.
    pub(crate) fn replacement_blocked(&self, service_id: &str) -> bool {
        let Some(rt) = self.svc_rt.get(service_id) else {
            return false;
        };
        let (Some(last), streak) = (rt.last_failure, rt.fail_streak) else {
            return false;
        };
        if streak == 0 {
            return false;
        }
        let delay = Duration::from_secs((5u64 << (streak - 1).min(6)).min(300));
        last.elapsed() < delay
    }
}

/// A new workload counts as healthy for rollouts and drains once it has been
/// RUNNING for `min_healthy` seconds, or, with a health check, has been
/// passing it for that long.
fn settled(w: &Workload, checked: bool, now: i64, min_healthy: i64) -> bool {
    let since = if checked {
        w.healthy_since
            .filter(|_| w.health == crate::model::Health::Healthy)
    } else {
        w.started_at
    };
    w.status == WorkloadStatus::Running && since.is_some_and(|t| now - t >= min_healthy * 1000)
}
