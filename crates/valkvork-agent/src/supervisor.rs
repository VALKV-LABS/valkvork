//! Local workload supervisor.
//!
//! Owns the agent's view of its containers and carries out commands:
//! idempotent start/stop (FR-AGE-04), the local restart policy (FR-AGE-06),
//! adoption after an agent restart (FR-AGE-07), ephemeral disk enforcement
//! (FR-AGE-08) and per-container telemetry (FR-AGE-03).
//!
//! It never acts on its own when the controller is unreachable except for
//! restart-on-failure, which the policy delegates to it (FR-AGE-05).

use crate::runtime::{
    BindMount, ContainerRuntime, ContainerSpec, RunState, RuntimeEvent, LABEL_WORKLOAD,
};
use crate::telemetry::{dir_size, read_container, CpuRate};
use futures::StreamExt;
use pb::WorkloadState;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use valkvork_common::config::HostVolume;
use valkvork_common::errors::RuntimeError;
use valkvork_proto::v1 as pb;

const LABEL_SERVICE: &str = "valkvork.service_id";
const LABEL_VERSION: &str = "valkvork.service_version";
const LABEL_RESTART: &str = "valkvork.restart";
const LABEL_KILL_TIMEOUT: &str = "valkvork.kill_timeout";
const LABEL_DISK: &str = "valkvork.disk_bytes";

/// Paths a workload may never mount (FR-SEC-06).
const FORBIDDEN_MOUNTS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/var/lib/docker",
    "/run/containerd",
    "/var/lib/containerd",
];

#[derive(Debug, Clone, Copy)]
struct Policy {
    attempts: u32,
    interval: Duration,
    delay: Duration,
}

#[derive(Debug)]
struct Local {
    service_id: String,
    service_version: u64,
    container_id: Option<String>,
    state: WorkloadState,
    reason: String,
    exit_code: i32,
    restart_count: u32,
    recent_restarts: VecDeque<Instant>,
    policy: Policy,
    kill_timeout: u32,
    disk_limit: u64,
    /// Stop requested: exits are expected and never restarted.
    stopping: bool,
    oom: bool,
    cpu: CpuRate,
    cpu_millicores: u32,
    memory_bytes: u64,
    alloc_bytes: u64,
    rw_bytes: u64,
}

impl Local {
    fn new(
        service_id: String,
        service_version: u64,
        policy: Policy,
        kill_timeout: u32,
        disk_limit: u64,
    ) -> Self {
        Self {
            service_id,
            service_version,
            container_id: None,
            state: WorkloadState::Starting,
            reason: String::new(),
            exit_code: 0,
            restart_count: 0,
            recent_restarts: VecDeque::new(),
            policy,
            kill_timeout,
            disk_limit,
            stopping: false,
            oom: false,
            cpu: CpuRate::default(),
            cpu_millicores: 0,
            memory_bytes: 0,
            alloc_bytes: 0,
            rw_bytes: 0,
        }
    }
}

pub struct SupervisorConfig {
    pub data_dir: PathBuf,
    pub registry_auth_file: PathBuf,
    pub docker_socket: PathBuf,
    /// Additional never-mountable paths, e.g. the runtime's data root.
    pub protected_paths: Vec<PathBuf>,
    pub host_volumes: BTreeMap<String, HostVolume>,
}

pub struct Supervisor {
    rt: Arc<dyn ContainerRuntime>,
    cfg: SupervisorConfig,
    workloads: Mutex<HashMap<String, Local>>,
}

impl Supervisor {
    pub fn new(rt: Arc<dyn ContainerRuntime>, cfg: SupervisorConfig) -> Arc<Self> {
        Arc::new(Self {
            rt,
            cfg,
            workloads: Mutex::new(HashMap::new()),
        })
    }

    fn alloc_dir(&self, wid: &str) -> PathBuf {
        self.cfg.data_dir.join("alloc").join(wid)
    }

