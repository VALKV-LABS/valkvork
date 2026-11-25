//! Block volumes in the core (docs/block-volumes.md, stage 2).
//!
//! The core decides; the volume manager acts. A workload with block volumes
//! is placed as usual (ASSIGNED), but its `StartWorkload` waits until every
//! one of its volumes is ATTACHED to its node. To get there the core moves
//! each volume through its states, writing each state to SQLite before it
//! sends the manager an intent, and reacts to the manager's results.
//!
//! Invariants:
//! - One intent in flight per volume: one is sent only on entering a
//!   transitional state (CREATING, ATTACHING, DETACHING, FENCING).
//! - A volume attached to node A is never attached to node B until A has
//!   let go: A unmounted it and it was detached, or A is gone and it was
//!   force-detached (fenced). Never on a timeout alone.

use super::Core;
use crate::model::{NodeEntry, Volume, VolumeState};
use crate::volumes::{
    IntentSender, VolumeDriver, VolumeIntent, VolumeOp, VolumeOutcome, VolumeReply,
};
use std::sync::Arc;
use valkvork_common::models::{now_ms, EventKind, NodeStatus, SubjectType, WorkloadStatus};

const CONTROLLER: &str = "controller";

/// After a failed detach or fence, the next try waits this long.
pub(crate) const VOLUME_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// The volume driver and the manager's intent queue.
pub(crate) struct VolumeSystem {
    pub driver: Arc<dyn VolumeDriver>,
    pub tx: IntentSender,
}

/// Why a workload cannot get its volumes (it FAILS with this reason).
fn attach_failed(cause: impl std::fmt::Display) -> String {
    format!("VOLUME_ATTACH_FAILED: {cause}")
}

impl Core {
    /// Use `driver` through the manager behind `tx`, and resume every
    /// volume that was mid-transition when the controller stopped.
    pub fn enable_volumes(&mut self, driver: Arc<dyn VolumeDriver>, tx: IntentSender) {
        self.volume_system = Some(VolumeSystem { driver, tx });
        let resume: Vec<Volume> = self
            .volumes
            .values()
            .filter(|v| {
                matches!(
                    v.state,
                    VolumeState::Creating
                        | VolumeState::Attaching
                        | VolumeState::Detaching
                        | VolumeState::Fencing
                )
            })
            .cloned()
            .collect();
        for v in resume {
            tracing::info!(volume = %v.name, state = v.state.as_str(), "resuming volume operation");
            self.send_intent_for(&v);
        }
    }

