//! Behaviour tests for the core, driven without any network.

use super::*;
use pb::WorkloadState;
use std::time::Duration;
use valkvork_common::models::{ChangeKind, NodeStatus, RolloutStatus, ServiceStatus};

type Rx = mpsc::Receiver<Result<pb::ControllerMessage, tonic::Status>>;

const TOKEN: &str = "join-token";

fn cfg() -> ControllerConfig {
    ControllerConfig {
        recovery_grace_period_seconds: 0,
        ..Default::default()
    }
}

fn core() -> Core {
    Core::new(cfg(), Db::open_in_memory().unwrap(), TOKEN.into()).unwrap()
}

fn manifest(id: &str, image: &str, replicas: u32, extra: &str) -> String {
    format!(
        "[service]\nid = \"{id}\"\nimage = \"{image}\"\nreplicas = {replicas}\n\n[resources]\ncpu = 0.5\nmemory = \"256MiB\"\n{extra}"
    )
}

struct Agent {
    id: String,
    conn: u64,
    rx: Rx,
}

impl Agent {
    /// Drain everything the controller sent on this stream.
    fn take(&mut self) -> Vec<pb::controller_message::Msg> {
        let mut out = vec![];
        while let Ok(m) = self.rx.try_recv() {
            out.extend(m.unwrap().msg);
        }
        out
    }

    fn starts(&mut self) -> Vec<pb::StartWorkload> {
        self.take()
            .into_iter()
            .filter_map(|m| match m {
                pb::controller_message::Msg::StartWorkload(s) => Some(s),
                _ => None,
            })
            .collect()
    }
}

fn stops(msgs: &[pb::controller_message::Msg]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            pb::controller_message::Msg::StopWorkload(s) => Some(s.workload_id.clone()),
            _ => None,
        })
        .collect()
}

/// Agents support every feature, except nodes whose id starts with "old",
/// which behave like an agent from before the features existed.
fn features_for(id: &str) -> Vec<String> {
    if id.starts_with("old") {
        vec![]
    } else {
        valkvork_common::models::AGENT_FEATURES
            .iter()
            .map(|f| f.to_string())
            .collect()
    }
}

static CONN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn register_with(
    c: &mut Core,
    id: &str,
    class: &str,
    token: &str,
    inventory: Vec<pb::WorkloadStatus>,
) -> Agent {
    register_full(c, id, class, token, inventory, vec![])
}

fn register_full(
    c: &mut Core,
    id: &str,
    class: &str,
    token: &str,
    inventory: Vec<pb::WorkloadStatus>,
    host_volumes: Vec<String>,
) -> Agent {
    let (tx, rx) = mpsc::channel(1024);
    let conn = CONN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let register = pb::Register {
        node_id: id.into(),
        join_token: token.into(),
        hostname: format!("host-{id}"),
        ip_address: "10.0.0.1".into(),
        node_class: class.into(),
        agent_version: "0.1.0".into(),
        allocatable_cpu_millicores: 2000,
        allocatable_memory_bytes: 4 << 30,
        host_volumes,
        inventory,
        runtime: "docker/27.3.1".into(),
        features: features_for(id),
    };
    c.handle(CoreMsg::AgentConnected {
        register,
        conn_id: conn,
        tx,
    });
    c.finish().unwrap();
    Agent {
        id: id.into(),
        conn,
        rx,
    }
}

fn register(c: &mut Core, id: &str) -> Agent {
    let mut a = register_with(c, id, "general", TOKEN, vec![]);
    let first = a.take();
    assert!(
        matches!(first.first(), Some(pb::controller_message::Msg::RegisterAck(ack)) if ack.accepted)
    );
    a
}

fn status(wid: &str, state: WorkloadState) -> pb::WorkloadStatus {
    pb::WorkloadStatus {
        workload_id: wid.into(),
        state: state as i32,
        container_id: format!("c-{wid}"),
        ..Default::default()
    }
}

fn heartbeat(c: &mut Core, a: &Agent, workloads: Vec<pb::WorkloadStatus>) {
    let hb = pb::Heartbeat {
        seq: 1,
        workloads,
        ..Default::default()
    };
    c.handle(CoreMsg::AgentMsg {
        node_id: a.id.clone(),
        conn_id: a.conn,
        msg: pb::agent_message::Msg::Heartbeat(hb),
    });
    c.finish().unwrap();
}

fn apply(c: &mut Core, m: &str) -> ApplyOutcome {
    let out = c.apply(m, false, "test").unwrap();
    c.reconcile();
    c.finish().unwrap();
    out
}

fn by_status(c: &Core, svc: &str, s: WorkloadStatus) -> Vec<Workload> {
    let mut v: Vec<_> = c
        .workloads
        .values()
        .filter(|w| w.service_id == svc && w.status == s)
        .cloned()
        .collect();
    v.sort_by_key(|w| w.created_at);
    v
}