    /// Marker that survives agent restarts so a FAILED container is not
    /// restarted on adoption.
    fn failed_marker(&self, wid: &str) -> PathBuf {
        self.cfg.data_dir.join("failed").join(wid)
    }

    fn mark_failed(&self, wid: &str, l: &mut Local, reason: &str) {
        l.state = WorkloadState::Failed;
        l.reason = reason.to_string();
        let marker = self.failed_marker(wid);
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(marker, reason);
        tracing::warn!(workload = wid, reason, "workload FAILED");
    }

    // ------------------------------------------------------------------
    // Startup adoption (FR-AGE-07)
    // ------------------------------------------------------------------

    /// Resume managing containers that already exist. Never restarts a
    /// running container.
    pub async fn adopt(self: &Arc<Self>) -> Result<usize, RuntimeError> {
        let containers = self.rt.list_by_label(LABEL_WORKLOAD).await?;
        let mut exited = Vec::new();
        let mut created = Vec::new();
        {
            let mut map = self.workloads.lock().unwrap();
            for c in &containers {
                let Some(wid) = c.labels.get(LABEL_WORKLOAD).cloned() else {
                    continue;
                };
                if map.contains_key(&wid) {
                    continue; // already managed
                }
                let label = |k: &str| c.labels.get(k).cloned().unwrap_or_default();
                let policy = parse_policy(&label(LABEL_RESTART));
                let mut l = Local::new(
                    label(LABEL_SERVICE),
                    label(LABEL_VERSION).parse().unwrap_or(0),
                    policy,
                    label(LABEL_KILL_TIMEOUT).parse().unwrap_or(10),
                    label(LABEL_DISK).parse().unwrap_or(u64::MAX),
                );
                l.container_id = Some(c.id.clone());
                match c.state {
                    RunState::Running | RunState::Restarting => l.state = WorkloadState::Running,
                    RunState::Exited => match std::fs::read_to_string(self.failed_marker(&wid)) {
                        Ok(reason) => {
                            l.state = WorkloadState::Failed;
                            l.reason = reason;
                        }
                        Err(_) => {
                            // Exited while the agent was down: treat as a crash.
                            l.state = WorkloadState::Running;
                            exited.push(wid.clone());
                        }
                    },
                    RunState::Created | RunState::Other => created.push(wid.clone()),
                }
                map.insert(wid, l);
            }
        }
        for wid in exited {
            let code = match self.container_of(&wid) {
                Some(cid) => self
                    .rt
                    .inspect(&cid)
                    .await
                    .map(|d| d.exit_code as i32)
                    .unwrap_or(-1),
                None => -1,
            };
            self.handle_exit(&wid, code);
        }
        for wid in created {
            // Agent died between create and start: finish the start.
            if let Some(cid) = self.container_of(&wid) {
                let ok = self.rt.start(&cid).await.is_ok();
                let mut map = self.workloads.lock().unwrap();
                if let Some(l) = map.get_mut(&wid) {
                    if ok {
                        l.state = WorkloadState::Running;
                    } else {
                        self.mark_failed(&wid, l, "START_FAILED");
                    }
                }
            }
        }
        Ok(containers.len())
    }

    fn container_of(&self, wid: &str) -> Option<String> {
        self.workloads
            .lock()
            .unwrap()
            .get(wid)
            .and_then(|l| l.container_id.clone())
    }

    // ------------------------------------------------------------------
    // Commands (FR-AGE-04)
    // ------------------------------------------------------------------