    /// The block volume names `wid`'s spec uses.
    fn block_volumes_of(&self, wid: &str) -> Vec<String> {
        self.workloads
            .get(wid)
            .and_then(|w| self.services.get(&w.service_id))
            .map(|s| {
                s.spec
                    .volumes
                    .iter()
                    .filter(|v| v.is_block())
                    .map(|v| v.source.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Gate for `StartWorkload`: true when every block volume of `wid` is
    /// attached to its node. Otherwise drives them there, records `wid` as
    /// waiting, and returns false; `start_volume_waiters` sends the start
    /// once they are. May fail the workload.
    pub(crate) fn volumes_ready(&mut self, wid: &str) -> bool {
        let names = self.block_volumes_of(wid);
        if names.is_empty() {
            return true;
        }
        let Some((node_id, service_id)) = self
            .workloads
            .get(wid)
            .and_then(|w| w.node_id.clone().map(|n| (n, w.service_id.clone())))
        else {
            return false;
        };
        if self.volume_system.is_none() {
            self.fail_volume_waiter(
                wid,
                &attach_failed("this controller has no [block_volumes] driver"),
            );
            return false;
        }
        let Some((zone, instance)) = self
            .nodes
            .get(&node_id)
            .map(|e| (e.node.zone.clone(), e.node.instance_id.clone()))
        else {
            return false;
        };
        let mut ready = true;
        for name in names {
            let spec = self
                .services
                .get(&service_id)
                .and_then(|s| {
                    s.spec
                        .volumes
                        .iter()
                        .find(|v| v.is_block() && v.source == name)
                })
                .cloned();
            let Some(spec) = spec else { continue };
            let current = self.volumes.get(&name).cloned();
            match current {
                None => {
                    let now = now_ms();
                    let v = Volume {
                        name: name.clone(),
                        size_bytes: spec.size_bytes.unwrap_or(0),
                        class: spec.class.clone().unwrap_or_else(|| "gp3".into()),
                        driver: self.volume_system.as_ref().unwrap().driver.name().into(),
                        cloud_id: None,
                        zone: Some(zone.clone()),
                        state: VolumeState::Creating,
                        node_id: None,
                        error: None,
                        created_by: service_id.clone(),
                        created_at: now,
                        updated_at: now,
                    };
                    self.event(
                        EventKind::Workload,
                        SubjectType::Service,
                        &service_id,
                        format!(
                            "creating block volume {name} ({} GiB {}) in {zone}",
                            v.size_bytes >> 30,
                            v.class
                        ),
                        CONTROLLER,
                    );
                    self.set_volume(v.clone());
                    self.send_intent_for(&v);
                    ready = false;
                }
                Some(v) => match (v.state, v.node_id.as_deref()) {
                    (VolumeState::Attached, Some(n)) if n == node_id => {}
                    (VolumeState::Available, _) => {
                        if v.zone.as_deref().is_some_and(|z| z != zone) {
                            self.fail_volume_waiter(
                                wid,
                                &attach_failed(format!(
                                    "block volume {name} is in zone {}, the node in {zone}",
                                    v.zone.as_deref().unwrap_or("?")
                                )),
                            );
                            return false;
                        }
                        if instance.is_empty() {
                            self.fail_volume_waiter(
                                wid,
                                &attach_failed(format!("node {node_id} reports no instance id")),
                            );
                            return false;
                        }
                        let mut v = v.clone();
                        v.state = VolumeState::Attaching;
                        v.node_id = Some(node_id.clone());
                        v.error = None;
                        self.set_volume(v.clone());
                        self.send_intent_for(&v);
                        ready = false;
                    }
                    (VolumeState::Attached, Some(other)) => {
                        let other = other.to_string();
                        self.release_volume(&name, &other);
                        ready = false;
                    }
                    (VolumeState::Failed | VolumeState::Deleting, _) => {
                        let why = v
                            .error
                            .clone()
                            .unwrap_or_else(|| v.state.as_str().to_lowercase());
                        self.fail_volume_waiter(
                            wid,
                            &attach_failed(format!("block volume {name}: {why}")),
                        );
                        return false;
                    }
                    // In flight: wait for the manager.
                    _ => ready = false,
                },
            }
        }
        if !ready {
            self.volume_waiters.insert(wid.to_string());
        }
        ready
    }

    /// Get `name` off node `holder` so another node can attach it: a normal
    /// detach once the holder has unmounted it, fencing if the holder is
    /// gone or UNHEALTHY. Waits (does nothing) while the holder still uses it.
    fn release_volume(&mut self, name: &str, holder: &str) {
        if self
            .volume_retry_at
            .get(name)
            .is_some_and(|t| std::time::Instant::now() < *t)
        {
            return;
        }
        let entry: Option<&NodeEntry> = self.nodes.get(holder);
        let fence = entry.is_none_or(|e| e.node.status == NodeStatus::Unhealthy);
        if !fence {
            let e = entry.unwrap();
            let in_use = e.block_in_use.contains(name)
                || self.workloads.values().any(|w| {
                    w.node_id.as_deref() == Some(holder)
                        && !w.status.is_terminal()
                        && w.status != WorkloadStatus::Lost
                        && self.services.get(&w.service_id).is_some_and(|s| {
                            s.spec
                                .volumes
                                .iter()
                                .any(|v| v.is_block() && v.source == name)
                        })
                });
            if in_use {
                return; // the old workload is still stopping
            }
        }
        let Some(mut v) = self.volumes.get(name).cloned() else {
            return;
        };
        v.state = if fence {
            VolumeState::Fencing
        } else {
            VolumeState::Detaching
        };
        self.event(
            EventKind::Node,
            SubjectType::Node,
            holder,
            if fence {
                format!("fencing: force-detaching block volume {name} from this node, which is gone or unreachable")
            } else {
                format!("detaching block volume {name}, for a workload on another node")
            },
            CONTROLLER,
        );
        self.set_volume(v.clone());
        self.send_intent_for(&v);
    }

    /// The intent for a volume in a transitional state.
    fn send_intent_for(&mut self, v: &Volume) {
        let Some(sys) = &self.volume_system else {
            return;
        };
        let instance = |core: &Core| {
            v.node_id
                .as_ref()
                .and_then(|n| core.nodes.get(n))
                .map(|e| e.node.instance_id.clone())
                .unwrap_or_default()
        };
        let cloud_id = v.cloud_id.clone().unwrap_or_default();
        let op = match v.state {
            VolumeState::Creating => VolumeOp::Create {
                size_bytes: v.size_bytes,
                class: v.class.clone(),
                zone: v.zone.clone().unwrap_or_default(),
            },
            VolumeState::Attaching => VolumeOp::Attach {
                cloud_id,
                instance_id: instance(self),
            },
            VolumeState::Detaching => VolumeOp::Detach {
                cloud_id,
                instance_id: instance(self),
            },
            VolumeState::Fencing => VolumeOp::Fence {
                cloud_id,
                instance_id: instance(self),
            },
            _ => return,
        };
        let _ = sys.tx.send(VolumeIntent {
            name: v.name.clone(),
            op,
        });
    }

    /// The manager's result for `name`.
    pub(crate) fn on_volume_result(&mut self, name: &str, result: VolumeReply) {
        let Some(mut v) = self.volumes.get(name).cloned() else {
            return;
        };
        let was = v.state;
        match (was, result) {
            (VolumeState::Creating, Ok(VolumeOutcome::Created { cloud_id, zone })) => {
                v.cloud_id = Some(cloud_id);
                if !zone.is_empty() {
                    v.zone = Some(zone);
                }
                v.state = VolumeState::Available;
                v.error = None;
                self.set_volume(v);
            }
            (VolumeState::Creating, Err(e)) => {
                // Nothing usable exists: forget the row (a retry creates it
                // again, and drivers find a half-created one by name).
                self.remove_volume(name);
                self.fail_waiters_of(
                    name,
                    &attach_failed(format!("creating block volume {name}: {e}")),
                );
            }
            (VolumeState::Attaching, Ok(VolumeOutcome::Attached)) => {
                v.state = VolumeState::Attached;
                self.set_volume(v);
            }
            (VolumeState::Attaching, Err(e)) => {
                v.state = VolumeState::Available;
                v.node_id = None;
                v.error = Some(e.clone());
                self.set_volume(v);
                self.fail_waiters_of(
                    name,
                    &attach_failed(format!("attaching block volume {name}: {e}")),
                );
            }
            (VolumeState::Detaching | VolumeState::Fencing, Ok(VolumeOutcome::Detached)) => {
                self.volume_retry_at.remove(name);
                let from = v.node_id.take().unwrap_or_default();
                v.state = VolumeState::Available;
                v.error = None;
                self.set_volume(v);
                if was == VolumeState::Fencing {
                    self.event(
                        EventKind::Node,
                        SubjectType::Node,
                        &from,
                        format!("block volume {name} force-detached: this node can no longer write to it"),
                        CONTROLLER,
                    );
                }
            }
            (VolumeState::Detaching | VolumeState::Fencing, Err(e)) => {
                // Still attached to the old node, as far as we know: never
                // attach elsewhere. The next pass tries again.
                v.state = VolumeState::Attached;
                v.error = Some(e);
                self.set_volume(v);
                self.volume_retry_at
                    .insert(name.to_string(), std::time::Instant::now() + VOLUME_RETRY);
            }
            (state, r) => {
                tracing::warn!(volume = name, state = state.as_str(), result = ?r, "unexpected volume result ignored");
                return;
            }
        }
        self.start_volume_waiters();
    }

    /// Send the starts whose volumes are now ready; retry the waits (a
    /// detach waiting for an unmount, a failed fence). Called after every
    /// volume result and each reconcile pass.
    pub(crate) fn start_volume_waiters(&mut self) {
        let waiting: Vec<String> = self.volume_waiters.iter().cloned().collect();
        for wid in waiting {
            let still = self
                .workloads
                .get(&wid)
                .is_some_and(|w| w.status == WorkloadStatus::Assigned);
            if !still {
                self.volume_waiters.remove(&wid);
                continue;
            }
            if self.volumes_ready(&wid) {
                self.volume_waiters.remove(&wid);
                self.queue_start(&wid);
            }
        }
    }

    fn fail_volume_waiter(&mut self, wid: &str, reason: &str) {
        self.volume_waiters.remove(wid);
        self.assigned_at.remove(wid);
        self.fail_workload(wid, reason);
    }

    fn fail_waiters_of(&mut self, name: &str, reason: &str) {
        let hit: Vec<String> = self
            .volume_waiters
            .iter()
            .filter(|w| self.block_volumes_of(w).iter().any(|n| n == name))
            .cloned()
            .collect();
        for wid in hit {
            self.fail_volume_waiter(&wid, reason);
        }
    }

    /// Persist first, then act (docs/block-volumes.md).
    fn set_volume(&mut self, mut v: Volume) {
        v.updated_at = now_ms();
        if let Err(e) = self.db.save_volume(&v) {
            tracing::error!(volume = %v.name, error = %e, "cannot save the volume");
        }
        self.volumes.insert(v.name.clone(), v);
    }

    fn remove_volume(&mut self, name: &str) {
        if let Err(e) = self.db.delete_volume(name) {
            tracing::error!(volume = name, error = %e, "cannot delete the volume row");
        }
        self.volumes.remove(name);
    }

    /// The mount source the agent uses for a block volume (`local` driver),
    /// and its cloud id.
    pub(crate) fn block_mount(&self, name: &str) -> (String, String) {
        let Some(v) = self.volumes.get(name) else {
            return (String::new(), String::new());
        };
        let cloud_id = v.cloud_id.clone().unwrap_or_default();
        let local = self
            .volume_system
            .as_ref()
            .and_then(|s| s.driver.local_path(&cloud_id))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        (cloud_id, local)
    }
}