/// Report every ASSIGNED workload on `a` as RUNNING, started long enough ago
/// to satisfy `min_healthy_time`.
fn run_all(c: &mut Core, a: &Agent) {
    let on_node: Vec<_> = c
        .workloads
        .values()
        .filter(|w| {
            w.node_id.as_deref() == Some(&a.id)
                && matches!(w.status, WorkloadStatus::Assigned | WorkloadStatus::Running)
        })
        .map(|w| status(&w.workload_id, WorkloadState::Running))
        .collect();
    heartbeat(c, a, on_node);
    for w in c.workloads.values_mut() {
        if w.status == WorkloadStatus::Running {
            w.started_at = Some(now_ms() - 60_000);
        }
    }
}

#[test]
fn rejects_bad_join_token() {
    let mut c = core();
    let mut a = register_with(&mut c, "n1", "general", "wrong", vec![]);
    match a.take().as_slice() {
        [pb::controller_message::Msg::RegisterAck(ack)] => assert!(!ack.accepted),
        other => panic!("unexpected {other:?}"),
    }
    assert!(c.nodes.is_empty());
}

#[test]
fn at01_apply_twice_is_unchanged() {
    let mut c = core();
    let mut a = register(&mut c, "n1");
    let m = manifest("web", "nginx:1.27", 1, "");
    assert_eq!(apply(&mut c, &m).change, ChangeKind::Created);
    assert_eq!(a.starts().len(), 1);
    let events = c.db.count_rows("cluster_events");

    let again = apply(&mut c, &m);
    assert_eq!(again.change, ChangeKind::Unchanged);
    assert!(a.take().is_empty(), "no commands on unchanged apply");
    assert_eq!(
        c.db.count_rows("cluster_events"),
        events,
        "no writes on unchanged apply"
    );
}

#[test]
fn at02_scale_up_spreads_and_runs() {
    let mut c = core();
    let mut agents: Vec<_> = ["n1", "n2", "n3"]
        .iter()
        .map(|n| register(&mut c, n))
        .collect();
    apply(&mut c, &manifest("web", "nginx:1.27", 1, ""));
    let out = apply(&mut c, &manifest("web", "nginx:1.27", 3, ""));
    assert_eq!(out.change, ChangeKind::ScaleOnly);
    assert_eq!(out.version, 1, "scale-only never bumps the version");

    let per_node: Vec<usize> = agents.iter_mut().map(|a| a.starts().len()).collect();
    assert_eq!(per_node, vec![1, 1, 1], "spread across nodes");
    for a in &agents {
        run_all(&mut c, a);
    }
    assert_eq!(by_status(&c, "web", WorkloadStatus::Running).len(), 3);
}

#[test]
fn manual_scale_is_restored_by_next_apply() {
    let mut c = core();
    let _a = register(&mut c, "n1");
    let m = manifest("web", "nginx:1.27", 2, "");
    apply(&mut c, &m);
    c.scale_service_for_test("web", 1);
    let out = c.apply(&m, true, "test").unwrap();
    assert_eq!(out.change, ChangeKind::ScaleOnly, "plan shows drift");
    apply(&mut c, &m);
    assert_eq!(c.services["web"].target_replicas, 2);
}

#[test]
fn autoscaled_apply_never_resets_target() {
    let mut c = core();
    let _a = register(&mut c, "n1");
    let auto = "[autoscale]\nmin_replicas = 2\nmax_replicas = 6\ntarget_cpu_percent = 50\n";
    apply(&mut c, &manifest("web", "nginx:1.27", 3, auto));
    c.services.get_mut("web").unwrap().target_replicas = 5; // autoscaler moved it
    apply(&mut c, &manifest("web", "nginx:1.27", 3, auto));
    assert_eq!(c.services["web"].target_replicas, 5);
    let narrower = "[autoscale]\nmin_replicas = 2\nmax_replicas = 4\ntarget_cpu_percent = 50\n";
    apply(&mut c, &manifest("web", "nginx:1.27", 3, narrower));
    assert_eq!(
        c.services["web"].target_replicas, 4,
        "clamped into [min, max]"
    );
}

#[test]
fn validation_errors_write_nothing() {
    let mut c = core();
    let err = c
        .apply(&manifest("Bad_Id", "nginx:1", 1, ""), false, "t")
        .unwrap_err();
    match err {
        ValkvorkError::Validation(v) => assert_eq!(v[0].code.to_string(), "INVALID_SERVICE_ID"),
        e => panic!("{e}"),
    }
    c.finish().unwrap();
    assert_eq!(c.db.count_rows("services"), 0);
}

#[test]
fn unplaceable_workload_stays_pending_with_reason() {
    let mut c = core();
    let _a = register(&mut c, "n1");
    let out = apply(
        &mut c,
        &manifest(
            "web",
            "nginx:1",
            1,
            "[constraints]\nrequired_class = \"gpu\"\n",
        ),
    );
    assert!(out.warnings.iter().any(|w| w.contains("UNSATISFIABLE_NOW")));
    let p = by_status(&c, "web", WorkloadStatus::Pending);
    assert_eq!(p.len(), 1);
    assert!(
        p[0].status_reason
            .as_deref()
            .unwrap()
            .starts_with("NO_NODE"),
        "{:?}",
        p[0].status_reason
    );
}

