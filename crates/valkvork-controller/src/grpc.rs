//! gRPC adapters. No business logic lives here: requests are translated into
//! core messages and answers back into proto types.

// tonic's API returns `Result<_, tonic::Status>`; Status is large by design.
#![allow(clippy::result_large_err)]

use crate::core::{AdminOp, CoreHandle, CoreMsg, WorkloadFilter};
use crate::db::EventReader;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};
use valkvork_common::models::ChangeKind;
use valkvork_common::tls::tokens_equal;
use valkvork_proto::v1 as pb;

// ============================================================================
// AgentService
// ============================================================================

pub struct AgentApi {
    core: CoreHandle,
    next_conn: AtomicU64,
}

impl AgentApi {
    pub fn new(core: CoreHandle) -> Self {
        Self {
            core,
            next_conn: AtomicU64::new(1),
        }
    }
}

type ControllerStream = Pin<Box<dyn Stream<Item = Result<pb::ControllerMessage, Status>> + Send>>;

#[tonic::async_trait]
impl pb::agent_service_server::AgentService for AgentApi {
    type ConnectStream = ControllerStream;

    async fn connect(
        &self,
        request: Request<Streaming<pb::AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let peer = request.remote_addr();
        let mut inbound = request.into_inner();

        // FR-AGE-02: the first message on every stream MUST be Register.
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no Register within 10 s"))??;
        let register = match first.and_then(|m| m.msg) {
            Some(pb::agent_message::Msg::Register(r)) => r,
            _ => return Err(Status::invalid_argument("first message must be Register")),
        };
        let node_id = register.node_id.clone();
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        tracing::info!(node = %node_id, ?peer, conn_id, "agent stream opened");

        // Only the core holds the sender: dropping it there closes the stream.
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        self.core
            .send(CoreMsg::AgentConnected {
                register,
                conn_id,
                tx,
            })
            .await
            .map_err(Status::from)?;

        let core = self.core.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(pb::AgentMessage { msg: Some(msg) })) => {
                        if core
                            .send(CoreMsg::AgentMsg {
                                node_id: node_id.clone(),
                                conn_id,
                                msg,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Some(_)) => continue,
                    Ok(None) => break,
                    Err(e) => {
                        tracing::info!(node = %node_id, conn_id, "agent stream error: {e}");
                        break;
                    }
                }
            }
            let _ = core
                .send(CoreMsg::AgentDisconnected { node_id, conn_id })
                .await;
        });

        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}

// ============================================================================
// AdminService
// ============================================================================

pub struct AdminApi {
    core: CoreHandle,
    reader: Arc<EventReader>,
}

impl AdminApi {
    pub fn new(core: CoreHandle, reader: Arc<EventReader>) -> Self {
        Self { core, reader }
    }
}

/// FR-CON-10 / FR-SEC-03: every admin request needs `authorization: Bearer <token>`.
pub fn admin_auth(token: String) -> impl Fn(Request<()>) -> Result<Request<()>, Status> + Clone {
    move |req: Request<()>| {
        let presented = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if tokens_equal(presented, &token) {
            Ok(req)
        } else {
            Err(Status::unauthenticated("missing or invalid admin token"))
        }
    }
}

fn opt(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

const CLI: &str = "cli";

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, valkvork_common::ValkvorkError> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Status::internal(e.to_string()))?
        .map_err(Status::from)
}

#[tonic::async_trait]
impl pb::admin_service_server::AdminService for AdminApi {
    async fn apply(
        &self,
        request: Request<pb::ApplyRequest>,
    ) -> Result<Response<pb::ApplyResponse>, Status> {
        let manifest = request.into_inner().manifest_toml;
        let o = self
            .core
            .ask(|reply| AdminOp::Apply {
                manifest,
                dry_run: false,
                actor: CLI.into(),
                reply,
            })
            .await?;
        Ok(Response::new(pb::ApplyResponse {
            service_id: o.service_id,
            version: o.version,
            change: o.change.to_string(),
            warnings: o.warnings,
        }))
    }

    async fn plan(
        &self,
        request: Request<pb::ApplyRequest>,
    ) -> Result<Response<pb::PlanResponse>, Status> {
        let manifest = request.into_inner().manifest_toml;
        let o = self
            .core
            .ask(|reply| AdminOp::Apply {
                manifest,
                dry_run: true,
                actor: CLI.into(),
                reply,
            })
            .await?;
        let feasible = o.placeable == o.to_place;
        let reason = if feasible || o.change == ChangeKind::Unchanged {
            String::new()
        } else {
            format!(
                "only {}/{} workloads placeable with current capacity",
                o.placeable, o.to_place
            )
        };
        Ok(Response::new(pb::PlanResponse {
            service_id: o.service_id,
            version: o.version,
            change: o.change.to_string(),
            feasible,
            reason,
            warnings: o.warnings,
            diff: o.diff,
            to_place: o.to_place,
            placeable: o.placeable,
            target_replicas: o.target_replicas,
            max_parallel: o.max_parallel,
        }))
    }

    async fn delete_service(
        &self,
        request: Request<pb::DeleteServiceRequest>,
    ) -> Result<Response<pb::DeleteServiceResponse>, Status> {
        let service_id = request.into_inner().service_id;
        self.core
            .ask(|reply| AdminOp::Delete {
                service_id,
                actor: CLI.into(),
                reply,
            })
            .await?;
        Ok(Response::new(pb::DeleteServiceResponse {
            accepted: true,
            error: String::new(),
        }))
    }

