//! Supervisor tests against an in-memory fake runtime.

use super::*;
use crate::runtime::{ContainerDetails, ContainerSummary, RegistryCredentials, Result as RtResult};
use async_trait::async_trait;
use futures::stream::BoxStream;
use tokio::sync::broadcast;

#[derive(Clone)]
struct FakeContainer {
    name: String,
    labels: HashMap<String, String>,
    state: RunState,
    exit_code: i64,
}

#[derive(Default)]
struct FakeState {
    containers: HashMap<String, FakeContainer>,
    /// Every spec passed to create, in order.
    created: Vec<ContainerSpec>,
    starts: u32,
    next: u32,
    fail_pull: bool,
}

struct Fake {
    s: Mutex<FakeState>,
    events: broadcast::Sender<RuntimeEvent>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            s: Mutex::new(FakeState::default()),
            events: broadcast::channel(64).0,
        })
    }

    /// Simulate the container's process exiting.
    fn crash(&self, wid: &str, code: i64) {
        let id = {
            let mut s = self.s.lock().unwrap();
            let (id, c) = s
                .containers
                .iter_mut()
                .find(|(_, c)| c.labels.get(LABEL_WORKLOAD).map(String::as_str) == Some(wid))
                .unwrap();
            c.state = RunState::Exited;
            c.exit_code = code;
            id.clone()
        };
        let _ = self.events.send(RuntimeEvent::Died {
            container_id: id,
            workload_id: wid.into(),
            exit_code: code,
        });
    }

    fn starts(&self) -> u32 {
        self.s.lock().unwrap().starts
    }

    fn count(&self) -> usize {
        self.s.lock().unwrap().containers.len()
    }
}

#[async_trait]
impl ContainerRuntime for Fake {
    async fn version(&self) -> RtResult<String> {
        Ok("fake/1".into())
    }
    async fn host_resources(&self) -> RtResult<(u32, u64)> {
        Ok((4000, 8 << 30))
    }
    async fn pull(&self, image: &str, _: Option<RegistryCredentials>) -> RtResult<()> {
        if self.s.lock().unwrap().fail_pull {
            return Err(RuntimeError::ImagePullFailed(image.into()));
        }
        Ok(())
    }
    async fn create(&self, spec: &ContainerSpec) -> RtResult<String> {
        let mut s = self.s.lock().unwrap();
        if let Some((id, _)) = s.containers.iter().find(|(_, c)| c.name == spec.name) {
            return Ok(id.clone());
        }
        s.created.push(spec.clone());
        s.next += 1;
        let id = format!("c{}", s.next);
        s.containers.insert(
            id.clone(),
            FakeContainer {
                name: spec.name.clone(),
                labels: spec.labels.clone(),
                state: RunState::Created,
                exit_code: 0,
            },
        );
        Ok(id)
    }
    async fn start(&self, id: &str) -> RtResult<()> {
        let mut s = self.s.lock().unwrap();
        s.starts += 1;
        s.containers
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound(id.into()))?
            .state = RunState::Running;
        Ok(())
    }
    async fn stop(&self, id: &str, _: u32) -> RtResult<()> {
        // Like Docker, stopping a running container reports its exit.
        let died = {
            let mut s = self.s.lock().unwrap();
            match s.containers.get_mut(id) {
                Some(c) if c.state == RunState::Running => {
                    c.state = RunState::Exited;
                    c.exit_code = 143;
                    c.labels.get(LABEL_WORKLOAD).cloned()
                }
                Some(c) => {
                    c.state = RunState::Exited;
                    None
                }
                None => None,
            }
        };
        if let Some(wid) = died {
            let _ = self.events.send(RuntimeEvent::Died {
                container_id: id.into(),
                workload_id: wid,
                exit_code: 143,
            });
        }
        Ok(())
    }
    async fn remove(&self, id: &str) -> RtResult<()> {
        let mut s = self.s.lock().unwrap();
        let key = s
            .containers
            .iter()
            .find(|(k, c)| k.as_str() == id || c.name == id)
            .map(|(k, _)| k.clone());
        if let Some(k) = key {
            s.containers.remove(&k);
        }
        Ok(())
    }
    async fn list_by_label(&self, _: &str) -> RtResult<Vec<ContainerSummary>> {
        Ok(self
            .s
            .lock()
            .unwrap()
            .containers
            .iter()
            .map(|(id, c)| ContainerSummary {
                id: id.clone(),
                labels: c.labels.clone(),
                state: c.state,
            })
            .collect())
    }
    async fn inspect(&self, id: &str) -> RtResult<ContainerDetails> {
        let s = self.s.lock().unwrap();
        let c = s
            .containers
            .get(id)
            .ok_or_else(|| RuntimeError::NotFound(id.into()))?;
        Ok(ContainerDetails {
            id: id.into(),
            state: c.state,
            exit_code: c.exit_code,
            oom_killed: false,
        })
    }
    fn events(&self, _: &str) -> BoxStream<'static, RtResult<RuntimeEvent>> {
        let rx = self.events.subscribe();
        futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => return Some((Ok(ev), rx)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return None,
                }
            }
        })
        .boxed()
    }
    fn cgroup_paths(&self, _: &str) -> Vec<PathBuf> {
        vec![]
    }
    async fn writable_layer_size(&self, _: &str) -> RtResult<u64> {
        Ok(0)
    }
}