/// Flow 3 / AT-05: node lost → replacement elsewhere → old copy stopped on
/// reconnect, ending with exactly the target count.
#[test]
fn lost_node_is_replaced_and_old_copy_stopped_on_reconnect() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    let mut b = register(&mut c, "b");
    // Fill b's memory so the first placement lands on a.
    c.nodes.get_mut("b").unwrap().node.allocatable_memory_bytes = 1 << 20;
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    assert_eq!(a.starts().len(), 1);
    run_all(&mut c, &a);
    let old = by_status(&c, "web", WorkloadStatus::Running)[0]
        .workload_id
        .clone();
    c.nodes.get_mut("b").unwrap().node.allocatable_memory_bytes = 4 << 30;

    // a goes silent.
    c.nodes.get_mut("a").unwrap().last_seen = Instant::now() - Duration::from_secs(11);
    c.check_health();
    c.finish().unwrap();
    assert_eq!(c.nodes["a"].node.status, NodeStatus::Unhealthy);
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Lost);
    let starts = b.starts();
    assert_eq!(starts.len(), 1, "replacement dispatched to b");
    let new = starts[0].workload_id.clone();
    assert_eq!(
        c.workloads[&new].replaces_workload_id.as_deref(),
        Some(old.as_str())
    );
    run_all(&mut c, &b);

    // a comes back still running the old copy.
    let mut a2 = register_with(
        &mut c,
        "a",
        "general",
        TOKEN,
        vec![status(&old, WorkloadState::Running)],
    );
    let msgs = a2.take();
    assert!(
        matches!(msgs[0], pb::controller_message::Msg::RegisterAck(_)),
        "ack first"
    );
    assert_eq!(stops(&msgs), vec![old.clone()]);
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Stopping);
    heartbeat(&mut c, &a2, vec![]);
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Stopped);
    assert_eq!(
        c.workloads.values().filter(|w| w.is_live()).count(),
        1,
        "replicas exact"
    );
    drop(a);
}

/// FR-CON-03: if the replacement never got placed, re-adopt the lost copy.
#[test]
fn lost_copy_is_readopted_when_replacement_pending() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    a.starts();
    run_all(&mut c, &a);
    let old = by_status(&c, "web", WorkloadStatus::Running)[0]
        .workload_id
        .clone();

    c.nodes.get_mut("a").unwrap().last_seen = Instant::now() - Duration::from_secs(11);
    c.check_health();
    c.finish().unwrap();
    assert_eq!(
        by_status(&c, "web", WorkloadStatus::Pending).len(),
        1,
        "no node for the replacement"
    );

    let mut a2 = register_with(
        &mut c,
        "a",
        "general",
        TOKEN,
        vec![status(&old, WorkloadState::Running)],
    );
    assert!(stops(&a2.take()).is_empty());
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Running);
    assert!(
        by_status(&c, "web", WorkloadStatus::Pending).is_empty(),
        "pending replacement deleted"
    );
    assert_eq!(c.workloads.values().filter(|w| w.is_live()).count(), 1);
}

#[test]
fn orphans_are_left_alone_during_grace_then_stopped() {
    let mut c = Core::new(
        ControllerConfig {
            recovery_grace_period_seconds: 30,
            ..Default::default()
        },
        Db::open_in_memory().unwrap(),
        TOKEN.into(),
    )
    .unwrap();
    let mut a = register_with(
        &mut c,
        "a",
        "general",
        TOKEN,
        vec![status("ghost", WorkloadState::Running)],
    );
    assert!(stops(&a.take()).is_empty(), "no orphan stop during grace");
    c.grace_until = Instant::now();
    heartbeat(&mut c, &a, vec![status("ghost", WorkloadState::Running)]);
    assert_eq!(stops(&a.take()), vec!["ghost".to_string()]);
}

#[test]
fn grace_period_suppresses_unhealthy_marking() {
    let mut c = Core::new(
        ControllerConfig {
            recovery_grace_period_seconds: 30,
            ..Default::default()
        },
        Db::open_in_memory().unwrap(),
        TOKEN.into(),
    )
    .unwrap();
    let _a = register(&mut c, "a");
    c.nodes.get_mut("a").unwrap().last_seen = Instant::now() - Duration::from_secs(60);
    c.check_health();
    assert_eq!(c.nodes["a"].node.status, NodeStatus::Healthy);
}

