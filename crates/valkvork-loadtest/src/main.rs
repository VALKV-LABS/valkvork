//! Load harness for the controller (NFR-01, AT-11).
//!
//! Drives one real controller from one machine, at the spec's load:
//!
//! * `--nodes` simulated agents, each on its own agent stream: `Register`,
//!   heartbeats at the interval the controller asks for, acks for every
//!   command, and RUNNING reports for the workloads they were told to start.
//!   They enrol for a client certificate when the controller issues them and
//!   reconnect once with it, so the reconnect storm and mTLS are part of the
//!   load.
//! * `--services` services with `--workloads` replicas between them, applied
//!   through the admin API, plus churn: rolling updates and scale changes.
//! * `--ui-sessions` web UI sessions: signed in, holding the live-update
//!   stream open and fetching their page on every change, as the browser does.
//!
//! Meanwhile it samples the controller's RSS with `--rss-command` (output:
//! a `VmRSS:` line or a number of KiB) and fails if the peak is over
//! `--limit-mib`. Containers are not run: the agents are simulated.

use clap::Parser;
use hyper::body::HttpBody;
use pb::admin_service_client::AdminServiceClient;
use pb::agent_service_client::AgentServiceClient;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tonic::transport::{Channel, Endpoint, Identity};
use tower::{Service, ServiceExt};
use valkvork_common::node_cert::new_key_and_csr;
use valkvork_common::tls::client_tls_config;
use valkvork_proto::v1 as pb;

#[derive(Parser, Clone)]
#[command(about = "Load the controller at the NFR-01 scale and measure its memory")]
struct Args {
    /// The agent listener, e.g. https://localhost:50071
    #[arg(long)]
    agent_endpoint: String,
    /// The admin listener (API and UI), e.g. https://localhost:50072
    #[arg(long)]
    admin_endpoint: String,
    /// CA bundle for the controller's certificate.
    #[arg(long)]
    ca_file: Option<PathBuf>,
    /// Name to verify in the controller's certificate.
    #[arg(long)]
    tls_domain: Option<String>,
    #[arg(long)]
    join_token_file: PathBuf,
    #[arg(long)]
    admin_token_file: PathBuf,
    #[arg(long, default_value_t = 50)]
    nodes: usize,
    #[arg(long, default_value_t = 500)]
    workloads: u32,
    #[arg(long, default_value_t = 10)]
    services: u32,
    #[arg(long, default_value_t = 10)]
    ui_sessions: usize,
    /// How long to hold the full load, measuring, once every workload runs.
    #[arg(long, default_value_t = 120)]
    duration_seconds: u64,
    /// Rolling updates and scale changes during the window.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    churn: bool,
    /// Prints the controller's RSS: `ps -o rss= -p PID`, or
    /// `docker exec NAME cat /proc/1/status`.
    #[arg(long)]
    rss_command: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit_mib: u64,
}

#[derive(Default)]
struct Stats {
    registered: AtomicU64,
    reconnects: AtomicU64,
    heartbeats: AtomicU64,
    starts: AtomicU64,
    stops: AtomicU64,
    certificates: AtomicU64,
    ui_requests: AtomicU64,
    ui_changes: AtomicU64,
    ui_errors: AtomicU64,
    /// StopWorkload commands per workload id.
    stopped: Mutex<HashMap<String, u32>>,
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("loadtest: {msg}");
    std::process::exit(2)
}

fn read_secret(path: &PathBuf) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| die(format!("{}: {e}", path.display())))
        .trim()
        .to_string()
}

fn endpoint(args: &Args, url: &str, identity: Option<Identity>) -> Endpoint {
    let mut ep = Endpoint::from_shared(url.to_string())
        .unwrap_or_else(|e| die(format!("{url}: {e}")))
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(10))
        .keep_alive_timeout(Duration::from_secs(5));
    if url.starts_with("https://") {
        let mut tls = client_tls_config(args.ca_file.as_deref(), args.tls_domain.as_deref())
            .unwrap_or_else(|e| die(e));
        if let Some(id) = identity {
            tls = tls.identity(id);
        }
        ep = ep.tls_config(tls).unwrap_or_else(|e| die(e));
    }
    ep
}