    /// Accept or reject a start. The ack means "accepted"; progress is
    /// reported through heartbeats.
    pub async fn start(self: &Arc<Self>, cmd: pb::StartWorkload) -> Result<(), String> {
        if self
            .workloads
            .lock()
            .unwrap()
            .contains_key(&cmd.workload_id)
        {
            return Ok(()); // idempotent: already known
        }
        if tokio::time::timeout(Duration::from_secs(3), self.rt.version())
            .await
            .map_or(true, |r| r.is_err())
        {
            return Err("RUNTIME_UNAVAILABLE".into());
        }
        let restart = cmd.restart.clone().unwrap_or(pb::RestartPolicy {
            attempts: 3,
            interval_seconds: 300,
            delay_seconds: 5,
        });
        let policy = Policy {
            attempts: restart.attempts,
            interval: Duration::from_secs(restart.interval_seconds as u64),
            delay: Duration::from_secs(restart.delay_seconds as u64),
        };
        {
            let mut map = self.workloads.lock().unwrap();
            if map.contains_key(&cmd.workload_id) {
                return Ok(());
            }
            map.insert(
                cmd.workload_id.clone(),
                Local::new(
                    cmd.service_id.clone(),
                    cmd.service_version,
                    policy,
                    cmd.kill_timeout_seconds,
                    cmd.disk_bytes,
                ),
            );
        }
        let _ = std::fs::remove_file(self.failed_marker(&cmd.workload_id));
        let me = self.clone();
        tokio::spawn(async move { me.run_start(cmd, policy).await });
        Ok(())
    }

    async fn run_start(self: Arc<Self>, cmd: pb::StartWorkload, policy: Policy) {
        let wid = cmd.workload_id.clone();
        let result = self.do_start(&cmd, policy).await;
        let mut map = self.workloads.lock().unwrap();
        let Some(l) = map.get_mut(&wid) else { return };
        match result {
            Ok(cid) => {
                l.container_id = Some(cid);
                if l.state == WorkloadState::Starting {
                    l.state = WorkloadState::Running;
                    tracing::info!(workload = %wid, service = %cmd.service_id, "workload running");
                }
            }
            Err((reason, detail)) => {
                tracing::warn!(workload = %wid, %detail, "start failed");
                if !l.stopping {
                    self.mark_failed(&wid, l, &reason);
                }
            }
        }
        if l.stopping {
            // A stop arrived mid-start; finish it now that we know the container.
            let cid = l.container_id.clone();
            let timeout = l.kill_timeout;
            drop(map);
            let me = self.clone();
            tokio::spawn(async move { me.finish_stop(&wid, cid, timeout).await });
        }
    }