/// FR-CON-07 / AT-09: one at a time, new before old, never below target.
#[test]
fn rolling_update_replaces_one_at_a_time() {
    let mut c = core();
    let mut agents: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|n| register(&mut c, n))
        .collect();
    apply(&mut c, &manifest("web", "nginx:1", 2, ""));
    for a in agents.iter_mut() {
        a.take();
    }
    for a in &agents {
        run_all(&mut c, a);
    }

    let out = apply(&mut c, &manifest("web", "nginx:2", 2, ""));
    assert_eq!((out.change, out.version), (ChangeKind::Replacing, 2));
    assert_eq!(c.services["web"].rollout_status, RolloutStatus::InProgress);

    let mut rounds = 0;
    loop {
        let running = by_status(&c, "web", WorkloadStatus::Running);
        assert!(running.len() >= 2, "never below target");
        let new_live = c
            .workloads
            .values()
            .filter(|w| {
                w.service_version == 2 && w.is_live() && w.status != WorkloadStatus::Running
            })
            .count();
        assert!(new_live <= 1, "max_parallel = 1");
        if running.iter().all(|w| w.service_version == 2) && running.len() == 2 {
            break;
        }
        for a in &agents {
            run_all(&mut c, a);
            // Stopped containers disappear from the next heartbeat.
            let gone: Vec<_> = by_status(&c, "web", WorkloadStatus::Stopping)
                .into_iter()
                .filter(|w| w.node_id.as_deref() == Some(&a.id))
                .collect();
            if !gone.is_empty() {
                run_all(&mut c, a);
            }
        }
        c.reconcile();
        c.finish().unwrap();
        rounds += 1;
        assert!(rounds < 10, "rollout did not converge");
    }
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(c.services["web"].rollout_status, RolloutStatus::None);
}

#[test]
fn failed_new_workload_halts_rollout() {
    let mut c = core();
    let mut agents: Vec<_> = ["a", "b"].iter().map(|n| register(&mut c, n)).collect();
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    for a in &agents {
        run_all(&mut c, a);
    }
    apply(&mut c, &manifest("web", "nginx:bad", 1, ""));
    let new = c
        .workloads
        .values()
        .find(|w| w.service_version == 2)
        .unwrap()
        .clone();
    let node = agents
        .iter()
        .position(|a| Some(&a.id) == new.node_id.as_ref())
        .unwrap();
    let mut st = status(&new.workload_id, WorkloadState::Failed);
    st.reason = "IMAGE_PULL_FAILED".into();
    heartbeat(&mut c, &agents[node], vec![st]);
    assert_eq!(c.services["web"].rollout_status, RolloutStatus::Halted);
    assert_eq!(
        by_status(&c, "web", WorkloadStatus::Running).len(),
        1,
        "old keeps running"
    );
    assert_eq!(
        by_status(&c, "web", WorkloadStatus::Running)[0].service_version,
        1
    );
    // Halted: no further replacement attempts.
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(
        c.workloads
            .values()
            .filter(|w| w.service_version == 2 && w.is_live())
            .count(),
        0
    );
    let _ = agents[0].take();
}

#[test]
fn drain_migrates_before_stopping() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    let mut b = register(&mut c, "b");
    c.nodes.get_mut("b").unwrap().node.draining = true;
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    run_all(&mut c, &a);
    let old = by_status(&c, "web", WorkloadStatus::Running)[0]
        .workload_id
        .clone();
    c.nodes.get_mut("b").unwrap().node.draining = false;
    a.take();

    c.set_draining_for_test("a", true);
    assert_eq!(b.starts().len(), 1, "replacement started on b first");
    assert_eq!(
        c.workloads[&old].status,
        WorkloadStatus::Running,
        "old not stopped yet"
    );
    run_all(&mut c, &b);
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(stops(&a.take()), vec![old.clone()]);
}

#[test]
fn undrain_cancels_inflight_migration() {
    let mut c = core();
    let a = register(&mut c, "a");
    let _b = register(&mut c, "b");
    c.nodes.get_mut("b").unwrap().node.draining = true;
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    run_all(&mut c, &a);
    c.nodes.get_mut("b").unwrap().node.draining = false;

    c.set_draining_for_test("a", true);
    assert_eq!(
        c.workloads.values().filter(|w| w.is_live()).count(),
        2,
        "replacement in flight"
    );
    c.set_draining_for_test("a", false);
    let live: Vec<_> = c.workloads.values().filter(|w| w.is_live()).collect();
    assert_eq!(live.len(), 1, "replacement cancelled, original kept");
    assert_eq!(live[0].node_id.as_deref(), Some("a"));
}

#[test]
fn delete_stops_everything_then_removes_service() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    apply(&mut c, &manifest("web", "nginx:1", 2, ""));
    a.take();
    run_all(&mut c, &a);
    c.handle(CoreMsg::Admin(AdminOp::Delete {
        service_id: "web".into(),
        actor: "t".into(),
        reply: oneshot::channel().0,
    }));
    c.finish().unwrap();
    assert_eq!(stops(&a.take()).len(), 2);
    assert_eq!(c.services["web"].status, ServiceStatus::Deleting);
    heartbeat(&mut c, &a, vec![]);
    assert!(!c.services.contains_key("web"));
    assert_eq!(c.db.count_rows("services"), 0);
    assert_eq!(c.db.count_rows("workloads"), 0);
}

#[test]
fn scale_down_prefers_pending() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    // Room for exactly two.
    c.nodes
        .get_mut("a")
        .unwrap()
        .node
        .allocatable_cpu_millicores = 1000;
    apply(&mut c, &manifest("web", "nginx:1", 3, ""));
    a.take();
    run_all(&mut c, &a);
    assert_eq!(by_status(&c, "web", WorkloadStatus::Pending).len(), 1);
    apply(&mut c, &manifest("web", "nginx:1", 2, ""));
    assert!(stops(&a.take()).is_empty(), "the pending one goes first");
    assert_eq!(by_status(&c, "web", WorkloadStatus::Running).len(), 2);
}

