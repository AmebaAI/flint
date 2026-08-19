use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    catalog::{Protocol, ResolvedRuntime},
    session::SessionLease,
};
use async_stream::stream;
use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
};
use futures_util::StreamExt;
use thiserror::Error;
use tokio::time::Instant;

const RUNTIME_SESSION_ID_HEADER: &str = "x-amzn-bedrock-agentcore-runtime-session-id";
const RUNTIME_USER_ID_HEADER: &str = "x-amzn-bedrock-agentcore-runtime-user-id";
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const TRACE_HEADERS: [&str; 4] = ["x-amzn-trace-id", "traceparent", "tracestate", "baggage"];

#[derive(Clone)]
pub(crate) struct RuntimeProxy {
    client: reqwest::Client,
}

pub(crate) enum ProxyPayload {
    Buffered(Bytes),
    Streaming(Body),
}

pub(crate) struct ProxyRequest {
    pub(crate) deployment: Arc<ResolvedRuntime>,
    pub(crate) lease: SessionLease,
    pub(crate) headers: HeaderMap,
    pub(crate) payload: ProxyPayload,
    pub(crate) runtime_session_id: String,
}

impl RuntimeProxy {
    pub(crate) fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .expect("static Runtime proxy client configuration is valid"),
        }
    }

    pub(crate) async fn invoke(&self, request: ProxyRequest) -> Result<Response, ProxyError> {
        let path = request.deployment.protocol.invocation_path().to_owned();
        self.forward(reqwest::Method::POST, path, request).await
    }

    pub(crate) async fn agent_card(
        &self,
        mut request: ProxyRequest,
    ) -> Result<Response, ProxyError> {
        let path = request
            .deployment
            .protocol
            .agent_card_path()
            .map(str::to_owned)
            .ok_or(ProxyError::AgentCardNotConfigured)?;
        request.payload = ProxyPayload::Buffered(Bytes::new());
        self.forward(reqwest::Method::GET, path, request).await
    }

    async fn forward(
        &self,
        method: reqwest::Method,
        path: String,
        request: ProxyRequest,
    ) -> Result<Response, ProxyError> {
        let endpoint = request.lease.endpoint().trim_end_matches('/');
        let url = format!("{endpoint}{path}");
        let mcp_session_id = (request.deployment.protocol == Protocol::Mcp)
            .then(|| request.headers.get(MCP_SESSION_ID_HEADER).cloned())
            .flatten();
        let mut outbound = reqwest::header::HeaderMap::new();
        copy_request_header(
            &request.headers,
            &mut outbound,
            header::CONTENT_TYPE.as_str(),
        )?;
        copy_request_header(&request.headers, &mut outbound, header::ACCEPT.as_str())?;
        copy_request_header(&request.headers, &mut outbound, RUNTIME_USER_ID_HEADER)?;
        copy_request_header(&request.headers, &mut outbound, MCP_PROTOCOL_VERSION_HEADER)?;
        for name in TRACE_HEADERS {
            copy_request_header(&request.headers, &mut outbound, name)?;
        }
        for name in &request.deployment.allowed_custom_headers {
            copy_request_header(&request.headers, &mut outbound, name)?;
        }
        outbound.insert(
            HeaderName::from_static(RUNTIME_SESSION_ID_HEADER),
            HeaderValue::from_str(&request.runtime_session_id)
                .map_err(|_| ProxyError::InvalidHeader(RUNTIME_SESSION_ID_HEADER.to_owned()))?,
        );
        if let Some(value) = &mcp_session_id {
            outbound.insert(
                HeaderName::from_static(MCP_SESSION_ID_HEADER),
                value.clone(),
            );
        }

        let cancellation = request.lease.cancellation();
        let deadline =
            Instant::now() + Duration::from_secs(request.deployment.limits.max_duration_seconds);
        let request_limit_exceeded = Arc::new(AtomicBool::new(false));
        let payload = match request.payload {
            ProxyPayload::Buffered(payload) => reqwest::Body::from(payload),
            ProxyPayload::Streaming(payload) => {
                let mut chunks = payload.into_data_stream();
                let cancellation = cancellation.clone();
                let max_chunk_bytes = request.deployment.limits.max_chunk_bytes;
                let max_request_bytes = request.deployment.limits.max_request_bytes;
                let request_limit_exceeded = Arc::clone(&request_limit_exceeded);
                let stream = stream! {
                    let mut request_bytes = 0usize;
                    loop {
                        let next = tokio::select! {
                            biased;
                            () = cancellation.cancelled() => {
                                yield Err::<Bytes, io::Error>(io::Error::new(
                                    io::ErrorKind::Interrupted,
                                    "runtime session stopped",
                                ));
                                break;
                            }
                            next = chunks.next() => next,
                        };
                        let Some(chunk) = next else { break };
                        let chunk = match chunk {
                            Ok(chunk) => chunk,
                            Err(error) => {
                                yield Err::<Bytes, io::Error>(io::Error::other(error));
                                break;
                            }
                        };
                        request_bytes = request_bytes.saturating_add(chunk.len());
                        if chunk.len() > max_chunk_bytes || request_bytes > max_request_bytes {
                            request_limit_exceeded.store(true, Ordering::Release);
                            yield Err::<Bytes, io::Error>(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "runtime request exceeded configured limits",
                            ));
                            break;
                        }
                        yield Ok::<Bytes, io::Error>(chunk);
                    }
                };
                reqwest::Body::wrap_stream(stream)
            }
        };
        let send = self
            .client
            .request(method, url)
            .headers(outbound)
            .body(payload)
            .send();
        let response = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ProxyError::Cancelled),
            result = tokio::time::timeout_at(deadline, send) => {
                let result = result.map_err(|_| ProxyError::TimedOut)?;
                match result {
                    Ok(_response) if request_limit_exceeded.load(Ordering::Acquire) => {
                        return Err(ProxyError::RequestLimit);
                    }
                    Ok(response) => response,
                    Err(_) if request_limit_exceeded.load(Ordering::Acquire) => {
                        return Err(ProxyError::RequestLimit);
                    }
                    Err(error) => {
                        let message = error.to_string();
                        if let Err(invalidation) = request.lease.invalidate().await {
                            tracing::warn!(%invalidation, "failed to invalidate unavailable runtime compute");
                        }
                        return Err(ProxyError::Unavailable(message));
                    }
                }
            },
        };
        let status = response.status();
        if !status.is_success() {
            drain_error_body(
                response,
                request.deployment.limits.max_chunk_bytes,
                request.deployment.limits.max_response_bytes,
                &cancellation,
                deadline,
            )
            .await?;
            return Err(ProxyError::RuntimeClient(status.as_u16()));
        }

        let response_headers = response.headers().clone();
        let content_type = response_headers
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("application/octet-stream"));
        let response_mcp_session = response_headers
            .get(MCP_SESSION_ID_HEADER)
            .cloned()
            .or(mcp_session_id);
        let max_chunk_bytes = request.deployment.limits.max_chunk_bytes;
        let max_response_bytes = request.deployment.limits.max_response_bytes;
        let mut chunks = response.bytes_stream();
        let stream = stream! {
            let _lease = request.lease;
            let mut response_bytes = 0usize;
            loop {
                let next: Result<Option<Result<Bytes, reqwest::Error>>, io::Error> = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        Err(io::Error::new(io::ErrorKind::Interrupted, "runtime session stopped"))
                    },
                    next = tokio::time::timeout_at(deadline, chunks.next()) => {
                        next.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "runtime response timed out"))
                    }
                };
                let next = match next {
                    Ok(next) => next,
                    Err(error) => {
                        yield Err::<Bytes, io::Error>(error);
                        break;
                    }
                };
                let Some(chunk) = next else { break };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        yield Err::<Bytes, io::Error>(io::Error::other(error));
                        break;
                    }
                };
                if chunk.len() > max_chunk_bytes {
                    yield Err::<Bytes, io::Error>(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "runtime response chunk exceeded configured limit",
                    ));
                    break;
                }
                response_bytes = response_bytes.saturating_add(chunk.len());
                if response_bytes > max_response_bytes {
                    yield Err::<Bytes, io::Error>(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "runtime response exceeded configured limit",
                    ));
                    break;
                }
                yield Ok::<Bytes, io::Error>(chunk);
            }
        };
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .header(RUNTIME_SESSION_ID_HEADER, request.runtime_session_id);
        if let Some(value) = response_mcp_session {
            builder = builder.header(MCP_SESSION_ID_HEADER, value);
        }
        for name in TRACE_HEADERS {
            if let Some(value) = response_headers.get(name) {
                builder = builder.header(name, value);
            }
        }
        for name in &request.deployment.allowed_custom_headers {
            if let Some(value) = response_headers.get(name) {
                builder = builder.header(name, value);
            }
        }
        builder
            .body(Body::from_stream(stream))
            .map_err(|error| ProxyError::Response(error.to_string()))
    }
}