// ============================================================================
// RSS
// ============================================================================

/// A phase of the run and the RSS sample at its end.
type Mark = (&'static str, Option<u64>);

#[derive(Default, Clone)]
struct Rss {
    samples: Vec<(Instant, u64)>,
}

/// KiB from `VmRSS:   12345 kB`, or the first number in the output.
fn parse_rss_kib(out: &str) -> Option<u64> {
    let line = out.lines().find(|l| l.starts_with("VmRSS:")).unwrap_or(out);
    line.split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

async fn sample_rss(cmd: &str) -> Option<u64> {
    let out = tokio::process::Command::new("sh")
        .args(["-c", cmd])
        .output()
        .await
        .ok()?;
    parse_rss_kib(&String::from_utf8_lossy(&out.stdout))
}

fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

// ============================================================================
// Simulated agents
// ============================================================================

struct FakeWorkload {
    service_id: String,
    version: u64,
}

struct FakeAgent {
    i: usize,
    node_id: String,
    workloads: Mutex<HashMap<String, FakeWorkload>>,
    /// (certificate, key) once enrolled; the key of a pending request.
    cert: Mutex<Option<(String, String)>>,
    pending_key: Mutex<Option<String>>,
}

impl FakeAgent {
    fn statuses(&self) -> Vec<pb::WorkloadStatus> {
        let wl = self.workloads.lock().unwrap();
        wl.iter()
            .map(|(id, w)| pb::WorkloadStatus {
                workload_id: id.clone(),
                service_id: w.service_id.clone(),
                service_version: w.version,
                container_id: format!("c-{id}"),
                state: pb::WorkloadState::Running as i32,
                cpu_usage_millicores: 5 + (id.len() as u32 + self.i as u32) % 20,
                memory_used_bytes: 24 << 20,
                disk_used_bytes: 1 << 20,
                ..Default::default()
            })
            .collect()
    }
}

async fn run_agent(args: Arc<Args>, a: Arc<FakeAgent>, join_token: String, stats: Arc<Stats>) {
    let mut first = true;
    loop {
        if !first {
            stats.reconnects.fetch_add(1, Relaxed);
        }
        first = false;
        match agent_session(&args, &a, &join_token, &stats).await {
            Ok(()) => {}
            Err(e) => eprintln!("agent {}: {e}", a.i),
        }
        tokio::time::sleep(Duration::from_millis(500 + (a.i as u64 * 37) % 1000)).await;
    }
}

/// Returns Ok after enrolling, so the next session presents the certificate.
async fn agent_session(
    args: &Args,
    a: &Arc<FakeAgent>,
    join_token: &str,
    stats: &Arc<Stats>,
) -> Result<(), String> {
    let identity = a
        .cert
        .lock()
        .unwrap()
        .as_ref()
        .map(|(c, k)| Identity::from_pem(c, k));
    let has_cert = identity.is_some();
    let channel = endpoint(args, &args.agent_endpoint, identity)
        .connect()
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let (tx, rx) = mpsc::channel::<pb::AgentMessage>(64);
    tx.send(pb::AgentMessage {
        msg: Some(pb::agent_message::Msg::Register(pb::Register {
            node_id: a.node_id.clone(),
            join_token: join_token.to_string(),
            hostname: format!("load-{:02}", a.i),
            ip_address: format!("10.200.{}.{}", a.i / 250, a.i % 250 + 1),
            node_class: "general".into(),
            agent_version: "0.0.0-load".into(),
            allocatable_cpu_millicores: 16_000,
            allocatable_memory_bytes: 64 << 30,
            host_volumes: vec![],
            inventory: a.statuses(),
            runtime: "loadtest/1".into(),
            zone: format!("load-zone-{}", a.i % 3),
            instance_id: format!("i-load{:04}", a.i),
            max_block_volumes: 20,
            features: valkvork_common::models::AGENT_FEATURES
                .iter()
                .map(|f| f.to_string())
                .collect(),
        })),
    })
    .await
    .map_err(|e| e.to_string())?;
    let mut inbound = AgentServiceClient::new(channel)
        .connect_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .map_err(|e| format!("stream: {e}"))?
        .into_inner();
    let ack = match inbound.message().await.map_err(|e| e.to_string())? {
        Some(pb::ControllerMessage {
            msg: Some(pb::controller_message::Msg::RegisterAck(ack)),
        }) => ack,
        other => return Err(format!("expected RegisterAck, got {other:?}")),
    };
    if !ack.accepted {
        return Err(format!("rejected: {}", ack.reject_reason));
    }
    stats.registered.fetch_add(1, Relaxed);
    if ack.issues_certificates && !has_cert {
        let (key, csr) = new_key_and_csr(&a.node_id).map_err(|e| e.to_string())?;
        *a.pending_key.lock().unwrap() = Some(key);
        tx.send(pb::AgentMessage {
            msg: Some(pb::agent_message::Msg::CertificateRequest(
                pb::CertificateRequest { csr_pem: csr },
            )),
        })
        .await
        .map_err(|e| e.to_string())?;
    }

    let hb = {
        let (tx, a, stats) = (tx.clone(), a.clone(), stats.clone());
        let interval = Duration::from_millis(ack.heartbeat_interval_ms.max(500) as u64);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            let mut seq = 0;
            loop {
                tick.tick().await;
                seq += 1;
                let hb = pb::Heartbeat {
                    seq,
                    node_cpu_usage_percent: 20.0,
                    node_used_memory_bytes: 8 << 30,
                    workloads: a.statuses(),
                };
                let msg = pb::AgentMessage {
                    msg: Some(pb::agent_message::Msg::Heartbeat(hb)),
                };
                if tx.send(msg).await.is_err() {
                    break;
                }
                stats.heartbeats.fetch_add(1, Relaxed);
            }
        })
    };

    let result = loop {
        let msg = match inbound.message().await {
            Ok(Some(m)) => m,
            Ok(None) => break Err("stream closed".to_string()),
            Err(e) => break Err(e.to_string()),
        };
        let (command_id, workload_id) = match msg.msg {
            Some(pb::controller_message::Msg::StartWorkload(s)) => {
                stats.starts.fetch_add(1, Relaxed);
                a.workloads.lock().unwrap().insert(
                    s.workload_id.clone(),
                    FakeWorkload {
                        service_id: s.service_id.clone(),
                        version: s.service_version,
                    },
                );
                (s.command_id, s.workload_id)
            }
            Some(pb::controller_message::Msg::StopWorkload(s)) => {
                stats.stops.fetch_add(1, Relaxed);
                *stats
                    .stopped
                    .lock()
                    .unwrap()
                    .entry(s.workload_id.clone())
                    .or_default() += 1;
                // Gone from the next heartbeat: the controller confirms STOPPED.
                a.workloads.lock().unwrap().remove(&s.workload_id);
                (s.command_id, s.workload_id)
            }
            Some(pb::controller_message::Msg::NodeCertificate(c)) => {
                let key = a.pending_key.lock().unwrap().take();
                if let (true, Some(key)) = (c.error.is_empty(), key) {
                    *a.cert.lock().unwrap() = Some((c.certificate_pem, key));
                    stats.certificates.fetch_add(1, Relaxed);
                    // Reconnect with it, as a restarted agent would.
                    break Ok(());
                }
                continue;
            }
            _ => continue,
        };
        let res = pb::AgentMessage {
            msg: Some(pb::agent_message::Msg::CommandResult(pb::CommandResult {
                command_id,
                workload_id,
                accepted: true,
                error: String::new(),
            })),
        };
        if tx.send(res).await.is_err() {
            break Err("send failed".into());
        }
    };
    hb.abort();
    result
}