    async fn do_start(
        &self,
        cmd: &pb::StartWorkload,
        policy: Policy,
    ) -> Result<String, (String, String)> {
        let wid = &cmd.workload_id;
        let alloc = self.alloc_dir(wid);
        std::fs::create_dir_all(&alloc)
            .and_then(|()| make_alloc_writable(&alloc))
            .map_err(|e| ("ALLOC_DIR_FAILED".to_string(), e.to_string()))?;

        let mut mounts = vec![BindMount {
            source: alloc,
            target: "/alloc".into(),
            read_only: false,
        }];
        for v in &cmd.volumes {
            let hv = self.cfg.host_volumes.get(&v.source).ok_or_else(|| {
                (
                    "HOST_VOLUME_NOT_FOUND".to_string(),
                    format!("host volume {:?} is not declared in agent.toml", v.source),
                )
            })?;
            if self.forbidden(&hv.path) {
                return Err((
                    "FORBIDDEN_MOUNT".into(),
                    format!("{} may not be mounted into a workload", hv.path.display()),
                ));
            }
            mounts.push(BindMount {
                source: hv.path.clone(),
                target: v.target.clone(),
                read_only: v.read_only || hv.read_only,
            });
        }

        let auth = crate::runtime::registry_credentials(&self.cfg.registry_auth_file, &cmd.image);
        self.rt.pull(&cmd.image, auth).await.map_err(|e| match e {
            RuntimeError::Unavailable(d) => ("RUNTIME_UNAVAILABLE".to_string(), d),
            other => ("IMAGE_PULL_FAILED".to_string(), other.to_string()),
        })?;
        if self.is_stopping(wid) {
            return Err(("STOPPED".into(), "stop requested during pull".into()));
        }

        let labels = HashMap::from([
            (LABEL_WORKLOAD.to_string(), wid.clone()),
            (LABEL_SERVICE.to_string(), cmd.service_id.clone()),
            (LABEL_VERSION.to_string(), cmd.service_version.to_string()),
            (
                LABEL_RESTART.to_string(),
                format!(
                    "{}/{}/{}",
                    policy.attempts,
                    policy.interval.as_secs(),
                    policy.delay.as_secs()
                ),
            ),
            (
                LABEL_KILL_TIMEOUT.to_string(),
                cmd.kill_timeout_seconds.to_string(),
            ),
            (LABEL_DISK.to_string(), cmd.disk_bytes.to_string()),
        ]);
        let mut env: Vec<(String, String)> = cmd
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        env.sort();
        env.push(("VALKVORK_WORKLOAD_ID".into(), wid.clone()));
        env.push(("VALKVORK_SERVICE_ID".into(), cmd.service_id.clone()));
        let spec = ContainerSpec {
            name: format!("vk-{wid}"),
            image: cmd.image.clone(),
            cpu_millicores: cmd.cpu_millicores,
            memory_bytes: cmd.memory_bytes,
            env,
            ports: cmd
                .ports
                .iter()
                .map(|p| (p.container_port as u16, p.host_port as u16))
                .collect(),
            mounts,
            labels,
        };
        let cid = self
            .rt
            .create(&spec)
            .await
            .map_err(|e| ("CREATE_FAILED".to_string(), e.to_string()))?;
        if let Some(l) = self.workloads.lock().unwrap().get_mut(wid) {
            l.container_id = Some(cid.clone());
        }
        if self.is_stopping(wid) {
            return Ok(cid);
        }
        self.rt
            .start(&cid)
            .await
            .map_err(|e| ("START_FAILED".to_string(), e.to_string()))?;
        Ok(cid)
    }