impl Fake {
    fn last_created(&self) -> ContainerSpec {
        self.s
            .lock()
            .unwrap()
            .created
            .last()
            .cloned()
            .expect("a container was created")
    }
}

/// A supervisor whose `file:` secrets may come from `dir/secrets`.
fn supervisor_with_secrets(rt: Arc<Fake>, dir: &Path) -> Arc<Supervisor> {
    let secrets = dir.join("secrets");
    std::fs::create_dir_all(&secrets).unwrap();
    Supervisor::new(
        rt,
        SupervisorConfig {
            advertise_ip: "10.0.0.5".into(),
            secrets: Arc::new(crate::secrets::SecretResolver::new(
                valkvork_common::config::SecretsConfig {
                    file_dirs: vec![secrets],
                    ..Default::default()
                },
            )),
            data_dir: dir.to_path_buf(),
            registry_auth_file: dir.join("none.json"),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            protected_paths: vec![],
            host_volumes: BTreeMap::new(),
        },
    )
}

fn supervisor(rt: Arc<Fake>, dir: &Path) -> Arc<Supervisor> {
    Supervisor::new(
        rt,
        SupervisorConfig {
            advertise_ip: "10.0.0.5".into(),
            secrets: Arc::new(crate::secrets::SecretResolver::new(Default::default())),
            data_dir: dir.to_path_buf(),
            registry_auth_file: dir.join("none.json"),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            protected_paths: vec![],
            host_volumes: BTreeMap::new(),
        },
    )
}

fn start_cmd(wid: &str, attempts: u32) -> pb::StartWorkload {
    pb::StartWorkload {
        command_id: "1".into(),
        workload_id: wid.into(),
        service_id: "svc".into(),
        service_version: 1,
        image: "img:1".into(),
        cpu_millicores: 100,
        memory_bytes: 64 << 20,
        disk_bytes: 1 << 30,
        restart: Some(pb::RestartPolicy {
            attempts,
            interval_seconds: 300,
            delay_seconds: 0,
        }),
        kill_timeout_seconds: 1,
        ..Default::default()
    }
}

fn state_of(s: &Supervisor, wid: &str) -> Option<(WorkloadState, String, u32)> {
    s.report()
        .into_iter()
        .find(|w| w.workload_id == wid)
        .map(|w| {
            (
                WorkloadState::try_from(w.state).unwrap(),
                w.reason,
                w.restart_count,
            )
        })
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
}

#[tokio::test]
async fn start_is_idempotent_and_runs() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    s.start(start_cmd("w1", 3)).await.unwrap();
    s.start(start_cmd("w1", 3)).await.unwrap();
    settle().await;
    assert_eq!(state_of(&s, "w1").unwrap().0, WorkloadState::Running);
    assert_eq!(rt.count(), 1);
    assert_eq!(rt.starts(), 1);
    assert!(d.path().join("alloc/w1").is_dir());
}

/// Images that run as a non-root USER can write to /alloc.
#[cfg(unix)]
#[tokio::test]
async fn alloc_dir_is_writable_by_any_user() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let s = supervisor(Fake::new(), d.path());
    s.start(start_cmd("w1", 3)).await.unwrap();
    settle().await;
    let mode = std::fs::metadata(d.path().join("alloc/w1"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o1777);
}