// ============================================================================
// Admin API
// ============================================================================

/// Adds the admin bearer token to every request.
#[derive(Clone)]
struct Auth(tonic::metadata::MetadataValue<tonic::metadata::Ascii>);

impl tonic::service::Interceptor for Auth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        req.metadata_mut().insert("authorization", self.0.clone());
        Ok(req)
    }
}

type Admin = AdminServiceClient<tonic::codegen::InterceptedService<Channel, Auth>>;

async fn admin(args: &Args, token: &str) -> Admin {
    let channel = endpoint(args, &args.admin_endpoint, None)
        .connect()
        .await
        .unwrap_or_else(|e| die(format!("admin: {e}")));
    let v = format!("Bearer {token}")
        .parse()
        .unwrap_or_else(|_| die("the admin token is not a valid header value"));
    AdminServiceClient::with_interceptor(channel, Auth(v))
}

fn manifest(i: u32, replicas: u32, version: u32) -> String {
    let env: String = (0..8)
        .map(|k| format!("SETTING_{k} = \"value-{k}-of-service-{i}\"\n"))
        .collect();
    format!(
        "[service]\nid = \"load-{i:02}\"\nimage = \"registry.invalid/load/app-{i}:v{version}\"\nreplicas = {replicas}\n\n\
         [resources]\ncpu = 0.05\nmemory = \"32MiB\"\n\n\
         [update]\nmax_parallel = 10\nmin_healthy_time = \"1s\"\n\n\
         [env]\nREVISION = \"{version}\"\n{env}"
    )
}

