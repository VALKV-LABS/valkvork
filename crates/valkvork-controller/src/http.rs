//! Plain-HTTP pieces of the controller.
//!
//! * [`UiLayer`] splits the admin listener by content type (FR-UI-02):
//!   `application/grpc` goes to `AdminService`, everything else to the web
//!   UI, so there is one port, one certificate and one firewall rule.
//! * [`serve_metrics`] is the Prometheus endpoint (NFR-07).

use crate::core::{AdminOp, CoreHandle};
use crate::ui::{self, UiState};
use hyper::body::HttpBody;
use hyper::{header, Body, Request, Response, StatusCode};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tonic::body::BoxBody;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct UiLayer {
    ui: Option<Arc<UiState>>,
}

impl UiLayer {
    /// `None` disables the UI (`[ui] enabled = false`): non-gRPC requests get 404.
    pub fn new(ui: Option<Arc<UiState>>) -> Self {
        Self { ui }
    }
}

impl<S> Layer<S> for UiLayer {
    type Service = UiSplit<S>;

    fn layer(&self, inner: S) -> Self::Service {
        UiSplit {
            inner,
            ui: self.ui.clone(),
        }
    }
}

#[derive(Clone)]
pub struct UiSplit<S> {
    inner: S,
    ui: Option<Arc<UiState>>,
}

fn is_grpc(req: &Request<Body>) -> bool {
    req.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/grpc"))
}

type BoxFuture<T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send>>;

impl<S> Service<Request<Body>> for UiSplit<S>
where
    S: Service<Request<Body>, Response = Response<BoxBody>> + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<BoxBody>;
    type Error = S::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if is_grpc(&req) {
            return Box::pin(self.inner.call(req));
        }
        let ui = self.ui.clone();
        Box::pin(async move {
            let resp = match ui {
                Some(ui) => ui::handle(ui, req).await,
                None => ui::not_found(),
            };
            Ok(resp.map(|b| {
                b.map_err(|e| tonic::Status::internal(e.to_string()))
                    .boxed_unsync()
            }))
        })
    }
}

/// `GET /metrics` in the Prometheus text format (NFR-07).
pub async fn serve_metrics(
    addr: SocketAddr,
    core: CoreHandle,
    db_path: PathBuf,
) -> Result<(), hyper::Error> {
    let make = hyper::service::make_service_fn(move |_| {
        let core = core.clone();
        let db_path = db_path.clone();
        async move {
            Ok::<_, std::convert::Infallible>(hyper::service::service_fn(
                move |req: Request<Body>| {
                    let core = core.clone();
                    let db_path = db_path.clone();
                    async move {
                        Ok::<_, std::convert::Infallible>(
                            metrics_response(req, &core, &db_path).await,
                        )
                    }
                },
            ))
        }
    });
    tracing::info!(%addr, "metrics listening");
    hyper::Server::bind(&addr).serve(make).await
}

async fn metrics_response(
    req: Request<Body>,
    core: &CoreHandle,
    db_path: &std::path::Path,
) -> Response<Body> {
    if req.uri().path() != "/metrics" {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found\n"))
            .unwrap();
    }
    match core.ask(|reply| AdminOp::Metrics { reply }).await {
        Ok(mut text) => {
            let db_bytes: u64 = ["", "-wal"]
                .iter()
                .filter_map(|s| std::fs::metadata(format!("{}{s}", db_path.display())).ok())
                .map(|m| m.len())
                .sum();
            text.push_str("# HELP valkvork_db_size_bytes SQLite database plus WAL size.\n# TYPE valkvork_db_size_bytes gauge\n");
            text.push_str(&format!("valkvork_db_size_bytes {db_bytes}\n"));
            Response::builder()
                .header(
                    header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                )
                .body(Body::from(text))
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from(format!("{e}\n")))
            .unwrap(),
    }
}