#[tokio::test]
async fn stop_removes_container_and_alloc_dir() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    s.start(start_cmd("w1", 3)).await.unwrap();
    settle().await;
    s.stop(pb::StopWorkload {
        command_id: "2".into(),
        workload_id: "w1".into(),
        kill_timeout_seconds: 1,
    })
    .await
    .unwrap();
    settle().await;
    assert!(state_of(&s, "w1").is_none());
    assert_eq!(rt.count(), 0);
    assert!(!d.path().join("alloc/w1").exists());
    // Unknown workload: success.
    s.stop(pb::StopWorkload {
        command_id: "3".into(),
        workload_id: "nope".into(),
        kill_timeout_seconds: 1,
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn restart_policy_then_failed() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    tokio::spawn(s.clone().run_events());
    settle().await;
    s.start(start_cmd("w1", 2)).await.unwrap();
    settle().await;

    for expected in 1..=2 {
        rt.crash("w1", 1);
        settle().await;
        let (st, _, restarts) = state_of(&s, "w1").unwrap();
        assert_eq!(
            (st, restarts),
            (WorkloadState::Running, expected),
            "restart {expected} in place"
        );
    }
    rt.crash("w1", 137);
    settle().await;
    let (st, reason, _) = state_of(&s, "w1").unwrap();
    assert_eq!(st, WorkloadState::Failed);
    assert_eq!(reason, "EXITED");
    assert_eq!(
        rt.count(),
        1,
        "failed container left in place for the controller"
    );
    assert!(d.path().join("failed/w1").exists());
}

#[tokio::test]
async fn pull_failure_reports_failed() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    rt.s.lock().unwrap().fail_pull = true;
    let s = supervisor(rt.clone(), d.path());
    s.start(start_cmd("w1", 3)).await.unwrap();
    settle().await;
    assert_eq!(state_of(&s, "w1").unwrap().1, "IMAGE_PULL_FAILED");
}

#[tokio::test]
async fn adoption_never_restarts_running_containers() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    {
        let s = supervisor(rt.clone(), d.path());
        s.start(start_cmd("w1", 3)).await.unwrap();
        s.start(start_cmd("w2", 0)).await.unwrap();
        settle().await;
    }
    // w2 died while the agent was down and has no restarts left.
    rt.s.lock()
        .unwrap()
        .containers
        .values_mut()
        .find(|c| c.name == "vk-w2")
        .unwrap()
        .state = RunState::Exited;
    let starts_before = rt.starts();

    let s = supervisor(rt.clone(), d.path());
    assert_eq!(s.adopt().await.unwrap(), 2);
    assert_eq!(
        rt.starts(),
        starts_before,
        "no container restarted on adoption"
    );
    assert_eq!(state_of(&s, "w1").unwrap().0, WorkloadState::Running);
    assert_eq!(state_of(&s, "w2").unwrap().0, WorkloadState::Failed);
}

#[tokio::test]
async fn forbidden_host_volume_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = Supervisor::new(
        rt.clone(),
        SupervisorConfig {
            advertise_ip: "10.0.0.5".into(),
            secrets: Arc::new(crate::secrets::SecretResolver::new(Default::default())),
            data_dir: d.path().to_path_buf(),
            registry_auth_file: d.path().join("none.json"),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            protected_paths: vec![PathBuf::from("/data/docker")],
            host_volumes: BTreeMap::from([
                (
                    "sock".to_string(),
                    HostVolume {
                        path: "/var/run".into(),
                        read_only: true,
                        ..Default::default()
                    },
                ),
                (
                    "dockerdata".to_string(),
                    HostVolume {
                        path: "/data/docker/volumes".into(),
                        read_only: false,
                        ..Default::default()
                    },
                ),
            ]),
        },
    );
    for (wid, vol) in [("w1", "sock"), ("w2", "dockerdata")] {
        let mut cmd = start_cmd(wid, 3);
        cmd.volumes.push(pb::VolumeMount {
            r#type: "host".into(),
            source: vol.into(),
            target: "/s".into(),
            read_only: true,
        });
        s.start(cmd).await.unwrap();
    }
    settle().await;
    assert_eq!(state_of(&s, "w1").unwrap().1, "FORBIDDEN_MOUNT");
    assert_eq!(state_of(&s, "w2").unwrap().1, "FORBIDDEN_MOUNT");
    assert_eq!(rt.count(), 0);
}

#[test]
fn policy_label_round_trip() {
    let p = parse_policy("4/60/2");
    assert_eq!(
        (p.attempts, p.interval.as_secs(), p.delay.as_secs()),
        (4, 60, 2)
    );
    assert_eq!(parse_policy("garbage").attempts, 3);
}

#[tokio::test]
async fn resync_reports_foreign_labelled_containers() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    assert_eq!(s.adopt().await.unwrap(), 0);
    rt.s.lock().unwrap().containers.insert(
        "x1".into(),
        FakeContainer {
            name: "vk-ghost".into(),
            labels: HashMap::from([(LABEL_WORKLOAD.to_string(), "ghost".to_string())]),
            state: RunState::Running,
            exit_code: 0,
        },
    );
    s.resync().await;
    assert_eq!(state_of(&s, "ghost").unwrap().0, WorkloadState::Running);
}