async fn count(admin: &mut Admin, status: &str) -> usize {
    admin
        .list_workloads(pb::ListWorkloadsRequest {
            status: status.into(),
            ..Default::default()
        })
        .await
        .map(|r| r.into_inner().workloads.len())
        .unwrap_or(0)
}

async fn healthy_nodes(admin: &mut Admin) -> usize {
    admin
        .list_nodes(pb::ListNodesRequest::default())
        .await
        .map(|r| {
            r.into_inner()
                .nodes
                .iter()
                .filter(|n| n.status == "HEALTHY")
                .count()
        })
        .unwrap_or(0)
}

// ============================================================================
// Web UI sessions
// ============================================================================

async fn ui_request(
    ch: &mut Channel,
    method: http::Method,
    path: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
    body: Option<serde_json::Value>,
) -> Result<http::Response<hyper::Body>, String> {
    let mut b = http::Request::builder().method(method).uri(path);
    if let Some(c) = cookie {
        b = b.header(http::header::COOKIE, c);
    }
    if let Some(t) = csrf {
        b = b.header("x-valkvork-csrf", t);
    }
    let req = match body {
        Some(v) => b
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(
                hyper::Body::from(v.to_string())
                    .map_err(|e| tonic::Status::internal(e.to_string()))
                    .boxed_unsync(),
            ),
        None => b.body(tonic::body::empty_body()),
    }
    .map_err(|e| e.to_string())?;
    let ch = ch.ready().await.map_err(|e| e.to_string())?;
    ch.call(req).await.map_err(|e| e.to_string())
}

