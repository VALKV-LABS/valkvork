//! Agent Prometheus endpoint (NFR-07), bound to loopback only (FR-ARC-01).

use crate::supervisor::Supervisor;
use hyper::{header, Body, Request, Response, StatusCode};
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub static RECONNECTS: AtomicU64 = AtomicU64::new(0);
pub static CONNECTED: AtomicBool = AtomicBool::new(false);
pub static HEARTBEATS: AtomicU64 = AtomicU64::new(0);
pub static LOCAL_RESTARTS: AtomicU64 = AtomicU64::new(0);
static COMMANDS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Count a command ack: `start`/`stop` × accepted/rejected.
pub fn command(start: bool, accepted: bool) {
    COMMANDS[(start as usize) * 2 + accepted as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn render(supervisor: &Supervisor) -> String {
    let mut o = String::new();
    let family = |o: &mut String, name: &str, kind: &str, help: &str| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
    };
    family(
        &mut o,
        "valkvork_agent_reconnects_total",
        "counter",
        "Times the controller stream was re-established after a failure.",
    );
    let _ = writeln!(
        o,
        "valkvork_agent_reconnects_total {}",
        RECONNECTS.load(Ordering::Relaxed)
    );
    family(
        &mut o,
        "valkvork_agent_connected",
        "gauge",
        "1 while registered with the controller.",
    );
    let _ = writeln!(
        o,
        "valkvork_agent_connected {}",
        CONNECTED.load(Ordering::Relaxed) as u8
    );
    family(
        &mut o,
        "valkvork_agent_heartbeats_total",
        "counter",
        "Heartbeats sent.",
    );
    let _ = writeln!(
        o,
        "valkvork_agent_heartbeats_total {}",
        HEARTBEATS.load(Ordering::Relaxed)
    );
    family(
        &mut o,
        "valkvork_agent_commands_total",
        "counter",
        "Commands received, by command and result.",
    );
    for (i, (command, result)) in [
        ("stop", "rejected"),
        ("stop", "accepted"),
        ("start", "rejected"),
        ("start", "accepted"),
    ]
    .iter()
    .enumerate()
    {
        let _ = writeln!(
            o,
            "valkvork_agent_commands_total{{command=\"{command}\",result=\"{result}\"}} {}",
            COMMANDS[i].load(Ordering::Relaxed)
        );
    }
    family(
        &mut o,
        "valkvork_agent_local_restarts_total",
        "counter",
        "In-place restarts under the restart policy (FR-AGE-06).",
    );
    let _ = writeln!(
        o,
        "valkvork_agent_local_restarts_total {}",
        LOCAL_RESTARTS.load(Ordering::Relaxed)
    );
    family(
        &mut o,
        "valkvork_agent_workloads",
        "gauge",
        "Managed containers by state.",
    );
    for (state, n) in supervisor.state_counts() {
        let _ = writeln!(o, "valkvork_agent_workloads{{state=\"{state}\"}} {n}");
    }
    o
}

pub async fn serve(addr: SocketAddr, supervisor: Arc<Supervisor>) {
    let make = hyper::service::make_service_fn(move |_| {
        let supervisor = supervisor.clone();
        async move {
            Ok::<_, std::convert::Infallible>(hyper::service::service_fn(
                move |req: Request<Body>| {
                    let supervisor = supervisor.clone();
                    async move {
                        let resp = if req.uri().path() == "/metrics" {
                            Response::builder()
                                .header(
                                    header::CONTENT_TYPE,
                                    "text/plain; version=0.0.4; charset=utf-8",
                                )
                                .body(Body::from(render(&supervisor)))
                        } else {
                            Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(Body::from("not found\n"))
                        };
                        Ok::<_, std::convert::Infallible>(resp.unwrap())
                    }
                },
            ))
        }
    });
    tracing::info!(%addr, "metrics listening");
    if let Err(e) = hyper::Server::bind(&addr).serve(make).await {
        // Metrics are optional; never take the agent down for them.
        tracing::warn!(error = %e, "metrics listener stopped");
    }
}