#[tokio::test]
async fn metrics_render_counts_workloads() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    s.start(start_cmd("w1", 3)).await.unwrap();
    settle().await;
    let text = crate::metrics::render(&s);
    assert!(
        text.contains("valkvork_agent_workloads{state=\"running\"} 1"),
        "{text}"
    );
    assert!(text.contains("valkvork_agent_reconnects_total "));
    assert!(text.contains("# TYPE valkvork_agent_commands_total counter"));
}

fn supervisor_with(dir: &Path, vols: Vec<(&str, HostVolume)>) -> Arc<Supervisor> {
    Supervisor::new(
        Fake::new(),
        SupervisorConfig {
            advertise_ip: "10.0.0.5".into(),
            secrets: Arc::new(crate::secrets::SecretResolver::new(Default::default())),
            data_dir: dir.to_path_buf(),
            registry_auth_file: dir.join("none.json"),
            docker_socket: PathBuf::from("/var/run/docker.sock"),
            protected_paths: vec![dir.join("docker")],
            host_volumes: vols.into_iter().map(|(n, v)| (n.to_string(), v)).collect(),
        },
    )
}

/// owner and mode from agent.toml are applied to the directory at startup.
#[cfg(unix)]
#[test]
fn host_volume_owner_and_mode_are_applied() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = tempfile::tempdir().unwrap();
    // Without root, the only uid we may chown to is our own.
    let me = std::fs::metadata(d.path()).unwrap();
    let logs = d.path().join("logs");
    let s = supervisor_with(
        d.path(),
        vec![
            (
                "logs",
                HostVolume {
                    path: logs.clone(),
                    owner: Some(format!("{}:{}", me.uid(), me.gid())),
                    mode: Some("1777".into()),
                    ..Default::default()
                },
            ),
            // Neither set: left alone, not even created.
            (
                "plain",
                HostVolume {
                    path: d.path().join("plain"),
                    ..Default::default()
                },
            ),
        ],
    );
    s.prepare_host_volumes().unwrap();
    let m = std::fs::metadata(&logs).unwrap();
    assert_eq!(m.permissions().mode() & 0o7777, 0o1777);
    assert_eq!((m.uid(), m.gid()), (me.uid(), me.gid()));
    assert!(!d.path().join("plain").exists());
}

#[cfg(unix)]
#[test]
fn host_volume_prepare_refuses_symlinks_and_protected_paths() {
    let d = tempfile::tempdir().unwrap();
    let target = d.path().join("real");
    std::fs::create_dir(&target).unwrap();
    let link = d.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let vol = |p: PathBuf| HostVolume {
        path: p,
        mode: Some("0750".into()),
        ..Default::default()
    };
    let err = supervisor_with(d.path(), vec![("l", vol(link))])
        .prepare_host_volumes()
        .unwrap_err();
    assert!(err.contains("not a symlink"), "{err}");
    let err = supervisor_with(d.path(), vec![("p", vol(d.path().join("docker/volumes")))])
        .prepare_host_volumes()
        .unwrap_err();
    assert!(err.contains("protected"), "{err}");
}

/// command, entrypoint and each port's bind address reach the runtime;
/// "advertise" becomes the node's address.
#[tokio::test]
async fn passes_command_and_bind_addresses() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    let mut cmd = start_cmd("w1", 3);
    cmd.command = vec!["postgres".into(), "-c".into(), "wal_level=logical".into()];
    cmd.ports = vec![
        pb::PortMapping {
            name: "pg".into(),
            container_port: 5432,
            host_port: 5432,
            host_ip: "advertise".into(),
        },
        pb::PortMapping {
            name: "admin".into(),
            container_port: 9000,
            host_port: 9000,
            host_ip: "127.0.0.1".into(),
        },
        pb::PortMapping {
            name: "any".into(),
            container_port: 80,
            host_port: 8080,
            host_ip: String::new(),
        },
    ];
    s.start(cmd).await.unwrap();
    settle().await;
    assert_eq!(state_of(&s, "w1").unwrap().0, WorkloadState::Running);
    let spec = rt.last_created();
    assert_eq!(spec.command.as_ref().unwrap()[0], "postgres");
    assert_eq!(spec.entrypoint, None, "unset keeps the image's entrypoint");
    let ips: Vec<Option<&str>> = spec.ports.iter().map(|p| p.host_ip.as_deref()).collect();
    assert_eq!(ips, vec![Some("10.0.0.5"), Some("127.0.0.1"), None]);
}

