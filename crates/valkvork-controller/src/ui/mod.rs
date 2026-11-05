//! Built-in web UI (FR-UI-01 … FR-UI-09).
//!
//! Plain hyper handlers, no web framework. The JSON endpoints under
//! `/ui/api/` are thin adapters over the same core operations as
//! `AdminService` (FR-UI-05); no business logic lives here.

#[cfg(test)]
mod tests;

use crate::core::{AdminOp, CoreHandle, WorkloadFilter};
use crate::db::EventReader;
use bytes::Bytes;
use hyper::body::HttpBody;
use hyper::{header, Body, Method, Request, Response, StatusCode};
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use valkvork_common::tls::tokens_equal;
use valkvork_common::ValkvorkError;

mod assets {
    pub struct Asset {
        pub name: &'static str,
        pub content_type: &'static str,
        pub raw: &'static [u8],
        pub gzip: &'static [u8],
        pub etag: &'static str,
    }
    include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

    pub fn get(name: &str) -> Option<&'static Asset> {
        ASSETS.iter().find(|a| a.name == name)
    }
}

/// FR-UI-01: on every response.
pub const CSP: &str = "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'";
const COOKIE: &str = "vk_session";
const CSRF_HEADER: &str = "x-valkvork-csrf";
const MAX_BODY: usize = 1 << 20;
const MAX_SESSIONS: usize = 256;

struct Session {
    csrf: String,
    /// Public identifier recorded in audit events. Never the cookie value,
    /// so reading the event log does not reveal session credentials.
    audit_id: String,
    expires: Instant,
}

pub struct UiState {
    core: CoreHandle,
    reader: Arc<EventReader>,
    admin_token: String,
    sessions: Mutex<HashMap<String, Session>>,
    ttl: Duration,
    /// `Secure` cookies only work over HTTPS.
    secure_cookie: bool,
    db_path: PathBuf,
}

impl UiState {
    pub fn new(
        core: CoreHandle,
        reader: Arc<EventReader>,
        admin_token: String,
        ttl: Duration,
        tls: bool,
        db_path: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            core,
            reader,
            admin_token,
            sessions: Mutex::new(HashMap::new()),
            ttl,
            secure_cookie: tls,
            db_path,
        })
    }

    fn create_session(&self) -> (String, String) {
        let id = random_hex(32);
        let csrf = random_hex(32);
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS {
            // Evict the session closest to expiry.
            if let Some(oldest) = sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(k, _)| k.clone())
            {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            id.clone(),
            Session {
                csrf: csrf.clone(),
                audit_id: random_hex(4),
                expires: now + self.ttl,
            },
        );
        (id, csrf)
    }

    /// (session id, csrf token, audit id) for a valid session cookie.
    fn session(&self, req: &Request<Body>) -> Option<(String, String, String)> {
        let id = cookie(req, COOKIE)?;
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(&id) {
            Some(s) if s.expires > Instant::now() => Some((id, s.csrf.clone(), s.audit_id.clone())),
            Some(_) => {
                sessions.remove(&id);
                None
            }
            None => None,
        }
    }

    fn session_cookie(&self, value: &str, max_age: u64) -> String {
        let secure = if self.secure_cookie { "; Secure" } else { "" };
        format!("{COOKIE}={value}; Path=/ui; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

fn cookie(req: &Request<Body>, name: &str) -> Option<String> {
    req.headers()
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

/// Entry point for every non-gRPC request on the admin listener.
pub async fn handle(state: Arc<UiState>, req: Request<Body>) -> Response<Body> {
    let mut resp = route(&state, req).await;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    h.insert(
        header::REFERRER_POLICY,
        header::HeaderValue::from_static("no-referrer"),
    );
    resp
}

async fn route(state: &Arc<UiState>, req: Request<Body>) -> Response<Body> {
    let path = req.uri().path().to_string();
    let read = matches!(*req.method(), Method::GET | Method::HEAD);
    if read && (path == "/" || path == "/ui") {
        return redirect("/ui/");
    }
    if let Some(name) = path.strip_prefix("/ui/assets/") {
        return if read {
            asset(&req, name)
        } else {
            text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
        };
    }
    if let Some(rest) = path.strip_prefix("/ui/api/") {
        let rest = rest.to_string();
        return match api(state, req, &rest).await {
            Ok(r) => r,
            Err(e) => e.into_response(),
        };
    }
    if read && path.starts_with("/ui/") {
        // Deep links (/ui/services/<id>, …) are routed by the page script.
        return asset(&req, "index.html");
    }
    text(StatusCode::NOT_FOUND, "not found")
}

pub fn not_found() -> Response<Body> {
    let mut r = text(StatusCode::NOT_FOUND, "not found");
    r.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(CSP),
    );
    r
}

fn text(status: StatusCode, msg: &'static str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(msg))
        .unwrap()
}

fn redirect(to: &'static str) -> Response<Body> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, to)
        .body(Body::empty())
        .unwrap()
}

fn asset(req: &Request<Body>, name: &str) -> Response<Body> {
    let Some(a) = assets::get(name) else {
        return text(StatusCode::NOT_FOUND, "not found");
    };
    let builder = Response::builder()
        .header(header::CONTENT_TYPE, a.content_type)
        .header(header::ETAG, a.etag)
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::VARY, "Accept-Encoding");
    let not_modified = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(a.etag);
    if not_modified {
        return builder
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap();
    }
    let gzip = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip")));
    let body = if gzip { a.gzip } else { a.raw };
    let builder = if gzip {
        builder.header(header::CONTENT_ENCODING, "gzip")
    } else {
        builder
    };
    let body = if *req.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(body)
    };
    builder.body(body).unwrap()
}