#[test]
fn start_timeout_fails_and_backs_off() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    let wid = a.starts()[0].workload_id.clone();
    c.assigned_at
        .insert(wid.clone(), Instant::now() - Duration::from_secs(301));
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(c.workloads[&wid].status, WorkloadStatus::Failed);
    assert_eq!(
        c.workloads[&wid].status_reason.as_deref(),
        Some("START_TIMEOUT")
    );
    assert_eq!(stops(&a.take()), vec![wid], "container cleaned up");
    assert!(
        c.replacement_blocked("web"),
        "5 s backoff before the replacement"
    );
    assert_eq!(c.workloads.values().filter(|w| w.is_live()).count(), 0);
}

#[test]
fn repeated_failures_degrade_service() {
    let mut c = core();
    let _a = register(&mut c, "a");
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    for _ in 0..5 {
        c.record_failure("web", 1);
    }
    assert_eq!(c.services["web"].status, ServiceStatus::Degraded);
    c.clear_failures("web");
    assert_eq!(c.services["web"].status, ServiceStatus::Active);
}

#[test]
fn state_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.db");
    let wid = {
        let mut c = Core::new(cfg(), Db::open(&path).unwrap(), TOKEN.into()).unwrap();
        let mut a = register(&mut c, "a");
        apply(&mut c, &manifest("web", "nginx:1", 1, ""));
        let wid = a.starts()[0].workload_id.clone();
        run_all(&mut c, &a);
        wid
    };
    let c = Core::new(cfg(), Db::open(&path).unwrap(), TOKEN.into()).unwrap();
    assert_eq!(c.services["web"].target_replicas, 1);
    assert_eq!(c.workloads[&wid].status, WorkloadStatus::Running);
    assert_eq!(c.nodes["a"].node.status, NodeStatus::Healthy);
}

#[test]
fn rejected_start_is_rescheduled() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    let s = a.starts().remove(0);
    let res = pb::CommandResult {
        command_id: s.command_id,
        workload_id: s.workload_id.clone(),
        accepted: false,
        error: "RUNTIME_UNAVAILABLE".into(),
    };
    c.handle(CoreMsg::AgentMsg {
        node_id: "a".into(),
        conn_id: a.conn,
        msg: pb::agent_message::Msg::CommandResult(res),
    });
    c.finish().unwrap();
    // Unbound, then immediately re-placed (only one node) and re-sent.
    assert_eq!(c.workloads[&s.workload_id].status, WorkloadStatus::Assigned);
    assert_eq!(a.starts().len(), 1);
}

#[test]
fn autoscaler_scales_up_after_sustained_load() {
    let mut c = core();
    let a = register(&mut c, "a");
    let auto = "[autoscale]\nmin_replicas = 1\nmax_replicas = 4\ntarget_cpu_percent = 50\n";
    apply(&mut c, &manifest("web", "nginx:1", 1, auto));
    run_all(&mut c, &a);
    let wid = by_status(&c, "web", WorkloadStatus::Running)[0]
        .workload_id
        .clone();
    // 450m of a 500m limit = 90 % against a 50 % target.
    c.cpu_samples
        .insert(wid, [(Instant::now(), 450)].into_iter().collect());
    assert!((c.service_utilisation("web").unwrap() - 90.0).abs() < 0.01);

    c.autoscale();
    assert_eq!(
        c.services["web"].target_replicas, 1,
        "needs 2 minutes above the line"
    );
    c.svc_rt.get_mut("web").unwrap().above_since = Some(Instant::now() - Duration::from_secs(121));
    c.autoscale();
    c.finish().unwrap();
    assert_eq!(c.services["web"].target_replicas, 2, "ceil(1 × 90 / 50)");
    assert_eq!(c.db.count_rows("scaling_events"), 1);

    // Cooldown: no second action.
    c.svc_rt.get_mut("web").unwrap().above_since = Some(Instant::now() - Duration::from_secs(121));
    c.autoscale();
    assert_eq!(c.services["web"].target_replicas, 2);
}

impl Core {
    fn scale_service_for_test(&mut self, id: &str, n: u32) {
        let (tx, _rx) = oneshot::channel();
        self.handle(CoreMsg::Admin(AdminOp::Scale {
            service_id: id.into(),
            replicas: n,
            actor: "t".into(),
            reply: tx,
        }));
        self.finish().unwrap();
    }

    fn remove_node_for_test(&mut self, node: &str) -> Result<u32, ValkvorkError> {
        let (tx, mut rx) = oneshot::channel();
        self.handle(CoreMsg::Admin(AdminOp::RemoveNode {
            node_id: node.into(),
            actor: "t".into(),
            reply: tx,
        }));
        self.finish().unwrap();
        rx.try_recv().unwrap()
    }