    async fn get_service(
        &self,
        request: Request<pb::GetServiceRequest>,
    ) -> Result<Response<pb::ServiceDetail>, Status> {
        let service_id = request.into_inner().service_id;
        let id = service_id.clone();
        let mut detail = self
            .core
            .ask(|reply| AdminOp::GetService { service_id, reply })
            .await?;
        let reader = self.reader.clone();
        let rows = blocking(move || reader.scaling_events(&id, 20)).await?;
        detail.scaling_events = rows
            .into_iter()
            .map(|r| pb::ScalingEvent {
                event_id: r.event_id as u64,
                from_replicas: r.from_replicas,
                to_replicas: r.to_replicas,
                metric_value: r.metric_value,
                reason: r.reason,
                created_at: r.created_at as u64,
            })
            .collect();
        Ok(Response::new(detail))
    }

    async fn list_services(
        &self,
        request: Request<pb::ListServicesRequest>,
    ) -> Result<Response<pb::ListServicesResponse>, Status> {
        let service_id = opt(request.into_inner().service_id);
        let services = self
            .core
            .ask(|reply| AdminOp::ListServices { service_id, reply })
            .await?;
        Ok(Response::new(pb::ListServicesResponse { services }))
    }

    async fn list_workloads(
        &self,
        request: Request<pb::ListWorkloadsRequest>,
    ) -> Result<Response<pb::ListWorkloadsResponse>, Status> {
        let r = request.into_inner();
        let filter = WorkloadFilter {
            service_id: opt(r.service_id),
            node_id: opt(r.node_id),
            status: opt(r.status),
        };
        let workloads = self
            .core
            .ask(|reply| AdminOp::ListWorkloads { filter, reply })
            .await?;
        Ok(Response::new(pb::ListWorkloadsResponse { workloads }))
    }

    async fn list_nodes(
        &self,
        request: Request<pb::ListNodesRequest>,
    ) -> Result<Response<pb::ListNodesResponse>, Status> {
        let r = request.into_inner();
        let (node_id, node_class) = (opt(r.node_id), opt(r.node_class));
        let nodes = self
            .core
            .ask(|reply| AdminOp::ListNodes {
                node_id,
                node_class,
                reply,
            })
            .await?;
        Ok(Response::new(pb::ListNodesResponse { nodes }))
    }

    async fn drain_node(
        &self,
        request: Request<pb::DrainNodeRequest>,
    ) -> Result<Response<pb::DrainNodeResponse>, Status> {
        let node_id = request.into_inner().node_id;
        self.core
            .ask(|reply| AdminOp::SetDraining {
                node_id,
                draining: true,
                actor: CLI.into(),
                reply,
            })
            .await?;
        Ok(Response::new(pb::DrainNodeResponse {
            accepted: true,
            error: String::new(),
        }))
    }

    async fn undrain_node(
        &self,
        request: Request<pb::UndrainNodeRequest>,
    ) -> Result<Response<pb::UndrainNodeResponse>, Status> {
        let node_id = request.into_inner().node_id;
        self.core
            .ask(|reply| AdminOp::SetDraining {
                node_id,
                draining: false,
                actor: CLI.into(),
                reply,
            })
            .await?;
        Ok(Response::new(pb::UndrainNodeResponse {
            accepted: true,
            error: String::new(),
        }))
    }

    async fn remove_node(
        &self,
        request: Request<pb::RemoveNodeRequest>,
    ) -> Result<Response<pb::RemoveNodeResponse>, Status> {
        let node_id = request.into_inner().node_id;
        let workloads_stopped = self
            .core
            .ask(|reply| AdminOp::RemoveNode {
                node_id,
                actor: CLI.into(),
                reply,
            })
            .await?;
        Ok(Response::new(pb::RemoveNodeResponse {
            accepted: true,
            workloads_stopped,
        }))
    }

    async fn scale_service(
        &self,
        request: Request<pb::ScaleServiceRequest>,
    ) -> Result<Response<pb::ScaleServiceResponse>, Status> {
        let r = request.into_inner();
        let service_id = r.service_id.clone();
        let (old, new) = self
            .core
            .ask(|reply| AdminOp::Scale {
                service_id,
                replicas: r.replicas,
                actor: CLI.into(),
                reply,
            })
            .await?;
        Ok(Response::new(pb::ScaleServiceResponse {
            service_id: r.service_id,
            old_replicas: old,
            new_replicas: new,
            error: String::new(),
        }))
    }

    async fn list_events(
        &self,
        request: Request<pb::ListEventsRequest>,
    ) -> Result<Response<pb::ListEventsResponse>, Status> {
        let r = request.into_inner();
        let kind = opt(r.kind.to_uppercase());
        let limit = if r.limit == 0 {
            50
        } else {
            r.limit.min(1000) as u32
        };
        let since = r.since as i64;
        let reader = self.reader.clone();
        let rows = blocking(move || reader.events(kind.as_deref(), since, limit)).await?;
        let events = rows
            .into_iter()
            .map(|e| pb::ClusterEvent {
                event_id: e.event_id as u64,
                kind: e.kind,
                subject_type: e.subject_type,
                subject_id: e.subject_id,
                message: e.message,
                actor: e.actor,
                created_at: e.created_at as u64,
            })
            .collect();
        Ok(Response::new(pb::ListEventsResponse { events }))
    }
}