    /// Apply each host volume's `owner` and `mode` from agent.toml, creating
    /// the directory if needed, so images that run as a non-root user can
    /// write to it. Only the directory itself changes, never its contents.
    /// Protected paths, symlinks and non-directories are refused.
    pub fn prepare_host_volumes(&self) -> Result<(), String> {
        for (name, hv) in &self.cfg.host_volumes {
            let err = |e: String| format!("host volume {name} ({}): {e}", hv.path.display());
            let owner = hv.owner_ids().map_err(err)?;
            let mode = hv.mode_bits().map_err(err)?;
            if owner.is_none() && mode.is_none() {
                continue;
            }
            if self.forbidden(&hv.path) {
                return Err(err("refusing to change a protected path".into()));
            }
            std::fs::create_dir_all(&hv.path).map_err(|e| err(e.to_string()))?;
            let meta = std::fs::symlink_metadata(&hv.path).map_err(|e| err(e.to_string()))?;
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(err("must be a directory, not a symlink".into()));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Some((uid, gid)) = owner {
                    std::os::unix::fs::chown(&hv.path, Some(uid), gid)
                        .map_err(|e| err(format!("chown: {e}")))?;
                }
                if let Some(bits) = mode {
                    std::fs::set_permissions(&hv.path, std::fs::Permissions::from_mode(bits))
                        .map_err(|e| err(format!("chmod: {e}")))?;
                }
            }
            tracing::info!(
                volume = %name,
                path = %hv.path.display(),
                owner = hv.owner.as_deref().unwrap_or("-"),
                mode = hv.mode.as_deref().unwrap_or("-"),
                "prepared host volume"
            );
        }
        Ok(())
    }

    fn forbidden(&self, path: &Path) -> bool {
        // Check both the configured and the resolved path, so symlinks cannot
        // be used to smuggle the socket in (and /var/run → /run style links match).
        let resolve = |p: &Path| {
            [
                p.to_path_buf(),
                std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
            ]
        };
        let mut protected: Vec<PathBuf> = resolve(&self.cfg.docker_socket).into();
        for f in FORBIDDEN_MOUNTS {
            protected.extend(resolve(Path::new(f)));
        }
        for p in &self.cfg.protected_paths {
            protected.extend(resolve(p));
        }
        resolve(path).iter().any(|cand| {
            protected
                .iter()
                .any(|p| p.starts_with(cand) || cand.starts_with(p))
        })
    }

    fn is_stopping(&self, wid: &str) -> bool {
        self.workloads
            .lock()
            .unwrap()
            .get(wid)
            .is_none_or(|l| l.stopping)
    }

    /// Stop and remove. Unknown workloads ack success (FR-AGE-04).
    pub async fn stop(self: &Arc<Self>, cmd: pb::StopWorkload) -> Result<(), String> {
        let wid = cmd.workload_id.clone();
        let target = {
            let mut map = self.workloads.lock().unwrap();
            match map.get_mut(&wid) {
                Some(l) => {
                    if l.stopping {
                        return Ok(()); // already in progress
                    }
                    l.stopping = true;
                    if l.state == WorkloadState::Starting && l.container_id.is_none() {
                        None // run_start finishes the stop
                    } else {
                        Some((l.container_id.clone(), cmd.kill_timeout_seconds))
                    }
                }
                None => Some((None, cmd.kill_timeout_seconds)),
            }
        };
        if let Some((cid, timeout)) = target {
            let me = self.clone();
            tokio::spawn(async move { me.finish_stop(&wid, cid, timeout).await });
        }
        Ok(())
    }

    async fn finish_stop(&self, wid: &str, container_id: Option<String>, timeout: u32) {
        let id = container_id.unwrap_or_else(|| format!("vk-{wid}"));
        if let Err(e) = self.rt.stop(&id, timeout).await {
            tracing::warn!(workload = wid, error = %e, "stop failed; removing forcibly");
        }
        if let Err(e) = self.rt.remove(&id).await {
            tracing::warn!(workload = wid, error = %e, "remove failed");
            // Keep the entry so the controller keeps seeing it and retries.
            if let Some(l) = self.workloads.lock().unwrap().get_mut(wid) {
                l.stopping = false;
            }
            return;
        }
        let _ = std::fs::remove_dir_all(self.alloc_dir(wid));
        let _ = std::fs::remove_file(self.failed_marker(wid));
        self.workloads.lock().unwrap().remove(wid);
        tracing::info!(workload = wid, "workload stopped and removed");
    }

    // ------------------------------------------------------------------
    // Restart policy (FR-AGE-06)
    // ------------------------------------------------------------------

    fn handle_exit(self: &Arc<Self>, wid: &str, exit_code: i32) {
        let mut map = self.workloads.lock().unwrap();
        let Some(l) = map.get_mut(wid) else { return };
        if l.stopping || l.state != WorkloadState::Running {
            return;
        }
        l.exit_code = exit_code;
        let oom = std::mem::take(&mut l.oom);
        let now = Instant::now();
        let window = l.policy.interval;
        while l
            .recent_restarts
            .front()
            .is_some_and(|t| now.duration_since(*t) > window)
        {
            l.recent_restarts.pop_front();
        }
        let reason = if oom { "OOM_KILLED" } else { "EXITED" };
        if (l.recent_restarts.len() as u32) < l.policy.attempts {
            l.state = WorkloadState::Restarting;
            l.reason = format!("{reason}({exit_code})");
            l.restart_count += 1;
            crate::metrics::LOCAL_RESTARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            l.recent_restarts.push_back(now);
            let (delay, cid) = (l.policy.delay, l.container_id.clone());
            tracing::info!(
                workload = wid,
                exit_code,
                reason,
                restart = l.restart_count,
                "container exited; restarting in place"
            );
            drop(map);
            let me = self.clone();
            let wid = wid.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if me.is_stopping(&wid) {
                    return;
                }
                let ok = match &cid {
                    Some(cid) => me.rt.start(cid).await.is_ok(),
                    None => false,
                };
                let mut map = me.workloads.lock().unwrap();
                if let Some(l) = map.get_mut(&wid) {
                    if l.state == WorkloadState::Restarting {
                        if ok {
                            l.state = WorkloadState::Running;
                            l.cpu = CpuRate::default();
                        } else {
                            me.mark_failed(&wid, l, "START_FAILED");
                        }
                    }
                }
            });
        } else {
            l.exit_code = exit_code;
            self.mark_failed(wid, l, reason);
        }
    }

    /// Consume runtime events forever, resubscribing if the runtime restarts.
    pub async fn run_events(self: Arc<Self>) {
        loop {
            let mut events = self.rt.events(LABEL_WORKLOAD);
            // Events may have been missed while unsubscribed.
            self.resync().await;
            while let Some(ev) = events.next().await {
                match ev {
                    Ok(RuntimeEvent::Oom { workload_id, .. }) => {
                        if let Some(l) = self.workloads.lock().unwrap().get_mut(&workload_id) {
                            l.oom = true;
                        }
                    }
                    Ok(RuntimeEvent::Died {
                        workload_id,
                        container_id,
                        exit_code,
                    }) => {
                        let ours = self.container_of(&workload_id).as_deref()
                            == Some(container_id.as_str());
                        if ours {
                            self.handle_exit(&workload_id, exit_code as i32);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "runtime event stream failed");
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Safety net for missed events: compare RUNNING entries with reality.
    async fn resync(self: &Arc<Self>) {
        let Ok(list) = self.rt.list_by_label(LABEL_WORKLOAD).await else {
            return;
        };
        // Labelled containers we do not know (created behind our back, or left
        // over): adopt them so the inventory is complete and the controller
        // can decide their fate (FR-CON-03 orphans).
        let unknown = {
            let map = self.workloads.lock().unwrap();
            list.iter().any(|c| {
                c.labels
                    .get(LABEL_WORKLOAD)
                    .is_some_and(|w| !map.contains_key(w))
            })
        };
        if unknown {
            match self.adopt().await {
                Ok(_) => tracing::info!("adopted unknown labelled containers"),
                Err(e) => tracing::warn!(error = %e, "adopting unknown containers failed"),
            }
        }
        let states: HashMap<String, RunState> = list.into_iter().map(|c| (c.id, c.state)).collect();
        let candidates: Vec<(String, String)> = self
            .workloads
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, l)| l.state == WorkloadState::Running && !l.stopping)
            .filter_map(|(w, l)| Some((w.clone(), l.container_id.clone()?)))
            .collect();
        for (wid, cid) in candidates {
            match states.get(&cid) {
                Some(RunState::Running) | Some(RunState::Restarting) => {}
                Some(_) => {
                    let code = self
                        .rt
                        .inspect(&cid)
                        .await
                        .map(|d| d.exit_code as i32)
                        .unwrap_or(-1);
                    self.handle_exit(&wid, code);
                }
                None => {
                    if let Some(l) = self.workloads.lock().unwrap().get_mut(&wid) {
                        if l.state == WorkloadState::Running && !l.stopping {
                            self.mark_failed(&wid, l, "CONTAINER_MISSING");
                        }
                    }
                }
            }
        }
    }

    /// Periodic resync, for runtimes whose event stream silently stalls.
    pub async fn run_resync(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tick.tick().await;
            self.resync().await;
        }
    }

    // ------------------------------------------------------------------
    // Ephemeral disk enforcement (FR-AGE-08)
    // ------------------------------------------------------------------

    pub async fn run_disk_checks(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            let check_rw = n.is_multiple_of(6); // every 60 s
            n += 1;
            let running: Vec<(String, Option<String>)> = self
                .workloads
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, l)| l.state == WorkloadState::Running && !l.stopping)
                .map(|(w, l)| (w.clone(), l.container_id.clone()))
                .collect();
            for (wid, cid) in running {
                let alloc = dir_size(&self.alloc_dir(&wid));
                let rw = match (&cid, check_rw) {
                    (Some(cid), true) => self.rt.writable_layer_size(cid).await.ok(),
                    _ => None,
                };
                let exceeded = {
                    let mut map = self.workloads.lock().unwrap();
                    let Some(l) = map.get_mut(&wid) else { continue };
                    l.alloc_bytes = alloc;
                    if let Some(rw) = rw {
                        l.rw_bytes = rw;
                    }
                    let over = l.alloc_bytes + l.rw_bytes > l.disk_limit
                        && l.state == WorkloadState::Running
                        && !l.stopping;
                    if over {
                        // FAILED first, so the resulting exit is not restarted.
                        self.mark_failed(&wid, l, "EPHEMERAL_DISK_EXCEEDED");
                    }
                    over.then(|| (l.container_id.clone(), l.kill_timeout))
                };
                if let Some((Some(cid), timeout)) = exceeded {
                    let _ = self.rt.stop(&cid, timeout).await;
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Reporting (FR-AGE-03)
    // ------------------------------------------------------------------

    /// Sample cgroup counters and return a status for every managed container.
    /// Managed containers by state, for metrics. Cheap: no cgroup reads.
    pub fn state_counts(&self) -> std::collections::BTreeMap<&'static str, u32> {
        let mut out: std::collections::BTreeMap<&'static str, u32> =
            ["starting", "running", "restarting", "stopped", "failed"]
                .into_iter()
                .map(|s| (s, 0))
                .collect();
        for l in self.workloads.lock().unwrap().values() {
            let key = match l.state {
                WorkloadState::Starting => "starting",
                WorkloadState::Running => "running",
                WorkloadState::Restarting => "restarting",
                WorkloadState::Stopped => "stopped",
                WorkloadState::Failed => "failed",
                WorkloadState::Unspecified => continue,
            };
            *out.entry(key).or_default() += 1;
        }
        out
    }

    pub fn report(&self) -> Vec<pb::WorkloadStatus> {
        let now = Instant::now();
        let mut map = self.workloads.lock().unwrap();
        let mut out = Vec::with_capacity(map.len());
        for (wid, l) in map.iter_mut() {
            if let (Some(cid), WorkloadState::Running) = (&l.container_id, l.state) {
                if let Some((usage, mem)) = read_container(&self.rt.cgroup_paths(cid)) {
                    l.cpu_millicores = l.cpu.update(now, usage);
                    l.memory_bytes = mem;
                }
            } else {
                l.cpu_millicores = 0;
            }
            out.push(pb::WorkloadStatus {
                workload_id: wid.clone(),
                service_id: l.service_id.clone(),
                service_version: l.service_version,
                container_id: l.container_id.clone().unwrap_or_default(),
                state: l.state as i32,
                reason: l.reason.clone(),
                exit_code: l.exit_code,
                restart_count: l.restart_count,
                cpu_usage_millicores: l.cpu_millicores,
                memory_used_bytes: l.memory_bytes,
                disk_used_bytes: l.alloc_bytes + l.rw_bytes,
            });
        }
        out
    }
}

/// `/alloc` must be writable whatever user the image runs as, so the
/// directory is world-writable with the sticky bit, like `/tmp`. It is only
/// mounted into its own container, and the parent `alloc/` stays `root 0711`,
/// so other host users cannot list it.
fn make_alloc_writable(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777))?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn parse_policy(s: &str) -> Policy {
    let mut it = s.split('/').map(|p| p.parse::<u64>().ok());
    match (
        it.next().flatten(),
        it.next().flatten(),
        it.next().flatten(),
    ) {
        (Some(a), Some(i), Some(d)) => Policy {
            attempts: a as u32,
            interval: Duration::from_secs(i),
            delay: Duration::from_secs(d),
        },
        _ => Policy {
            attempts: 3,
            interval: Duration::from_secs(300),
            delay: Duration::from_secs(5),
        },
    }
}

#[cfg(test)]
mod tests;
