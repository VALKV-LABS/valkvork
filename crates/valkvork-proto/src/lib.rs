//! gRPC protocol definitions for Valkvork (`valkvork.v1`, spec §11).

// tonic's API returns `Result<_, tonic::Status>`; Status is large by design.
#![allow(clippy::result_large_err)]

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/agent/valkvork.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/admin/valkvork.v1.rs"));

    /// Client for `AgentService` (hand-written, see build.rs).
    pub mod agent_service_client {
        use super::{AgentMessage, ControllerMessage};
        use tonic::codegen::*;

        #[derive(Debug, Clone)]
        pub struct AgentServiceClient<T> {
            inner: tonic::client::Grpc<T>,
        }

        impl<T> AgentServiceClient<T>
        where
            T: tonic::client::GrpcService<tonic::body::BoxBody>,
            T::Error: Into<StdError>,
            T::ResponseBody: Body<Data = Bytes> + Send + 'static,
            <T::ResponseBody as Body>::Error: Into<StdError> + Send,
        {
            pub fn new(inner: T) -> Self {
                Self {
                    inner: tonic::client::Grpc::new(inner),
                }
            }

            /// `rpc Connect(stream AgentMessage) returns (stream ControllerMessage)`
            pub async fn connect_stream(
                &mut self,
                request: impl tonic::IntoStreamingRequest<Message = AgentMessage>,
            ) -> Result<tonic::Response<tonic::codec::Streaming<ControllerMessage>>, tonic::Status>
            {
                self.inner.ready().await.map_err(|e| {
                    tonic::Status::unknown(format!("Service was not ready: {}", e.into()))
                })?;
                let codec = tonic::codec::ProstCodec::default();
                let path =
                    http::uri::PathAndQuery::from_static("/valkvork.v1.AgentService/Connect");
                self.inner
                    .streaming(request.into_streaming_request(), path, codec)
                    .await
            }
        }
    }
}

pub use v1::*;
