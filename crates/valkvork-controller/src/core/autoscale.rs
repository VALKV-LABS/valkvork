//! CPU-based horizontal autoscaler (FR-AUT-01 … FR-AUT-05).

use super::Core;
use crate::db::NewScalingEvent;
use std::time::{Duration, Instant};
use valkvork_common::models::{now_ms, EventKind, RolloutStatus, SubjectType, WorkloadStatus};

/// Utilisation band around the target that causes no action (FR-AUT-02).
const UP_FACTOR: f64 = 1.1;
const DOWN_FACTOR: f64 = 0.9;
/// Workloads younger than this are excluded (startup spikes).
const MIN_AGE_MS: i64 = 30_000;

impl Core {
    /// Mean utilisation (%) over a service's eligible RUNNING workloads, or
    /// `None` when fewer than half of them have samples (FR-AUT-04).
    pub(crate) fn service_utilisation(&self, service_id: &str) -> Option<f64> {
        let svc = self.services.get(service_id)?;
        let limit = svc.spec.cpu_millicores.max(1) as f64;
        let now = now_ms();
        let running: Vec<_> = self
            .workloads
            .values()
            .filter(|w| {
                w.service_id == service_id && w.status == WorkloadStatus::Running && w.is_live()
            })
            .filter(|w| w.service_version == svc.version)
            .collect();
        if running.is_empty() {
            return None;
        }
        let samples: Vec<f64> = running
            .iter()
            .filter(|w| w.started_at.is_some_and(|t| now - t >= MIN_AGE_MS))
            .filter_map(|w| {
                let s = self.cpu_samples.get(&w.workload_id)?;
                if s.is_empty() {
                    return None;
                }
                let avg = s.iter().map(|(_, m)| *m as f64).sum::<f64>() / s.len() as f64;
                Some(avg / limit * 100.0)
            })
            .collect();
        if samples.len() * 2 < running.len() {
            return None;
        }
        Some(samples.iter().sum::<f64>() / samples.len() as f64)
    }

    pub(crate) fn autoscale(&mut self) {
        if self.in_grace() {
            return;
        }
        let sustain = Duration::from_secs(self.cfg.autoscaler.sustain_seconds);
        let cooldown_ms = (self.cfg.autoscaler.cooldown_seconds * 1000) as i64;
        let now = Instant::now();
        let ids: Vec<String> = self.services.keys().cloned().collect();

        for id in ids {
            let Some(svc) = self.services.get(&id) else {
                continue;
            };
            let Some(a) = svc.spec.autoscale.clone() else {
                continue;
            };
            if svc.rollout_status == RolloutStatus::InProgress
                || svc.status == valkvork_common::models::ServiceStatus::Deleting
            {
                self.reset_autoscale_window(&id);
                continue;
            }
            let current = svc.target_replicas;
            let in_cooldown = svc
                .last_scaled_at
                .is_some_and(|t| now_ms() - t < cooldown_ms);
            let unplaced = self.workloads.values().any(|w| {
                w.service_id == id
                    && w.status == WorkloadStatus::Pending
                    && w.desired_state == valkvork_common::models::DesiredState::Run
            });

            let Some(util) = self.service_utilisation(&id) else {
                // Missing data never triggers a scale-down.
                self.reset_autoscale_window(&id);
                continue;
            };
            let target = a.target_cpu_percent;
            let rt = self.svc_rt.entry(id.clone()).or_default();
            let (above, below) = (util > target * UP_FACTOR, util < target * DOWN_FACTOR);
            if above {
                rt.below_since = None;
                rt.above_since.get_or_insert(now);
            } else if below {
                rt.above_since = None;
                rt.below_since.get_or_insert(now);
            } else {
                rt.above_since = None;
                rt.below_since = None;
                continue;
            }
            if in_cooldown {
                continue;
            }

            let ratio_target = (current as f64 * util / target).ceil() as u32;
            let decision = if rt
                .above_since
                .is_some_and(|t| now.duration_since(t) >= sustain)
            {
                if unplaced {
                    continue; // capacity shortage: an infrastructure signal instead
                }
                Some((
                    "up",
                    ratio_target.min(a.max_replicas).max(current),
                    target * UP_FACTOR,
                ))
            } else if rt
                .below_since
                .is_some_and(|t| now.duration_since(t) >= sustain)
            {
                let half = current.div_ceil(2);
                Some((
                    "down",
                    ratio_target.max(half).max(a.min_replicas).min(current),
                    target * DOWN_FACTOR,
                ))
            } else {
                None
            };
            let Some((direction, desired, line)) = decision else {
                continue;
            };
            rt.above_since = None;
            rt.below_since = None;
            if desired == current {
                continue;
            }

            let reason = format!(
                "cpu {util:.1}% {} {line:.1}% for {}s",
                if direction == "up" { ">" } else { "<" },
                sustain.as_secs()
            );
            let svc = self.services.get_mut(&id).unwrap();
            svc.target_replicas = desired;
            svc.last_scaled_at = Some(now_ms());
            self.touch_service(&id);
            self.scaling_event(NewScalingEvent {
                service_id: id.clone(),
                from_replicas: current,
                to_replicas: desired,
                metric_value: util,
                reason: reason.clone(),
            });
            self.event(
                EventKind::Scale,
                SubjectType::Service,
                &id,
                format!("{id} {current} → {desired} ({reason})"),
                "autoscaler",
            );
        }
    }

    fn reset_autoscale_window(&mut self, id: &str) {
        if let Some(rt) = self.svc_rt.get_mut(id) {
            rt.above_since = None;
            rt.below_since = None;
        }
    }
}