async fn ui_session(args: Arc<Args>, i: usize, token: String, stats: Arc<Stats>) {
    let channel = endpoint(&args, &args.admin_endpoint, None)
        .connect()
        .await
        .unwrap_or_else(|e| die(format!("ui: {e}")));
    let mut ch = channel.clone();
    let r = ui_request(
        &mut ch,
        http::Method::POST,
        "/ui/api/login",
        None,
        None,
        Some(serde_json::json!({ "token": token })),
    )
    .await
    .unwrap_or_else(|e| die(format!("ui login: {e}")));
    if !r.status().is_success() {
        die(format!("ui login: {}", r.status()));
    }
    let cookie = r
        .headers()
        .get(http::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .unwrap_or_else(|| die("ui login: no session cookie"))
        .to_string();

    // The live-update stream: a change makes the page refetch, at most once
    // a second, as the browser debounces it.
    let changed = Arc::new(tokio::sync::Notify::new());
    {
        let (mut ch, cookie, changed, stats) = (
            channel.clone(),
            cookie.clone(),
            changed.clone(),
            stats.clone(),
        );
        tokio::spawn(async move {
            loop {
                match ui_request(
                    &mut ch,
                    http::Method::GET,
                    "/ui/api/events",
                    Some(&cookie),
                    None,
                    None,
                )
                .await
                {
                    Ok(mut r) => {
                        while let Some(Ok(chunk)) = r.body_mut().data().await {
                            if chunk.windows(6).any(|w| w == b"change") {
                                stats.ui_changes.fetch_add(1, Relaxed);
                                changed.notify_one();
                            }
                        }
                    }
                    Err(_) => {
                        stats.ui_errors.fetch_add(1, Relaxed);
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    // Each session shows one page; the overview also on every fifth refresh.
    let pages = [
        "overview",
        "services",
        "workloads?",
        "nodes",
        "cluster-events?limit=200",
    ];
    let page = pages[i % pages.len()];
    let mut n = 0u64;
    loop {
        tokio::select! {
            _ = changed.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        n += 1;
        let p = if n.is_multiple_of(5) {
            "overview"
        } else {
            page
        };
        match ui_request(
            &mut ch,
            http::Method::GET,
            &format!("/ui/api/{p}"),
            Some(&cookie),
            None,
            None,
        )
        .await
        {
            Ok(r) if r.status().is_success() => {
                let _ = hyper::body::to_bytes(r.into_body()).await;
                stats.ui_requests.fetch_add(1, Relaxed);
            }
            _ => {
                stats.ui_errors.fetch_add(1, Relaxed);
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

// ============================================================================
// The run
// ============================================================================

#[tokio::main]
async fn main() {
    let args = Arc::new(Args::parse());
    let join_token = read_secret(&args.join_token_file);
    let admin_token = read_secret(&args.admin_token_file);
    let stats = Arc::new(Stats::default());
    let rss = Arc::new(Mutex::new(Rss::default()));
    let phase = Arc::new(Mutex::new("start"));
    let marks: Arc<Mutex<Vec<Mark>>> = Arc::new(Mutex::new(vec![]));

    if let Some(cmd) = args.rss_command.clone() {
        let rss = rss.clone();
        tokio::spawn(async move {
            loop {
                if let Some(k) = sample_rss(&cmd).await {
                    rss.lock().unwrap().samples.push((Instant::now(), k));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
    let mark = |name: &'static str| {
        let last = rss.lock().unwrap().samples.last().map(|s| s.1);
        *phase.lock().unwrap() = name;
        println!(
            "== {name}: rss {}",
            last.map(|k| format!("{:.1} MiB", mib(k)))
                .unwrap_or("-".into())
        );
        marks.lock().unwrap().push((name, last));
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    mark("idle");

    let mut admin = admin(&args, &admin_token).await;
    let t0 = Instant::now();

    // 1. Agents.
    for i in 0..args.nodes {
        let a = Arc::new(FakeAgent {
            i,
            node_id: uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_OID,
                format!("valkvork-load-{i}").as_bytes(),
            )
            .to_string(),
            workloads: Mutex::new(HashMap::new()),
            cert: Mutex::new(None),
            pending_key: Mutex::new(None),
        });
        tokio::spawn(run_agent(
            args.clone(),
            a,
            join_token.clone(),
            stats.clone(),
        ));
    }
    wait("nodes HEALTHY", Duration::from_secs(120), || async {
        let mut a = admin.clone();
        healthy_nodes(&mut a).await >= args.nodes
    })
    .await;
    // Let the enrolment reconnects settle.
    tokio::time::sleep(Duration::from_secs(5)).await;
    mark("nodes registered");

    // 2. Workloads.
    let per = args.workloads / args.services.max(1);
    let mut versions = vec![1u32; args.services as usize];
    for s in 0..args.services {
        admin
            .apply(pb::ApplyRequest {
                manifest_toml: manifest(s, per, 1),
            })
            .await
            .unwrap_or_else(|e| die(format!("apply load-{s:02}: {e}")));
    }
    let total = (per * args.services) as usize;
    wait("workloads RUNNING", Duration::from_secs(300), || async {
        let mut a = admin.clone();
        count(&mut a, "RUNNING").await >= total
    })
    .await;
    mark("workloads running");

    // 3. UI sessions.
    for i in 0..args.ui_sessions {
        tokio::spawn(ui_session(
            args.clone(),
            i,
            admin_token.clone(),
            stats.clone(),
        ));
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    mark("ui sessions open");

    // 4. Hold the load, with churn.
    let window_start = Instant::now();
    let mut step = 0u32;
    while window_start.elapsed() < Duration::from_secs(args.duration_seconds) {
        tokio::time::sleep(Duration::from_secs(15)).await;
        if !args.churn {
            continue;
        }
        let s = step % args.services.max(1);
        let res = if step.is_multiple_of(2) {
            versions[s as usize] += 1;
            admin
                .apply(pb::ApplyRequest {
                    manifest_toml: manifest(s, per, versions[s as usize]),
                })
                .await
                .map(|_| format!("rolling update load-{s:02} to v{}", versions[s as usize]))
        } else {
            let to = if step % 4 == 1 { per + per / 2 } else { per };
            admin
                .scale_service(pb::ScaleServiceRequest {
                    service_id: format!("load-{s:02}"),
                    replicas: to,
                })
                .await
                .map(|_| format!("scale load-{s:02} to {to}"))
        };
        match res {
            Ok(what) => println!("   churn: {what}"),
            Err(e) => println!("   churn failed: {e}"),
        }
        step += 1;
    }
    mark("end of window");

    // Report.
    let samples = rss.lock().unwrap().samples.clone();
    let window: Vec<u64> = samples
        .iter()
        .filter(|(t, _)| *t >= window_start)
        .map(|s| s.1)
        .collect();
    let peak = samples.iter().map(|s| s.1).max();
    let window_peak = window.iter().copied().max();
    let window_avg = (!window.is_empty()).then(|| window.iter().sum::<u64>() / window.len() as u64);
    println!();
    println!(
        "Load: {} nodes, {} workloads in {} services, {} UI sessions, {}s window (churn {})",
        args.nodes, total, args.services, args.ui_sessions, args.duration_seconds, args.churn
    );
    println!(
        "Agents: {} registrations, {} reconnects, {} certificates, {} heartbeats, {} starts, {} stops",
        stats.registered.load(Relaxed),
        stats.reconnects.load(Relaxed),
        stats.certificates.load(Relaxed),
        stats.heartbeats.load(Relaxed),
        stats.starts.load(Relaxed),
        stats.stops.load(Relaxed)
    );
    {
        let stopped = stats.stopped.lock().unwrap();
        let max = stopped.values().copied().max().unwrap_or(0);
        println!(
            "Stops: {} workloads stopped, {:.1} StopWorkload each on average, at most {max}",
            stopped.len(),
            stats.stops.load(Relaxed) as f64 / stopped.len().max(1) as f64
        );
    }
    println!(
        "UI: {} page fetches, {} change events, {} errors",
        stats.ui_requests.load(Relaxed),
        stats.ui_changes.load(Relaxed),
        stats.ui_errors.load(Relaxed)
    );
    println!("Elapsed: {:.0}s", t0.elapsed().as_secs_f64());
    for (name, k) in marks.lock().unwrap().iter() {
        println!(
            "  rss at {name:<18} {}",
            k.map(|k| format!("{:>6.1} MiB", mib(k)))
                .unwrap_or("-".into())
        );
    }
    let Some(peak) = peak else {
        println!("No RSS samples (no --rss-command)");
        return;
    };
    println!(
        "  rss in the window    avg {:>6.1} MiB, peak {:>6.1} MiB",
        mib(window_avg.unwrap_or(0)),
        mib(window_peak.unwrap_or(0))
    );
    println!(
        "  rss peak overall          {:>6.1} MiB (limit {} MiB)",
        mib(peak),
        args.limit_mib
    );
    if mib(peak) > args.limit_mib as f64 {
        println!("FAIL: the controller's peak RSS is over the limit");
        std::process::exit(1);
    }
    println!("PASS: the controller's peak RSS is within the limit");
    std::process::exit(0);
}

async fn wait<F, Fut>(what: &str, timeout: Duration, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    while !ready().await {
        if start.elapsed() > timeout {
            die(format!("timed out waiting for {what}"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    println!("   {what} after {:.0}s", start.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_rss() {
        assert_eq!(super::parse_rss_kib(" 23456\n"), Some(23456));
        assert_eq!(
            super::parse_rss_kib("Name:\tvalkvork\nVmPeak:\t 9 kB\nVmRSS:\t   31337 kB\n"),
            Some(31337)
        );
        assert_eq!(super::parse_rss_kib(""), None);
    }

    #[test]
    fn manifests_validate() {
        for v in [1, 2] {
            valkvork_common::validation::parse_manifest_str(&super::manifest(3, 50, v)).unwrap();
        }
    }
}