    fn set_draining_for_test(&mut self, node: &str, draining: bool) {
        let (tx, _rx) = oneshot::channel();
        self.handle(CoreMsg::Admin(AdminOp::SetDraining {
            node_id: node.into(),
            draining,
            actor: "t".into(),
            reply: tx,
        }));
        self.finish().unwrap();
    }
}

#[test]
fn metrics_expose_the_nfr07_set() {
    let mut c = core();
    let a = register(&mut c, "a");
    apply(
        &mut c,
        &manifest(
            "web",
            "nginx:1",
            2,
            "[constraints]\nrequired_class = \"gpu\"\n",
        ),
    );
    apply(&mut c, &manifest("api", "nginx:1", 1, ""));
    run_all(&mut c, &a);
    let text = c.render_metrics();
    for name in [
        "valkvork_nodes{status=\"HEALTHY\",class=\"general\"} 1",
        "valkvork_workloads{status=\"RUNNING\"} 1",
        "valkvork_pending_unplaced_workloads{class=\"gpu\"} 2",
        "valkvork_pending_unplaced_workloads{class=\"general\"} 0",
        "valkvork_allocated_ratio{class=\"general\",resource=\"cpu\"} 0.25",
        "valkvork_scaling_actions_total",
        "valkvork_schedule_duration_seconds_count",
        "valkvork_schedule_duration_seconds_bucket{le=\"+Inf\"}",
        "valkvork_agent_registrations_total 1",
        "valkvork_controller_recovery_grace 0",
    ] {
        assert!(text.contains(name), "missing {name} in:\n{text}");
    }
    // Every sample line belongs to a declared metric family.
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        let family = l.split(['{', ' ']).next().unwrap();
        let base = family
            .trim_end_matches("_bucket")
            .trim_end_matches("_sum")
            .trim_end_matches("_count");
        assert!(
            text.contains(&format!("# TYPE {base} "))
                || text.contains(&format!("# TYPE {family} ")),
            "{l}"
        );
    }
}

#[test]
fn metric_labels_are_escaped() {
    let mut o = String::new();
    super::observe::line(&mut o, "m", &[("a", "x\"y\\z\n")], 1.0);
    assert_eq!(o, "m{a=\"x\\\"y\\\\z\\n\"} 1\n");
}

#[test]
fn chart_series_keep_sixty_minutes() {
    let mut c = core();
    let _a = register(&mut c, "a");
    apply(&mut c, &manifest("web", "nginx:1", 1, ""));
    for _ in 0..65 {
        c.sample_series();
    }
    assert_eq!(c.service_series("web").len(), 60);
    let o = c.overview();
    assert_eq!(o.classes[0].series.len(), 60);
    assert_eq!(o.classes[0].class, "general");
}

#[test]
fn removing_a_dead_node_stops_its_lost_workloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.db");
    let (old, new) = {
        let mut c = Core::new(cfg(), Db::open(&path).unwrap(), TOKEN.into()).unwrap();
        let mut a = register(&mut c, "a");
        let mut b = register(&mut c, "b");
        c.nodes.get_mut("b").unwrap().node.allocatable_memory_bytes = 1 << 20;
        apply(&mut c, &manifest("web", "nginx:1", 1, ""));
        let old = a.starts()[0].workload_id.clone();
        run_all(&mut c, &a);
        c.nodes.get_mut("b").unwrap().node.allocatable_memory_bytes = 4 << 30;

        c.nodes.get_mut("a").unwrap().last_seen = Instant::now() - Duration::from_secs(11);
        c.check_health();
        c.finish().unwrap();
        let new = b.starts()[0].workload_id.clone();
        run_all(&mut c, &b);
        assert_eq!(c.workloads[&old].status, WorkloadStatus::Lost);

        assert_eq!(c.remove_node_for_test("a").unwrap(), 1);
        assert!(!c.nodes.contains_key("a"));
        let w = &c.workloads[&old];
        assert_eq!(w.status, WorkloadStatus::Stopped);
        assert_eq!(w.status_reason.as_deref(), Some("NODE_REMOVED"));
        assert_eq!(w.node_id, None);
        assert_eq!(c.workloads[&new].status, WorkloadStatus::Running);
        assert!(c.events.is_empty(), "audit event committed");
        drop(a);
        (old, new)
    };

    // Durable: the node row is gone and the history row lost its node.
    let mut c = Core::new(cfg(), Db::open(&path).unwrap(), TOKEN.into()).unwrap();
    assert!(!c.nodes.contains_key("a"));
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Stopped);
    assert_eq!(c.workloads[&old].node_id, None);
    assert_eq!(c.workloads[&new].status, WorkloadStatus::Running);

    // The VM comes back after all: it joins as a new node and its stale
    // copy is stopped.
    let mut a2 = register_with(
        &mut c,
        "a",
        "general",
        TOKEN,
        vec![status(&old, WorkloadState::Running)],
    );
    assert_eq!(stops(&a2.take()), vec![old.clone()]);
    assert_eq!(c.nodes["a"].node.status, NodeStatus::Healthy);
}