fn copy_request_header(
    source: &HeaderMap,
    target: &mut reqwest::header::HeaderMap,
    name: &str,
) -> Result<(), ProxyError> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| ProxyError::InvalidHeader(name.to_owned()))?;
    for value in source.get_all(&name) {
        target.append(name.clone(), value.clone());
    }
    Ok(())
}

async fn drain_error_body(
    response: reqwest::Response,
    max_chunk_bytes: usize,
    max_response_bytes: usize,
    cancellation: &tokio_util::sync::CancellationToken,
    deadline: Instant,
) -> Result<(), ProxyError> {
    let mut chunks = response.bytes_stream();
    let mut bytes = 0usize;
    loop {
        let next = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(ProxyError::Cancelled),
            next = tokio::time::timeout_at(deadline, chunks.next()) => {
                next.map_err(|_| ProxyError::TimedOut)?
            }
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|error| ProxyError::Unavailable(error.to_string()))?;
        bytes = bytes.saturating_add(chunk.len());
        if chunk.len() > max_chunk_bytes || bytes > max_response_bytes {
            return Err(ProxyError::ResponseLimit);
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum ProxyError {
    #[error("runtime agent card is not configured")]
    AgentCardNotConfigured,
    #[error("runtime request contains invalid header {0}")]
    InvalidHeader(String),
    #[error("runtime container is unavailable: {0}")]
    Unavailable(String),
    #[error("runtime container returned HTTP {0}")]
    RuntimeClient(u16),
    #[error("runtime request exceeded configured limits")]
    RequestLimit,
    #[error("runtime request timed out")]
    TimedOut,
    #[error("runtime session was stopped")]
    Cancelled,
    #[error("runtime response exceeded its configured limit")]
    ResponseLimit,
    #[error("failed to construct runtime response: {0}")]
    Response(String),
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        extract::{OriginalUri, Request, State},
        http::{HeaderMap, HeaderValue, StatusCode, header},
        response::Response,
        routing::{get, post},
    };
    use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};
    use tokio_util::sync::CancellationToken;

    use super::{MCP_SESSION_ID_HEADER, ProxyError, ProxyPayload, ProxyRequest, RuntimeProxy};
    use crate::{
        catalog::{Protocol, ResolvedRuntime, RuntimeCatalog},
        session::{SessionBackend, SessionContainer, SessionHealth, SessionKey, SessionManager},
    };

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        path: String,
        headers: HeaderMap,
        body: Bytes,
    }

    #[derive(Clone)]
    struct EndpointBackend {
        endpoint: String,
    }

    #[async_trait]
    impl SessionBackend for EndpointBackend {
        async fn start(
            &self,
            key: &SessionKey,
            _runtime: Arc<ResolvedRuntime>,
            _cancellation: CancellationToken,
        ) -> Result<SessionContainer, String> {
            Ok(SessionContainer {
                id: format!("fixture-{}", key.runtime_session_id),
                endpoint: self.endpoint.clone(),
                age: Duration::ZERO,
            })
        }

        async fn ping(&self, _container: &SessionContainer) -> SessionHealth {
            SessionHealth::Healthy
        }

        async fn stop(&self, _container: &SessionContainer) -> Result<(), String> {
            Ok(())
        }
    }

    async fn fixture_handler(
        State(recorded): State<Arc<Mutex<Vec<RecordedRequest>>>>,
        OriginalUri(uri): OriginalUri,
        request: Request,
    ) -> Response {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, 1024 * 1024).await.expect("fixture body");
        recorded.lock().await.push(RecordedRequest {
            path: uri.path().to_owned(),
            headers: parts.headers,
            body: body.clone(),
        });
        if body == Bytes::from_static(b"slow-error") {
            return slow_error().await;
        }
        if body == Bytes::from_static(b"fail") {
            return Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("container details must not cross the edge"))
                .expect("fixture error response");
        }
        let chunks = futures_util::stream::iter([
            Ok::<Bytes, Infallible>(Bytes::from_static(b"chunk-one")),
            Ok::<Bytes, Infallible>(Bytes::from_static(b"chunk-two")),
        ]);
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header("x-flint-invocation-id", "response-custom")
            .header("traceparent", "response-trace")
            .body(Body::from_stream(chunks))
            .expect("fixture response")
    }

    async fn slow_error() -> Response {
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from_stream(futures_util::stream::pending::<
                Result<Bytes, Infallible>,
            >()))
            .expect("slow error response")
    }

    async fn fixture() -> (String, Arc<Mutex<Vec<RecordedRequest>>>, JoinHandle<()>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/invocations", post(fixture_handler))
            .route("/mcp", post(fixture_handler))
            .route("/", post(fixture_handler))
            .route("/slow-error", post(slow_error))
            .route("/.well-known/agent-card.json", get(fixture_handler))
            .with_state(Arc::clone(&recorded));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("fixture address"));
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve proxy fixture");
        });
        (endpoint, recorded, server)
    }

    fn deployment(protocol: Protocol, _path: &str) -> Arc<ResolvedRuntime> {
        let snapshot = RuntimeCatalog::test_catalog().default_snapshot();
        let mut deployment = (*snapshot).clone();
        deployment.protocol = protocol;
        Arc::new(deployment)
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("must-not-forward"),
        );
        headers.insert("x-amz-date", HeaderValue::from_static("must-not-forward"));
        headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert("traceparent", HeaderValue::from_static("request-trace"));
        headers.insert(
            "x-flint-invocation-id",
            HeaderValue::from_static("request-custom"),
        );
        headers
    }

    #[tokio::test]
    async fn routes_all_protocols_without_mutating_payloads_or_forwarding_credentials() {
        let (endpoint, recorded, server) = fixture().await;
        let manager = SessionManager::new(Arc::new(EndpointBackend { endpoint }));
        let proxy = RuntimeProxy::new();
        let payload = Bytes::from_static(b"\x00opaque\xffpayload");
        let cases = [
            (Protocol::Http, "/invocations"),
            (Protocol::AgUi, "/invocations"),
            (Protocol::Mcp, "/mcp"),
            (Protocol::A2a, "/"),
        ];

        for (index, (protocol, path)) in cases.into_iter().enumerate() {
            let deployment = deployment(protocol, path);
            let runtime_session_id = format!("session-{index}");
            let lease = manager
                .acquire(Arc::clone(&deployment), runtime_session_id.clone())
                .await
                .expect("session lease");
            let response = proxy
                .invoke(ProxyRequest {
                    deployment,
                    lease,
                    headers: headers(),
                    payload: ProxyPayload::Buffered(payload.clone()),
                    runtime_session_id,
                })
                .await
                .expect("proxied invocation");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("text/event-stream"))
            );
            assert_eq!(
                to_bytes(response.into_body(), 1024)
                    .await
                    .expect("streamed response"),
                Bytes::from_static(b"chunk-onechunk-two")
            );
        }

        let recorded = recorded.lock().await;
        assert_eq!(recorded.len(), 4);
        for (request, (_, expected_path)) in recorded.iter().zip(cases) {
            assert_eq!(request.path, expected_path);
            assert_eq!(request.body, payload);
            assert!(request.headers.get(header::AUTHORIZATION).is_none());
            assert!(request.headers.get("x-amz-date").is_none());
            assert!(request.headers.get(header::CONNECTION).is_none());
            assert_eq!(
                request.headers.get("x-flint-invocation-id"),
                Some(&HeaderValue::from_static("request-custom"))
            );
        }
        let mcp_request = &recorded[2];
        assert!(mcp_request.headers.get(MCP_SESSION_ID_HEADER).is_none());
        server.abort();
    }

    #[tokio::test]
    async fn proxies_agent_card_and_maps_container_errors_without_leaking_their_body() {
        let (endpoint, recorded, server) = fixture().await;
        let manager = SessionManager::new(Arc::new(EndpointBackend { endpoint }));
        let proxy = RuntimeProxy::new();
        let deployment = deployment(Protocol::A2a, "/");
        let lease = manager
            .acquire(Arc::clone(&deployment), "card-session".to_owned())
            .await
            .expect("card lease");
        let card = proxy
            .agent_card(ProxyRequest {
                deployment: Arc::clone(&deployment),
                lease,
                headers: HeaderMap::new(),
                payload: ProxyPayload::Buffered(Bytes::new()),
                runtime_session_id: "card-session".to_owned(),
            })
            .await
            .expect("agent card");
        assert_eq!(
            to_bytes(card.into_body(), 1024).await.expect("card body"),
            Bytes::from_static(b"chunk-onechunk-two")
        );

        let lease = manager
            .acquire(Arc::clone(&deployment), "error-session".to_owned())
            .await
            .expect("error lease");
        let error = proxy
            .invoke(ProxyRequest {
                deployment: Arc::clone(&deployment),
                lease,
                headers: HeaderMap::new(),
                payload: ProxyPayload::Buffered(Bytes::from_static(b"fail")),
                runtime_session_id: "error-session".to_owned(),
            })
            .await
            .expect_err("container failure maps at the edge");
        assert!(matches!(error, ProxyError::RuntimeClient(503)));
        assert_eq!(
            manager.active_request_count(&deployment, "error-session"),
            Some(0)
        );
        assert_eq!(
            recorded.lock().await[0].path,
            "/.well-known/agent-card.json"
        );
        server.abort();
    }

    #[tokio::test]
    async fn streaming_request_limit_is_reported_as_a_request_error() {
        let (endpoint, _recorded, server) = fixture().await;
        let manager = SessionManager::new(Arc::new(EndpointBackend { endpoint }));
        let mut deployment = (*deployment(Protocol::Http, "/invocations")).clone();
        deployment.limits.max_request_bytes = 3;
        deployment.limits.max_chunk_bytes = 3;
        let deployment = Arc::new(deployment);
        let lease = manager
            .acquire(Arc::clone(&deployment), "limited-session".to_owned())
            .await
            .expect("limited request lease");
        let payload = Body::from_stream(futures_util::stream::iter([
            Ok::<Bytes, Infallible>(Bytes::from_static(b"ab")),
            Ok::<Bytes, Infallible>(Bytes::from_static(b"cd")),
        ]));
        let error = RuntimeProxy::new()
            .invoke(ProxyRequest {
                deployment,
                lease,
                headers: HeaderMap::new(),
                payload: ProxyPayload::Streaming(payload),
                runtime_session_id: "limited-session".to_owned(),
            })
            .await
            .expect_err("streaming request limit");
        assert!(matches!(error, ProxyError::RequestLimit));
        server.abort();
    }

    #[tokio::test]
    async fn stopping_a_session_cancels_a_slow_container_error_body() {
        let (endpoint, _recorded, server) = fixture().await;
        let manager = SessionManager::new(Arc::new(EndpointBackend { endpoint }));
        let proxy = RuntimeProxy::new();
        let deployment = deployment(Protocol::Http, "/slow-error");
        let lease = manager
            .acquire(Arc::clone(&deployment), "slow-error-session".to_owned())
            .await
            .expect("slow error lease");
        let request_deployment = Arc::clone(&deployment);
        let invocation = tokio::spawn(async move {
            proxy
                .invoke(ProxyRequest {
                    deployment: request_deployment,
                    lease,
                    headers: HeaderMap::new(),
                    payload: ProxyPayload::Buffered(Bytes::from_static(b"slow-error")),
                    runtime_session_id: "slow-error-session".to_owned(),
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        manager
            .stop(
                &deployment,
                "slow-error-session".to_owned(),
                Some("slow-error-stop-token".to_owned()),
            )
            .await
            .expect("stop cancels slow error response");
        assert!(matches!(
            invocation.await.expect("proxy task"),
            Err(ProxyError::Cancelled)
        ));
        assert!(
            manager
                .active_request_count(&deployment, "slow-error-session")
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn dropping_streaming_response_releases_the_session_lease() {
        let (endpoint, _recorded, server) = fixture().await;
        let manager = SessionManager::new(Arc::new(EndpointBackend { endpoint }));
        let proxy = RuntimeProxy::new();
        let deployment = deployment(Protocol::Http, "/invocations");
        let lease = manager
            .acquire(Arc::clone(&deployment), "drop-session".to_owned())
            .await
            .expect("stream lease");
        let response = proxy
            .invoke(ProxyRequest {
                deployment: Arc::clone(&deployment),
                lease,
                headers: HeaderMap::new(),
                payload: ProxyPayload::Buffered(Bytes::new()),
                runtime_session_id: "drop-session".to_owned(),
            })
            .await
            .expect("stream response");
        assert_eq!(
            manager.active_request_count(&deployment, "drop-session"),
            Some(1)
        );
        drop(response);
        assert_eq!(
            manager.active_request_count(&deployment, "drop-session"),
            Some(0)
        );
        server.abort();
    }
}