/// A resolved secret becomes an environment variable of the container.
#[tokio::test]
async fn secrets_become_environment_variables() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor_with_secrets(rt.clone(), d.path());
    std::fs::write(d.path().join("secrets/pw"), "hunter2\n").unwrap();
    let mut cmd = start_cmd("w1", 3);
    cmd.env.insert("PGDATA".into(), "/data".into());
    cmd.secrets.insert(
        "POSTGRES_PASSWORD".into(),
        format!("file:{}", d.path().join("secrets/pw").display()),
    );
    s.start(cmd).await.unwrap();
    settle().await;
    assert_eq!(state_of(&s, "w1").unwrap().0, WorkloadState::Running);
    let env = rt.last_created().env;
    assert!(
        env.contains(&("POSTGRES_PASSWORD".into(), "hunter2".into())),
        "{env:?}"
    );
    assert!(env.contains(&("PGDATA".into(), "/data".into())));
}

/// An unresolvable reference fails the start before anything is created, and
/// the reason names the reference.
#[tokio::test]
async fn unresolvable_secret_fails_the_start() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor_with_secrets(rt.clone(), d.path());
    let mut cmd = start_cmd("w1", 3);
    cmd.secrets
        .insert("TOKEN".into(), "file:/etc/shadow".into());
    s.start(cmd).await.unwrap();
    settle().await;
    let (state, reason, _) = state_of(&s, "w1").unwrap();
    assert_eq!(state, WorkloadState::Failed);
    assert!(
        reason.starts_with("SECRET_UNRESOLVED: TOKEN (file:/etc/shadow)"),
        "{reason}"
    );
    assert_eq!(rt.count(), 0, "nothing is created");
}

/// A workload that passes its check is healthy; when the check keeps failing,
/// the agent stops it and restarts it under the restart policy.
#[tokio::test]
async fn failing_health_check_restarts_the_container() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    tokio::spawn(s.clone().run_events());
    tokio::spawn(s.clone().run_health_checks());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut cmd = start_cmd("w1", 3);
    cmd.ports = vec![pb::PortMapping {
        name: "http".into(),
        container_port: 80,
        host_port: port as u32,
        host_ip: "127.0.0.1".into(),
    }];
    cmd.check = Some(pb::HealthCheck {
        r#type: "tcp".into(),
        port_name: "http".into(),
        interval_seconds: 1,
        timeout_seconds: 1,
        failures: 2,
        ..Default::default()
    });
    s.start(cmd).await.unwrap();
    settle().await;
    assert_eq!(health(&s).0, pb::HealthState::Starting);
    assert!(
        rt.last_created().labels.contains_key("valkvork.check"),
        "the check is kept for adoption"
    );

    wait_health(&s, |h| h.0 == pb::HealthState::Healthy).await;

    drop(listener); // the "application" stops answering
    let (_, restarts, reason) = wait_health(&s, |h| h.1 == 1).await;
    assert_eq!(restarts, 1);
    assert!(reason.starts_with("UNHEALTHY"), "{reason}");
}

type HealthReport = (pb::HealthState, u32, String);

fn health(s: &Supervisor) -> HealthReport {
    let w = s
        .report()
        .into_iter()
        .find(|w| w.workload_id == "w1")
        .unwrap();
    (
        pb::HealthState::try_from(w.health).unwrap(),
        w.restart_count,
        w.reason,
    )
}

async fn wait_health(s: &Supervisor, want: impl Fn(&HealthReport) -> bool) -> HealthReport {
    for _ in 0..60 {
        let h = health(s);
        if want(&h) {
            return h;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out; last {:?}", health(s));
}

/// A paused container is not an exit: resync leaves it alone, and a failing
/// health check is what restarts it. Before, resync took "paused" for an exit,
/// the in-place restart failed on the paused container, and the workload
/// went FAILED (START_FAILED) instead of UNHEALTHY.
#[tokio::test]
async fn paused_container_is_not_an_exit() {
    let d = tempfile::tempdir().unwrap();
    let rt = Fake::new();
    let s = supervisor(rt.clone(), d.path());
    s.start(start_cmd("w1", 3)).await.unwrap();
    settle().await;
    let starts = rt.starts();
    for c in rt.s.lock().unwrap().containers.values_mut() {
        c.state = RunState::Paused;
    }
    s.resync().await;
    settle().await;
    assert_eq!(state_of(&s, "w1").unwrap().0, WorkloadState::Running);
    assert_eq!(rt.starts(), starts, "not restarted");

    // Adoption keeps a paused container too.
    let s = supervisor(rt.clone(), d.path());
    assert_eq!(s.adopt().await.unwrap(), 1);
    assert_eq!(state_of(&s, "w1").unwrap().0, WorkloadState::Running);
    assert_eq!(rt.starts(), starts);
}