// ============================================================================
// JSON API
// ============================================================================

struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": msg.into() }),
        }
    }

    fn into_response(self) -> Response<Body> {
        json_response(self.status, &self.body)
    }
}

impl From<ValkvorkError> for ApiError {
    fn from(e: ValkvorkError) -> Self {
        match e {
            ValkvorkError::Validation(errors) => Self {
                status: StatusCode::BAD_REQUEST,
                body: json!({
                    "error": "validation failed",
                    "errors": errors.iter().map(|e| json!({
                        "code": e.code.to_string(),
                        "field": e.field_path,
                        "message": e.message,
                    })).collect::<Vec<_>>(),
                }),
            },
            ValkvorkError::NotFound(m) => {
                Self::new(StatusCode::NOT_FOUND, format!("not found: {m}"))
            }
            ValkvorkError::FailedPrecondition(m) => Self::new(StatusCode::CONFLICT, m),
            other => {
                tracing::warn!(error = %other, "UI request failed");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, other.to_string())
            }
        }
    }
}

type ApiResult = Result<Response<Body>, ApiError>;

fn json_response(status: StatusCode, v: &impl serde::Serialize) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(serde_json::to_vec(v).unwrap_or_default()))
        .unwrap()
}

fn ok(v: &impl serde::Serialize) -> ApiResult {
    Ok(json_response(StatusCode::OK, v))
}

async fn read_json<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let mut body = body;
    let mut buf = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
        if buf.len() + chunk.len() > MAX_BODY {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body too large",
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
}