#[test]
fn removing_a_healthy_node_is_refused() {
    let mut c = core();
    let _a = register(&mut c, "a");
    let err = c.remove_node_for_test("a").unwrap_err();
    assert!(matches!(err, ValkvorkError::FailedPrecondition(_)), "{err}");
    assert!(c.nodes.contains_key("a"));
    assert!(matches!(
        c.remove_node_for_test("missing"),
        Err(ValkvorkError::NotFound(_))
    ));
}

// ----------------------------------------------------------------------
// Single-writer volumes: stop-first updates and drains
// ----------------------------------------------------------------------

const PGDATA: &str = "[[volumes]]\ntype = \"host\"\nsource = \"pgdata\"\ntarget = \"/data\"\n";

fn register_pg(c: &mut Core, id: &str) -> Agent {
    let mut a = register_full(c, id, "general", TOKEN, vec![], vec!["pgdata".into()]);
    a.take();
    a
}

/// Report the agent's containers: RUNNING for live ones, none for stopping
/// ones (their containers are gone).
fn report(c: &mut Core, a: &Agent) {
    let on_node: Vec<_> = c
        .workloads
        .values()
        .filter(|w| {
            w.node_id.as_deref() == Some(&a.id)
                && matches!(w.status, WorkloadStatus::Assigned | WorkloadStatus::Running)
        })
        .map(|w| status(&w.workload_id, WorkloadState::Running))
        .collect();
    heartbeat(c, a, on_node);
    for w in c.workloads.values_mut() {
        if w.status == WorkloadStatus::Running && w.started_at.is_some() {
            w.started_at = Some(now_ms() - 60_000);
        }
    }
}

#[test]
fn single_writer_rolling_update_stops_old_before_starting_new() {
    let mut c = core();
    let mut a = register_pg(&mut c, "a");
    apply(&mut c, &manifest("pg", "postgres:16", 1, PGDATA));
    let old = a.starts()[0].workload_id.clone();
    report(&mut c, &a);

    let plan = c
        .apply(&manifest("pg", "postgres:17", 1, PGDATA), true, "test")
        .unwrap();
    assert_eq!(
        (plan.to_place, plan.placeable),
        (1, 1),
        "old one is stopped first"
    );
    let out = apply(&mut c, &manifest("pg", "postgres:17", 1, PGDATA));
    assert_eq!(out.change, ChangeKind::Replacing);
    // Old first: a stop goes out, no start, and the replacement waits.
    let msgs = a.take();
    assert_eq!(stops(&msgs), vec![old.clone()]);
    assert!(!msgs
        .iter()
        .any(|m| matches!(m, pb::controller_message::Msg::StartWorkload(_))));
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Stopping);
    assert_eq!(
        c.workloads[&old].status_reason.as_deref(),
        Some("ROLLED_OUT")
    );
    let new = by_status(&c, "pg", WorkloadStatus::Pending);
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].replaces_workload_id.as_deref(), Some(old.as_str()));
    assert!(new[0]
        .status_reason
        .as_deref()
        .unwrap()
        .contains("single-writer host volume pgdata in use"));
    assert_eq!(c.services["pg"].rollout_status, RolloutStatus::InProgress);

    // The old container is gone: the replacement starts on the same node.
    report(&mut c, &a);
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Stopped);
    let starts = a.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].workload_id, new[0].workload_id);
    assert!(starts[0].image.ends_with("postgres:17"));
    assert_eq!(c.services["pg"].rollout_status, RolloutStatus::InProgress);

    report(&mut c, &a);
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(c.services["pg"].rollout_status, RolloutStatus::None);
    assert_eq!(c.workloads.values().filter(|w| w.is_live()).count(), 1);
}

#[test]
fn single_writer_drain_keeps_workload_when_it_cannot_move() {
    let mut c = core();
    let mut a = register_pg(&mut c, "a");
    let _b = register(&mut c, "b"); // no pgdata
    apply(&mut c, &manifest("pg", "postgres:16", 1, PGDATA));
    let old = a.starts()[0].workload_id.clone();
    report(&mut c, &a);

    c.set_draining_for_test("a", true);
    c.reconcile();
    c.finish().unwrap();
    assert!(
        stops(&a.take()).is_empty(),
        "never stopped with nowhere to go"
    );
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Running);
    assert_eq!(c.workloads.values().filter(|w| w.is_live()).count(), 1);
    let blocked = c.db_events_for_test(&old);
    assert_eq!(blocked, 1, "reported once, not every pass");
}

#[test]
fn single_writer_drain_moves_when_another_node_has_the_volume() {
    let mut c = core();
    let mut a = register_pg(&mut c, "a");
    let mut b = register_pg(&mut c, "b");
    c.nodes.get_mut("b").unwrap().node.allocatable_memory_bytes = 1 << 20;
    apply(&mut c, &manifest("pg", "postgres:16", 1, PGDATA));
    let old = a.starts()[0].workload_id.clone();
    report(&mut c, &a);
    c.nodes.get_mut("b").unwrap().node.allocatable_memory_bytes = 4 << 30;

    c.set_draining_for_test("a", true);
    assert_eq!(stops(&a.take()), vec![old.clone()]);
    assert_eq!(c.workloads[&old].status_reason.as_deref(), Some("DRAINED"));
    // b's pgdata is a different directory, so the replacement need not wait.
    let starts = b.starts();
    assert_eq!(starts.len(), 1);
    report(&mut c, &b);
    report(&mut c, &a);
    assert_eq!(c.workloads[&old].status, WorkloadStatus::Stopped);
    assert_eq!(c.workloads.values().filter(|w| w.is_live()).count(), 1);
}