fn query(req: &Request<Body>) -> HashMap<String, String> {
    req.uri()
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                match (
                    bytes.get(i + 1).and_then(|c| hex(*c)),
                    bytes.get(i + 2).and_then(|c| hex(*c)),
                ) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn non_empty(m: &HashMap<String, String>, k: &str) -> Option<String> {
    m.get(k).filter(|v| !v.is_empty()).cloned()
}

#[derive(Deserialize)]
struct LoginBody {
    token: String,
}

#[derive(Deserialize)]
struct ManifestBody {
    manifest: String,
}

#[derive(Deserialize)]
struct ScaleBody {
    replicas: u32,
}

#[derive(Deserialize)]
struct DeleteBody {
    confirm: String,
}

async fn api(state: &Arc<UiState>, req: Request<Body>, rest: &str) -> ApiResult {
    let method = req.method().clone();
    let segs: Vec<String> = rest
        .split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode)
        .collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();

    // Unauthenticated endpoints.
    match (&method, segs.as_slice()) {
        (&Method::GET, ["session"]) => {
            return ok(&match state.session(&req) {
                Some((_, csrf, _)) => json!({ "authenticated": true, "csrf": csrf }),
                None => json!({ "authenticated": false }),
            });
        }
        (&Method::POST, ["login"]) => {
            let body: LoginBody = read_json(req.into_body()).await?;
            if !tokens_equal(body.token.trim(), &state.admin_token) {
                // Slow down guessing.
                tokio::time::sleep(Duration::from_millis(500)).await;
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "invalid admin token",
                ));
            }
            let (id, csrf) = state.create_session();
            let mut r = json_response(
                StatusCode::OK,
                &json!({ "authenticated": true, "csrf": csrf }),
            );
            r.headers_mut().insert(
                header::SET_COOKIE,
                state
                    .session_cookie(&id, state.ttl.as_secs())
                    .parse()
                    .unwrap(),
            );
            return Ok(r);
        }
        _ => {}
    }

    // Everything else needs a session (FR-UI-06) ...
    let Some((sid, csrf, audit_id)) = state.session(&req) else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "sign in with the admin token",
        ));
    };
    // ... and mutating requests the session's CSRF token.
    let mutating = !matches!(method, Method::GET | Method::HEAD);
    if mutating {
        let presented = req
            .headers()
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !tokens_equal(presented, &csrf) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "missing or invalid X-Valkvork-CSRF header",
            ));
        }
    }
    let actor = format!("ui:{audit_id}");
    let core = &state.core;
    let q = query(&req);

    match (&method, segs.as_slice()) {
        (&Method::POST, ["logout"]) => {
            state.sessions.lock().unwrap().remove(&sid);
            let mut r = json_response(StatusCode::OK, &json!({ "authenticated": false }));
            r.headers_mut().insert(
                header::SET_COOKIE,
                state.session_cookie("", 0).parse().unwrap(),
            );
            Ok(r)
        }
        (&Method::GET, ["events"]) => Ok(sse(state)),

        (&Method::GET, ["overview"]) => {
            let overview = core.ask(|reply| AdminOp::Overview { reply }).await?;
            let reader = state.reader.clone();
            let events = blocking(move || reader.events(None, 0, 20)).await?;
            let db_bytes = ["", "-wal"]
                .iter()
                .filter_map(|suffix| {
                    std::fs::metadata(format!("{}{suffix}", state.db_path.display())).ok()
                })
                .map(|m| m.len())
                .sum::<u64>();
            ok(&json!({
                "overview": overview,
                "events": events.iter().map(event_json).collect::<Vec<_>>(),
                "controller": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "db_bytes": db_bytes,
                    "tls": state.secure_cookie,
                },
            }))
        }

        (&Method::GET, ["services"]) => {
            let services = core
                .ask(|reply| AdminOp::ListServices {
                    service_id: None,
                    reply,
                })
                .await?;
            ok(&services)
        }
        (&Method::GET, ["services", id]) => {
            let id = id.to_string();
            let detail = core
                .ask(|reply| AdminOp::GetService {
                    service_id: id.clone(),
                    reply,
                })
                .await?;
            let series = core
                .ask(|reply| AdminOp::ServiceSeries {
                    service_id: id.clone(),
                    reply,
                })
                .await?;
            let reader = state.reader.clone();
            let sid = id.clone();
            let scaling = blocking(move || reader.scaling_events(&sid, 20)).await?;
            ok(&json!({
                "service": detail,
                "cpu_series": series,
                "scaling_events": scaling.iter().map(|r| json!({
                    "from_replicas": r.from_replicas,
                    "to_replicas": r.to_replicas,
                    "metric_value": r.metric_value,
                    "reason": r.reason,
                    "created_at": r.created_at,
                })).collect::<Vec<_>>(),
            }))
        }
        (&Method::POST, ["services", id, "scale"]) => {
            let service_id = id.to_string();
            let body: ScaleBody = read_json(req.into_body()).await?;
            let (old, new) = core
                .ask(|reply| AdminOp::Scale {
                    service_id,
                    replicas: body.replicas,
                    actor,
                    reply,
                })
                .await?;
            ok(&json!({ "old_replicas": old, "new_replicas": new }))
        }
        (&Method::POST, ["services", id, "delete"]) => {
            let service_id = id.to_string();
            let body: DeleteBody = read_json(req.into_body()).await?;
            // FR-UI-03: confirmed by typing the service id.
            if body.confirm != service_id {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "type the service id to confirm the delete",
                ));
            }
            core.ask(|reply| AdminOp::Delete {
                service_id,
                actor,
                reply,
            })
            .await?;
            ok(&json!({ "accepted": true }))
        }

        (&Method::GET, ["nodes"]) => {
            let nodes = core
                .ask(|reply| AdminOp::ListNodes {
                    node_id: None,
                    node_class: None,
                    reply,
                })
                .await?;
            ok(&nodes)
        }
        (&Method::GET, ["nodes", id]) => {
            let node_id = id.to_string();
            let nid = node_id.clone();
            let mut nodes = core
                .ask(|reply| AdminOp::ListNodes {
                    node_id: Some(nid),
                    node_class: None,
                    reply,
                })
                .await?;
            let Some(node) = nodes.pop() else {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    format!("not found: node {node_id}"),
                ));
            };
            let filter = WorkloadFilter {
                node_id: Some(node_id),
                ..Default::default()
            };
            let workloads = core
                .ask(|reply| AdminOp::ListWorkloads { filter, reply })
                .await?;
            ok(&json!({ "node": node, "workloads": workloads }))
        }
        (&Method::POST, ["nodes", id, action @ ("drain" | "undrain")]) => {
            let node_id = id.to_string();
            let draining = *action == "drain";
            core.ask(|reply| AdminOp::SetDraining {
                node_id,
                draining,
                actor,
                reply,
            })
            .await?;
            ok(&json!({ "accepted": true, "draining": draining }))
        }

        (&Method::GET, ["workloads"]) => {
            let filter = WorkloadFilter {
                service_id: non_empty(&q, "service"),
                node_id: non_empty(&q, "node"),
                status: non_empty(&q, "status"),
            };
            let workloads = core
                .ask(|reply| AdminOp::ListWorkloads { filter, reply })
                .await?;
            ok(&workloads)
        }
        (&Method::GET, ["cluster-events"]) => {
            let kind = non_empty(&q, "kind").map(|k| k.to_uppercase());
            let limit = q
                .get("limit")
                .and_then(|l| l.parse().ok())
                .unwrap_or(100u32)
                .min(1000);
            let reader = state.reader.clone();
            let events = blocking(move || reader.events(kind.as_deref(), 0, limit)).await?;
            ok(&events.iter().map(event_json).collect::<Vec<_>>())
        }

        (&Method::POST, [op @ ("plan" | "apply")]) => {
            let dry_run = *op == "plan";
            let body: ManifestBody = read_json(req.into_body()).await?;
            let o = core
                .ask(|reply| AdminOp::Apply {
                    manifest: body.manifest,
                    dry_run,
                    actor,
                    reply,
                })
                .await?;
            ok(&json!({
                "service_id": o.service_id,
                "version": o.version,
                "change": o.change.to_string(),
                "warnings": o.warnings,
                "diff": o.diff,
                "to_place": o.to_place,
                "placeable": o.placeable,
                "target_replicas": o.target_replicas,
                "max_parallel": o.max_parallel,
                "applied": !dry_run,
            }))
        }

        _ => Err(ApiError::new(StatusCode::NOT_FOUND, "no such endpoint")),
    }
}

fn event_json(e: &crate::db::EventRow) -> Value {
    json!({
        "event_id": e.event_id,
        "kind": e.kind,
        "subject_type": e.subject_type,
        "subject_id": e.subject_id,
        "message": e.message,
        "actor": e.actor,
        "created_at": e.created_at,
    })
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ValkvorkError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(ApiError::from)
}

/// Server-Sent Events: one `change` event per committed state change
/// (FR-UI-04). A client that falls behind its bounded buffer is dropped and
/// reconnects (FR-UI-09); the control loop never waits for it.
fn sse(state: &Arc<UiState>) -> Response<Body> {
    use tokio::sync::broadcast::error::RecvError;
    let mut rx = state.core.subscribe();
    let (mut tx, body) = Body::channel();
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        if tx
            .send_data(Bytes::from_static(b"retry: 5000\n\n"))
            .await
            .is_err()
        {
            return;
        }
        loop {
            let chunk = tokio::select! {
                r = rx.recv() => match r {
                    Ok(seq) => Bytes::from(format!("event: change\ndata: {seq}\n\n")),
                    Err(RecvError::Lagged(_)) | Err(RecvError::Closed) => break,
                },
                _ = ping.tick() => Bytes::from_static(b": ping\n\n"),
            };
            if tx.send_data(chunk).await.is_err() {
                break;
            }
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .unwrap()
}