#[test]
fn single_writer_volume_is_not_shared_by_replicas() {
    let mut c = core();
    let mut a = register_pg(&mut c, "a");
    apply(&mut c, &manifest("pg", "postgres:16", 2, PGDATA));
    assert_eq!(a.starts().len(), 1);
    let pending = by_status(&c, "pg", WorkloadStatus::Pending);
    assert_eq!(pending.len(), 1);
    assert!(pending[0]
        .status_reason
        .as_deref()
        .unwrap()
        .contains("pgdata in use"));

    // Declared shared: both replicas run.
    let shared = format!("{PGDATA}single_writer = false\n");
    apply(&mut c, &manifest("pg", "postgres:16", 2, &shared));
    report(&mut c, &a);
    report(&mut c, &a);
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(by_status(&c, "pg", WorkloadStatus::Running).len(), 2);
}

impl Core {
    /// Committed cluster events about one subject.
    fn db_events_for_test(&self, subject: &str) -> usize {
        self.db.count_events_for_test(subject)
    }
}

/// With a health check, a rolling update keeps the old workload until the new
/// one has passed its check for min_healthy_time, not just started.
#[test]
fn rollout_waits_for_the_health_check() {
    let mut c = core();
    let mut a = register(&mut c, "a");
    let mut b = register(&mut c, "b");
    let extra = "[[ports]]\nname = \"http\"\ncontainer_port = 80\nhost_port = 8080\n\n\
                 [check]\ntype = \"http\"\nport = \"http\"\npath = \"/healthz\"\n";
    apply(&mut c, &manifest("web", "nginx:1", 1, extra));
    a.take();
    b.take();
    run_all(&mut c, &a);
    run_all(&mut c, &b);
    let old = by_status(&c, "web", WorkloadStatus::Running)[0]
        .workload_id
        .clone();

    apply(&mut c, &manifest("web", "nginx:2", 1, extra));
    let starts: Vec<_> = a.starts().into_iter().chain(b.starts()).collect();
    assert_eq!(starts.len(), 1);
    let check = starts[0]
        .check
        .as_ref()
        .expect("the start carries the check");
    assert_eq!(
        (
            check.r#type.as_str(),
            check.port_name.as_str(),
            check.path.as_str()
        ),
        ("http", "http", "/healthz")
    );
    let new = starts[0].workload_id.clone();
    // host_port 8080 is static, so the new workload is on the other node.
    let agent = if c.workloads[&new].node_id.as_deref() == Some("a") {
        &a
    } else {
        &b
    };

    let mut st = status(&new, WorkloadState::Running);
    st.health = pb::HealthState::Starting as i32;
    heartbeat(&mut c, agent, vec![st.clone()]);
    c.workloads.get_mut(&new).unwrap().started_at = Some(now_ms() - 60_000);
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(
        c.workloads[&old].status,
        WorkloadStatus::Running,
        "running but not yet healthy"
    );
    assert_eq!(c.workloads[&new].health, crate::model::Health::Starting);

    st.health = pb::HealthState::Healthy as i32;
    heartbeat(&mut c, agent, vec![st]);
    assert!(c.workloads[&new].healthy_since.is_some());
    c.workloads.get_mut(&new).unwrap().healthy_since = Some(now_ms() - 60_000);
    c.reconcile();
    c.finish().unwrap();
    assert_eq!(
        c.workloads[&old].status,
        WorkloadStatus::Stopping,
        "healthy long enough"
    );
}

/// A workload using secrets is never placed on an agent that does not
/// support them; it waits with a reason, and plain services still run there.
#[test]
fn features_need_an_agent_that_supports_them() {
    let mut c = core();
    let mut old = register(&mut c, "old-1");
    apply(
        &mut c,
        &manifest("pg", "postgres:16", 1, "[secrets]\nPW = \"ssm:/x\"\n"),
    );
    apply(&mut c, &manifest("plain", "nginx:1", 1, ""));
    let pending = by_status(&c, "pg", WorkloadStatus::Pending);
    assert_eq!(pending.len(), 1, "not placed on the old agent");
    let reason = pending[0].status_reason.clone().unwrap_or_default();
    assert!(reason.contains("agent lacks secrets"), "{reason}");
    assert_eq!(by_status(&c, "plain", WorkloadStatus::Assigned).len(), 1);
    old.take();

    // A current agent joins: the workload goes there.
    // (register_with keeps the messages register() would drain.)
    let mut new = register_with(&mut c, "new-1", "general", TOKEN, vec![]);
    c.reconcile();
    c.finish().unwrap();
    let starts = new.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].service_id, "pg");
}
