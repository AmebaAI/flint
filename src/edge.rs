use std::sync::Arc;

#[cfg(test)]
use std::collections::HashMap;

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{OriginalUri, Path, Query, Request, State, rejection::QueryRejection},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
#[cfg(test)]
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    auth::{AuthorizationError, AuthorizationRequest, RuntimeIamAction, authorize, local_identity},
    catalog::{
        AuthenticationMode, CatalogError, LocalIdentity, ResolvedRuntime, RuntimeRegistry,
        RuntimeRegistryHealth,
    },
    command::event_stream_body,
    proxy::{ProxyError, ProxyPayload, ProxyRequest, RuntimeProxy},
    session::{SessionError, SessionManager},
};
#[cfg(test)]
use crate::{
    runtime::{InvocationError, InvocationRequest, InvocationRuntime},
    session::InvocationSessionBackend,
};

pub(crate) const RUNTIME_SESSION_ID_HEADER: &str = "x-amzn-bedrock-agentcore-runtime-session-id";
const RUNTIME_USER_ID_HEADER: &str = "x-amzn-bedrock-agentcore-runtime-user-id";
const REQUEST_ID_HEADER: &str = "x-amzn-requestid";
const ERROR_TYPE_HEADER: &str = "x-amzn-errortype";
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const TRACE_ID_HEADER: &str = "x-amzn-trace-id";
const TRACE_PARENT_HEADER: &str = "traceparent";
const TRACE_STATE_HEADER: &str = "tracestate";
const BAGGAGE_HEADER: &str = "baggage";

#[derive(Clone)]
pub(crate) struct RuntimeApiState {
    registry: RuntimeRegistry,
    sessions: SessionManager,
    proxy: Option<RuntimeProxy>,
    #[cfg(test)]
    runtime: Option<InvocationRuntime>,
    #[cfg(test)]
    active_sessions: Arc<Mutex<HashMap<String, Uuid>>>,
}

impl RuntimeApiState {
    #[cfg(test)]
    pub(crate) fn new(registry: RuntimeRegistry, runtime: InvocationRuntime) -> Self {
        let sessions =
            SessionManager::new(Arc::new(InvocationSessionBackend::new(runtime.clone())));
        Self {
            registry,
            sessions,
            proxy: None,
            runtime: Some(runtime),
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn with_sessions(registry: RuntimeRegistry, sessions: SessionManager) -> Self {
        Self {
            registry,
            sessions,
            proxy: Some(RuntimeProxy::new()),
            #[cfg(test)]
            runtime: None,
            #[cfg(test)]
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(crate) fn runtime(&self) -> Option<&InvocationRuntime> {
        self.runtime.as_ref()
    }

    pub(crate) fn registry_health(&self) -> RuntimeRegistryHealth {
        self.registry.health()
    }

    fn proxy(&self) -> Option<&RuntimeProxy> {
        self.proxy.as_ref()
    }
}

pub(crate) fn routes() -> Router<RuntimeApiState> {
    Router::new()
        .route(
            "/runtimes/{agent_runtime_arn}/invocations",
            post(invoke_agent_runtime),
        )
        .route(
            "/runtimes/{agent_runtime_arn}/commands",
            post(invoke_agent_runtime_command),
        )
        .route(
            "/runtimes/{agent_runtime_arn}/stopruntimesession",
            post(stop_runtime_session),
        )
        .route(
            "/runtimes/{agent_runtime_arn}/invocations/.well-known/agent-card.json",
            get(get_agent_card),
        )
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeQuery {
    qualifier: Option<String>,
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommandBody {
    command: String,
    timeout: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopRuntimeSessionBody {
    client_token: Option<String>,
}

async fn invoke_agent_runtime(
    State(state): State<RuntimeApiState>,
    Path(runtime_identifier): Path<String>,
    OriginalUri(original_uri): OriginalUri,
    query: Result<Query<RuntimeQuery>, QueryRejection>,
    request: Request,
) -> Result<Response, RuntimeApiError> {
    let request_id = request_id();
    let query = query
        .map_err(|error| RuntimeApiError::validation(request_id.clone(), error.to_string()))?
        .0;
    let (parts, body) = request.into_parts();
    validate_runtime_headers(&parts.headers, &request_id)?;
    let runtime_session_id = runtime_session_id(&parts.headers, true, &request_id)?
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let deployment = resolve_session_runtime(
        &state,
        &runtime_identifier,
        query.account_id.as_deref(),
        query.qualifier.as_deref(),
        &runtime_session_id,
        &parts.headers,
        &request_id,
    )?;
    if let Some(proxy) = state.proxy()
        && deployment.authentication.mode == AuthenticationMode::Permissive
        && !parts.headers.contains_key(header::AUTHORIZATION)
    {
        authorize_request(
            &deployment,
            &parts.method,
            &original_uri,
            &parts.headers,
            &[],
            RuntimeIamAction::InvokeAgentRuntime,
            &request_id,
        )?;
        let lease = state
            .sessions
            .acquire(Arc::clone(&deployment), runtime_session_id.clone())
            .await
            .map_err(|error| map_session_error(error, request_id.clone()))?;
        let mut response = proxy
            .invoke(ProxyRequest {
                deployment,
                lease,
                headers: parts.headers,
                payload: ProxyPayload::Streaming(body),
                runtime_session_id,
            })
            .await
            .map_err(|error| map_proxy_error(error, request_id.clone()))?;
        response.headers_mut().insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            HeaderValue::from_str(&request_id).expect("request ID is a valid header"),
        );
        return Ok(response);
    }
    let payload = to_bytes(body, deployment.limits.max_request_bytes)
        .await
        .map_err(|_| {
            RuntimeApiError::validation(
                request_id.clone(),
                format!(
                    "payload exceeds {} bytes",
                    deployment.limits.max_request_bytes
                ),
            )
        })?;
    authorize_request(
        &deployment,
        &parts.method,
        &original_uri,
        &parts.headers,
        &payload,
        RuntimeIamAction::InvokeAgentRuntime,
        &request_id,
    )?;
    let session_lease = state
        .sessions
        .acquire(Arc::clone(&deployment), runtime_session_id.clone())
        .await
        .map_err(|error| map_session_error(error, request_id.clone()))?;
    if let Some(proxy) = state.proxy() {
        let mut response = proxy
            .invoke(ProxyRequest {
                deployment,
                lease: session_lease,
                headers: parts.headers,
                payload: ProxyPayload::Buffered(payload),
                runtime_session_id,
            })
            .await
            .map_err(|error| map_proxy_error(error, request_id.clone()))?;
        response.headers_mut().insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            HeaderValue::from_str(&request_id).expect("request ID is a valid header"),
        );
        return Ok(response);
    }
    #[cfg(not(test))]
    {
        drop(session_lease);
        Err(RuntimeApiError::internal(
            request_id,
            "runtime proxy is unavailable",
        ))
    }
    #[cfg(test)]
    {
        let _session_target = (session_lease.container_id(), session_lease.endpoint());
        let session_cancellation = session_lease.cancellation();
        let invocation_id = Uuid::new_v4();
        let identity_id = Uuid::parse_str(&runtime_session_id).unwrap_or(invocation_id);
        let content_type = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let legacy_input = json!({
            "contentType": content_type,
            "payload": payload.as_ref(),
        });
        let invocation = InvocationRequest {
            invocation_id,
            workspace_id: Uuid::nil(),
            fencing_token: 1,
            backend_credential: "edge-opaque-payload".to_owned(),
            input: legacy_input,
        };

        {
            let mut active = state.active_sessions.lock().await;
            if active.contains_key(&runtime_session_id) {
                return Err(RuntimeApiError::retryable_conflict(
                    request_id,
                    "another operation is active for this runtime session",
                    "identity_already_active",
                ));
            }
            active.insert(runtime_session_id.clone(), invocation_id);
        }
        let runtime = state
            .runtime()
            .expect("legacy invocation requires the test runtime");
        let invocation = runtime.invoke(identity_id, invocation);
        tokio::pin!(invocation);
        let result = tokio::select! {
            biased;
            () = session_cancellation.cancelled() => {
                runtime.cancel(invocation_id).await;
                (&mut invocation).await
            }
            result = &mut invocation => result,
        };
        let mut active = state.active_sessions.lock().await;
        if active.get(&runtime_session_id) == Some(&invocation_id) {
            active.remove(&runtime_session_id);
        }
        drop(active);
        let output = result.map_err(|error| map_invocation_error(error, request_id.clone()))?;
        let body = serde_json::to_vec(&output).map_err(|error| {
            RuntimeApiError::internal(request_id.clone(), format!("serialize response: {error}"))
        })?;

        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(REQUEST_ID_HEADER, &request_id)
            .header(RUNTIME_SESSION_ID_HEADER, &runtime_session_id);
        response = copy_response_header(response, &parts.headers, MCP_SESSION_ID_HEADER);
        response = copy_response_header(response, &parts.headers, MCP_PROTOCOL_VERSION_HEADER);
        response = copy_response_header(response, &parts.headers, TRACE_ID_HEADER);
        response = copy_response_header(response, &parts.headers, TRACE_PARENT_HEADER);
        response = copy_response_header(response, &parts.headers, TRACE_STATE_HEADER);
        response = copy_response_header(response, &parts.headers, BAGGAGE_HEADER);
        response.body(Body::from(body)).map_err(|error| {
            RuntimeApiError::internal(request_id, format!("build response: {error}"))
        })
    }
}

async fn invoke_agent_runtime_command(
    State(state): State<RuntimeApiState>,
    Path(runtime_identifier): Path<String>,
    OriginalUri(original_uri): OriginalUri,
    query: Result<Query<RuntimeQuery>, QueryRejection>,
    request: Request,
) -> Result<Response, RuntimeApiError> {
    let request_id = request_id();
    let query = query
        .map_err(|error| RuntimeApiError::validation(request_id.clone(), error.to_string()))?
        .0;
    let (parts, body) = request.into_parts();
    let runtime_session_id = runtime_session_id(&parts.headers, false, &request_id)?
        .expect("required runtime session ID was validated");
    let deployment = resolve_session_runtime(
        &state,
        &runtime_identifier,
        query.account_id.as_deref(),
        query.qualifier.as_deref(),
        &runtime_session_id,
        &parts.headers,
        &request_id,
    )?;
    let payload = to_bytes(body, deployment.limits.max_request_bytes)
        .await
        .map_err(|_| {
            RuntimeApiError::validation(
                request_id.clone(),
                format!(
                    "command payload exceeds {} bytes",
                    deployment.limits.max_request_bytes
                ),
            )
        })?;
    authorize_request(
        &deployment,
        &parts.method,
        &original_uri,
        &parts.headers,
        &payload,
        RuntimeIamAction::InvokeAgentRuntimeCommand,
        &request_id,
    )?;
    if !deployment.command.enabled {
        return Err(RuntimeApiError::validation(
            request_id,
            "commands are disabled for this runtime",
        ));
    }
    let body: CommandBody = serde_json::from_slice(&payload).map_err(|error| {
        RuntimeApiError::validation(
            request_id.clone(),
            format!("invalid command request body: {error}"),
        )
    })?;
    if body.command.is_empty() {
        return Err(RuntimeApiError::validation(
            request_id,
            "command must not be empty",
        ));
    }
    if body
        .timeout
        .is_some_and(|timeout| !(1..=3600).contains(&timeout))
    {
        return Err(RuntimeApiError::validation(
            request_id,
            "command timeout must be between 1 and 3600 seconds",
        ));
    }
    let execution = state
        .sessions
        .execute_command(
            Arc::clone(&deployment),
            runtime_session_id.clone(),
            body.command,
            body.timeout,
        )
        .await
        .map_err(|error| map_session_error(error, request_id.clone()))?;
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.amazon.eventstream")
        .header(RUNTIME_SESSION_ID_HEADER, runtime_session_id)
        .header(REQUEST_ID_HEADER, request_id);
    for name in [
        TRACE_ID_HEADER,
        TRACE_PARENT_HEADER,
        TRACE_STATE_HEADER,
        BAGGAGE_HEADER,
    ] {
        response = copy_response_header(response, &parts.headers, name);
    }
    Ok(response
        .body(event_stream_body(execution))
        .expect("command event stream response is valid"))
}

async fn stop_runtime_session(
    State(state): State<RuntimeApiState>,
    Path(runtime_identifier): Path<String>,
    OriginalUri(original_uri): OriginalUri,
    query: Result<Query<RuntimeQuery>, QueryRejection>,
    request: Request,
) -> Result<Response, RuntimeApiError> {
    let request_id = request_id();
    let query = query
        .map_err(|error| RuntimeApiError::validation(request_id.clone(), error.to_string()))?
        .0;
    let (parts, body) = request.into_parts();
    let runtime_session_id = runtime_session_id(&parts.headers, false, &request_id)?
        .expect("required runtime session ID was validated");
    let deployment = resolve_session_runtime(
        &state,
        &runtime_identifier,
        query.account_id.as_deref(),
        query.qualifier.as_deref(),
        &runtime_session_id,
        &parts.headers,
        &request_id,
    )?;
    let payload = to_bytes(body, deployment.limits.max_request_bytes)
        .await
        .map_err(|_| {
            RuntimeApiError::validation(
                request_id.clone(),
                format!(
                    "stop payload exceeds {} bytes",
                    deployment.limits.max_request_bytes
                ),
            )
        })?;
    authorize_request(
        &deployment,
        &parts.method,
        &original_uri,
        &parts.headers,
        &payload,
        RuntimeIamAction::StopRuntimeSession,
        &request_id,
    )?;
    let stop = if payload.is_empty() {
        StopRuntimeSessionBody::default()
    } else {
        serde_json::from_slice(&payload).map_err(|error| {
            RuntimeApiError::validation(
                request_id.clone(),
                format!("stop request body is invalid: {error}"),
            )
        })?
    };
    #[cfg(test)]
    if let Some(invocation_id) = state
        .active_sessions
        .lock()
        .await
        .remove(&runtime_session_id)
        && let Some(runtime) = state.runtime()
    {
        runtime.cancel(invocation_id).await;
    }
    state
        .sessions
        .stop(&deployment, runtime_session_id.clone(), stop.client_token)
        .await
        .map_err(|error| map_session_error(error, request_id.clone()))?;
    Response::builder()
        .status(StatusCode::OK)
        .header(REQUEST_ID_HEADER, &request_id)
        .header(RUNTIME_SESSION_ID_HEADER, runtime_session_id)
        .body(Body::empty())
        .map_err(|error| RuntimeApiError::internal(request_id, error.to_string()))
}

async fn get_agent_card(
    State(state): State<RuntimeApiState>,
    Path(runtime_identifier): Path<String>,
    OriginalUri(original_uri): OriginalUri,
    query: Result<Query<RuntimeQuery>, QueryRejection>,
    request: Request,
) -> Result<Response, RuntimeApiError> {
    let request_id = request_id();
    let query = query
        .map_err(|error| RuntimeApiError::validation(request_id.clone(), error.to_string()))?
        .0;
    let (parts, _) = request.into_parts();
    let runtime_session_id = runtime_session_id(&parts.headers, false, &request_id)?
        .expect("required runtime session ID was validated");
    let deployment = resolve_session_runtime(
        &state,
        &runtime_identifier,
        query.account_id.as_deref(),
        query.qualifier.as_deref(),
        &runtime_session_id,
        &parts.headers,
        &request_id,
    )?;
    authorize_request(
        &deployment,
        &parts.method,
        &original_uri,
        &parts.headers,
        &[],
        RuntimeIamAction::GetAgentCard,
        &request_id,
    )?;
    if deployment.protocol.agent_card_path().is_none() {
        return Err(RuntimeApiError::runtime_client(
            request_id,
            "this runtime does not expose an A2A agent card",
            "agent_card_not_configured",
        ));
    }
    if let Some(proxy) = state.proxy() {
        let lease = state
            .sessions
            .acquire(Arc::clone(&deployment), runtime_session_id.clone())
            .await
            .map_err(|error| map_session_error(error, request_id.clone()))?;
        let mut response = proxy
            .agent_card(ProxyRequest {
                deployment,
                lease,
                headers: parts.headers,
                payload: ProxyPayload::Buffered(Default::default()),
                runtime_session_id,
            })
            .await
            .map_err(|error| map_proxy_error(error, request_id.clone()))?;
        response.headers_mut().insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            HeaderValue::from_str(&request_id).expect("request ID is a valid header"),
        );
        return Ok(response);
    }
    Err(RuntimeApiError::runtime_client(
        request_id,
        "agent-card proxying is deferred until the A2A protocol implementation",
        "agent_card_not_implemented",
    ))
}

fn authorize_request(
    deployment: &ResolvedRuntime,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
    action: RuntimeIamAction,
    request_id: &str,
) -> Result<(), RuntimeApiError> {
    let runtime_user_id = headers
        .get(RUNTIME_USER_ID_HEADER)
        .map(|value| {
            value.to_str().map_err(|_| {
                RuntimeApiError::validation(
                    request_id.to_owned(),
                    "runtime user ID is not valid header text",
                )
            })
        })
        .transpose()?;
    authorize(
        deployment,
        AuthorizationRequest {
            method,
            uri,
            headers,
            body,
            action,
            runtime_user_id,
        },
    )
    .map(|_| ())
    .map_err(|error| match error {
        AuthorizationError::AccessDenied(message) => {
            RuntimeApiError::access_denied(request_id.to_owned(), message)
        }
    })
}

fn resolve_session_runtime(
    state: &RuntimeApiState,
    runtime_identifier: &str,
    account_id: Option<&str>,
    qualifier: Option<&str>,
    runtime_session_id: &str,
    headers: &HeaderMap,
    request_id: &str,
) -> Result<Arc<ResolvedRuntime>, RuntimeApiError> {
    let identity = local_identity(headers).map_err(|error| match error {
        AuthorizationError::AccessDenied(message) => {
            RuntimeApiError::access_denied(request_id.to_owned(), message)
        }
    })?;
    if let Some(runtime) = state.sessions.pinned_runtime(
        runtime_identifier,
        account_id,
        qualifier,
        runtime_session_id,
        &identity,
    ) {
        return Ok(runtime);
    }
    resolve_runtime(
        state,
        runtime_identifier,
        account_id,
        qualifier,
        &identity,
        request_id,
    )
}

fn resolve_runtime(
    state: &RuntimeApiState,
    runtime_identifier: &str,
    account_id: Option<&str>,
    qualifier: Option<&str>,
    identity: &LocalIdentity,
    request_id: &str,
) -> Result<Arc<ResolvedRuntime>, RuntimeApiError> {
    state
        .registry
        .resolve(runtime_identifier, account_id, qualifier, identity)
        .map_err(|error| match error {
            CatalogError::Resolution(message) if message.contains("accountId is required") => {
                RuntimeApiError::validation(request_id.to_owned(), message)
            }
            CatalogError::IdentityMismatch(message) => {
                RuntimeApiError::access_denied(request_id.to_owned(), message)
            }
            CatalogError::Resolution(message) => {
                RuntimeApiError::resource_not_found(request_id.to_owned(), message)
            }
            other => RuntimeApiError::internal(request_id.to_owned(), other.to_string()),
        })
}

fn validate_runtime_headers(headers: &HeaderMap, request_id: &str) -> Result<(), RuntimeApiError> {
    for name in [
        RUNTIME_USER_ID_HEADER,
        MCP_SESSION_ID_HEADER,
        MCP_PROTOCOL_VERSION_HEADER,
        "mcp-method",
        "mcp-name",
        TRACE_ID_HEADER,
        TRACE_PARENT_HEADER,
        TRACE_STATE_HEADER,
        BAGGAGE_HEADER,
    ] {
        if headers
            .get(name)
            .is_some_and(|value| value.as_bytes().is_empty())
        {
            return Err(RuntimeApiError::validation(
                request_id.to_owned(),
                format!("header {name} cannot be empty"),
            ));
        }
    }
    Ok(())
}

fn runtime_session_id(
    headers: &HeaderMap,
    optional: bool,
    request_id: &str,
) -> Result<Option<String>, RuntimeApiError> {
    let value = headers
        .get(RUNTIME_SESSION_ID_HEADER)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                RuntimeApiError::validation(
                    request_id.to_owned(),
                    "runtime session ID is not valid header text",
                )
            })
        })
        .transpose()?;
    let Some(value) = value else {
        return if optional {
            Ok(None)
        } else {
            Err(RuntimeApiError::validation(
                request_id.to_owned(),
                "runtime session ID is required",
            ))
        };
    };
    if !(33..=256).contains(&value.len())
        || !value.bytes().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, b'.' | b':' | b'_' | b'-')
        })
    {
        return Err(RuntimeApiError::validation(
            request_id.to_owned(),
            "runtime session ID must be 33-256 ASCII letters, digits, periods, colons, underscores, or hyphens",
        ));
    }
    Ok(Some(value))
}

fn copy_response_header(
    mut response: axum::http::response::Builder,
    request_headers: &HeaderMap,
    name: &'static str,
) -> axum::http::response::Builder {
    if let Some(value) = request_headers.get(name) {
        response = response.header(name, value);
    }
    response
}

fn map_proxy_error(error: ProxyError, request_id: String) -> RuntimeApiError {
    match error {
        ProxyError::AgentCardNotConfigured => RuntimeApiError::runtime_client(
            request_id,
            error.to_string(),
            "agent_card_not_configured",
        ),
        ProxyError::InvalidHeader(_) => RuntimeApiError::validation_with_code(
            request_id,
            error.to_string(),
            "invalid_forwarded_header",
        ),
        ProxyError::Unavailable(_) => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "runtime_unavailable")
        }
        ProxyError::RuntimeClient(_) => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "runtime_client_error")
        }
        ProxyError::RequestLimit => RuntimeApiError::validation_with_code(
            request_id,
            error.to_string(),
            "request_body_too_large",
        ),
        ProxyError::TimedOut => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "runtime_timed_out")
        }
        ProxyError::Cancelled => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "runtime_cancelled")
        }
        ProxyError::ResponseLimit => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "runtime_response_limit")
        }
        ProxyError::Response(_) => RuntimeApiError::internal(request_id, error.to_string()),
    }
}

fn map_session_error(error: SessionError, request_id: String) -> RuntimeApiError {
    match error {
        SessionError::RetryableConflict(message) => {
            RuntimeApiError::retryable_conflict(request_id, message, "session_state_conflict")
        }
        SessionError::Provisioning(message) => {
            RuntimeApiError::runtime_client(request_id, message, "session_provisioning_failed")
        }
        SessionError::Command(message) => {
            RuntimeApiError::runtime_client(request_id, message, "runtime_command_failed")
        }
        SessionError::StoppedDuringProvisioning => RuntimeApiError::retryable_conflict(
            request_id,
            error.to_string(),
            "session_stopped_during_provisioning",
        ),
        SessionError::Stopping(message) => {
            RuntimeApiError::runtime_client(request_id, message, "session_stop_failed")
        }
    }
}

#[cfg(test)]
fn map_invocation_error(error: InvocationError, request_id: String) -> RuntimeApiError {
    match error {
        InvocationError::IdentityAlreadyActive => RuntimeApiError::retryable_conflict(
            request_id,
            error.to_string(),
            "identity_already_active",
        ),
        InvocationError::InvalidRequest => {
            RuntimeApiError::validation_with_code(request_id, error.to_string(), "invalid_request")
        }
        InvocationError::InvocationNotActive => RuntimeApiError::resource_not_found_with_code(
            request_id,
            error.to_string(),
            "invocation_not_active",
        ),
        InvocationError::RuntimeFailed => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "attempts_exhausted")
        }
        InvocationError::AgentRejected { ref code, .. } => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), code.clone())
        }
        InvocationError::InvalidOutcome => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "invalid_outcome")
        }
        InvocationError::TimedOut => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "invocation_timed_out")
        }
        InvocationError::Cancelled => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "invocation_cancelled")
        }
        InvocationError::CostLimitExceeded => {
            RuntimeApiError::runtime_client(request_id, error.to_string(), "cost_limit_exceeded")
        }
        InvocationError::CleanupFailed => RuntimeApiError::runtime_client(
            request_id,
            error.to_string(),
            "container_cleanup_failed",
        ),
    }
}

fn request_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Debug)]
pub(crate) struct RuntimeApiError {
    status: StatusCode,
    error_type: &'static str,
    message: String,
    request_id: String,
    code: String,
}

impl RuntimeApiError {
    fn access_denied(request_id: String, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            error_type: "AccessDeniedException",
            message: message.into(),
            request_id,
            code: "access_denied".to_owned(),
        }
    }

    fn validation(request_id: String, message: impl Into<String>) -> Self {
        Self::validation_with_code(request_id, message, "validation_error")
    }

    fn validation_with_code(
        request_id: String,
        message: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error_type: "ValidationException",
            message: message.into(),
            request_id,
            code: code.into(),
        }
    }

    fn resource_not_found(request_id: String, message: impl Into<String>) -> Self {
        Self::resource_not_found_with_code(request_id, message, "resource_not_found")
    }

    fn resource_not_found_with_code(
        request_id: String,
        message: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error_type: "ResourceNotFoundException",
            message: message.into(),
            request_id,
            code: code.into(),
        }
    }

    fn retryable_conflict(
        request_id: String,
        message: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error_type: "RetryableConflictException",
            message: message.into(),
            request_id,
            code: code.into(),
        }
    }

    fn runtime_client(
        request_id: String,
        message: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            status: StatusCode::from_u16(424).expect("424 is a valid HTTP status"),
            error_type: "RuntimeClientError",
            message: message.into(),
            request_id,
            code: code.into(),
        }
    }

    fn internal(request_id: String, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error_type: "InternalServerException",
            message: message.into(),
            request_id,
            code: "internal_error".to_owned(),
        }
    }
}

impl IntoResponse for RuntimeApiError {
    fn into_response(self) -> Response {
        let body = Body::from(
            serde_json::to_vec(&json!({
                "message": &self.message,
                "detail": &self.message,
                "code": self.code,
            }))
            .expect("AWS error JSON is serializable"),
        );
        Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json")
            .header(ERROR_TYPE_HEADER, self.error_type)
            .header(REQUEST_ID_HEADER, self.request_id)
            .body(body)
            .expect("AWS error response is valid")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::Request,
        http::{StatusCode, header},
        routing::post,
    };

    use aws_sdk_bedrockagentcore::{
        Client,
        config::{BehaviorVersion, Credentials, Region},
        primitives::Blob,
        types::{InvokeAgentRuntimeCommandRequestBody, InvokeAgentRuntimeCommandStreamOutput},
    };
    use tokio::{net::TcpListener, task::JoinHandle};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use super::RUNTIME_SESSION_ID_HEADER;
    use crate::{
        InvocationRuntime, ScaffoldContainerRuntime,
        catalog::{
            AuthenticationMode, AuthorizationPolicy, PolicyEffect, PolicyStatement, RuntimeCatalog,
        },
        runtime_router, runtime_router_with_sessions,
        session::{SessionBackend, SessionContainer, SessionHealth, SessionKey, SessionManager},
    };

    const RUNTIME_ARN: &str =
        "arn:aws:bedrock-agentcore:us-west-2:000000000000:runtime/flint_local";
    const SESSION_ID: &str = "20000000-0000-0000-0000-000000000001";
    static SDK_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn sdk_against_edge() -> (Client, JoinHandle<()>) {
        sdk_against_catalog(RuntimeCatalog::test_catalog(), "local-secret-key").await
    }

    async fn sdk_against_catalog(
        catalog: RuntimeCatalog,
        secret_access_key: &str,
    ) -> (Client, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind Runtime API fixture");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("Runtime API address")
        );
        let app = runtime_router(
            catalog,
            InvocationRuntime::new(Arc::new(ScaffoldContainerRuntime)),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve Runtime API fixture");
        });
        let config = aws_sdk_bedrockagentcore::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-west-2"))
            .credentials_provider(Credentials::new(
                "local-access-key",
                secret_access_key,
                None,
                None,
                "runtime-edge-test",
            ))
            .endpoint_url(endpoint)
            .build();
        (Client::from_conf(config), task)
    }

    async fn run_command(client: &Client, command: &str) -> (bool, String, Option<(i32, String)>) {
        let mut output = client
            .invoke_agent_runtime_command()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id(SESSION_ID)
            .body(
                InvokeAgentRuntimeCommandRequestBody::builder()
                    .command(command)
                    .build()
                    .expect("command body"),
            )
            .send()
            .await
            .expect("official SDK starts a runtime command");
        assert_eq!(output.runtime_session_id(), Some(SESSION_ID));
        let mut saw_start = false;
        let mut stdout = String::new();
        let mut completed = None;
        while let Some(event) = output.stream.recv().await.expect("decode command event") {
            if let InvokeAgentRuntimeCommandStreamOutput::Chunk(chunk) = event {
                saw_start |= chunk.content_start().is_some();
                if let Some(delta) = chunk.content_delta()
                    && let Some(value) = delta.stdout()
                {
                    stdout.push_str(value);
                }
                if let Some(stop) = chunk.content_stop() {
                    completed = Some((stop.exit_code(), stop.status().as_str().to_owned()));
                }
            }
        }
        (saw_start, stdout, completed)
    }

    #[tokio::test]
    async fn unmodified_sdk_invokes_aws_runtime_edge_by_arn_and_id() {
        let _guard = SDK_TEST_LOCK.lock().await;
        let (client, server) = sdk_against_edge().await;
        for invocation in [
            client.invoke_agent_runtime().agent_runtime_arn(RUNTIME_ARN),
            client
                .invoke_agent_runtime()
                .agent_runtime_arn("flint_local")
                .account_id("000000000000"),
        ] {
            let output = invocation
                .runtime_session_id(SESSION_ID)
                .qualifier("DEFAULT")
                .content_type("application/octet-stream")
                .accept("application/json")
                .payload(Blob::new(b"\x00opaque\xffpayload"))
                .send()
                .await
                .expect("official SDK invokes emulator edge");
            assert_eq!(output.runtime_session_id(), Some(SESSION_ID));
            assert_eq!(output.status_code(), Some(200));
            assert_eq!(output.content_type(), "application/json");
            assert_eq!(
                output
                    .response
                    .collect()
                    .await
                    .expect("collect runtime response")
                    .into_bytes()
                    .as_ref(),
                br#"{"result":{"kind":"fixture"},"status":"completed"}"#,
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn unmodified_sdk_reaches_stop_command_card_and_modeled_errors() {
        let _guard = SDK_TEST_LOCK.lock().await;
        let (client, server) = sdk_against_edge().await;

        let stopped = client
            .stop_runtime_session()
            .agent_runtime_arn("flint_local")
            .runtime_session_id(SESSION_ID)
            .client_token("10000000-0000-0000-0000-000000000001")
            .customize()
            .mutate_request(|request| {
                let uri = format!("{}?accountId=000000000000", request.uri());
                request.set_uri(uri).expect("valid account ID query URI");
            })
            .send()
            .await
            .expect("official SDK stops an idempotent session");
        assert_eq!(stopped.runtime_session_id(), Some(SESSION_ID));
        assert_eq!(stopped.status_code(), Some(200));

        let (saw_start, stdout, completed) = run_command(&client, "pwd").await;
        assert!(saw_start);
        assert_eq!(stdout, "pwd");
        assert_eq!(completed, Some((0, "COMPLETED".to_owned())));

        let card_error = client
            .get_agent_card()
            .agent_runtime_arn("flint_local")
            .runtime_session_id(SESSION_ID)
            .customize()
            .mutate_request(|request| {
                let uri = format!("{}?accountId=000000000000", request.uri());
                request.set_uri(uri).expect("valid account ID query URI");
            })
            .send()
            .await
            .expect_err("HTTP runtime has no A2A agent card");
        assert!(
            card_error
                .as_service_error()
                .is_some_and(|error| error.is_runtime_client_error())
        );

        let validation_error = client
            .invoke_agent_runtime()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id("too-short")
            .payload(Blob::new(b"{}"))
            .send()
            .await
            .expect_err("invalid session ID is modeled");
        assert!(
            validation_error
                .as_service_error()
                .is_some_and(|error| error.is_validation_exception())
        );
        server.abort();
    }

    #[tokio::test]
    async fn unmodified_sdk_decodes_nonzero_and_timed_out_commands() {
        let _guard = SDK_TEST_LOCK.lock().await;
        let (client, server) = sdk_against_edge().await;

        assert_eq!(
            run_command(&client, "exit 7").await.2,
            Some((7, "COMPLETED".to_owned()))
        );
        assert_eq!(
            run_command(&client, "timeout").await.2,
            Some((-1, "TIMED_OUT".to_owned()))
        );

        let mut failed = client
            .invoke_agent_runtime_command()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id(SESSION_ID)
            .body(
                InvokeAgentRuntimeCommandRequestBody::builder()
                    .command("error")
                    .build()
                    .expect("command body"),
            )
            .send()
            .await
            .expect("command starts before asynchronous failure");
        assert!(matches!(
            failed.stream.recv().await.expect("decode content start"),
            Some(InvokeAgentRuntimeCommandStreamOutput::Chunk(_))
        ));
        let error = failed
            .stream
            .recv()
            .await
            .expect_err("SDK decodes the framed runtime error");
        let message = format!("{error:?}");
        assert!(
            message.contains("runtime command execution failed"),
            "unexpected SDK event error: {message}"
        );
        assert!(!message.contains("container secret details"));
        server.abort();
    }

    #[tokio::test]
    async fn unmodified_sdk_is_cryptographically_verified_in_signature_mode() {
        let _guard = SDK_TEST_LOCK.lock().await;
        let empty_policy = AuthorizationPolicy {
            identity_statements: Vec::new(),
            resource_statements: Vec::new(),
        };
        let catalog = RuntimeCatalog::test_catalog_with_security(
            AuthenticationMode::Signature,
            empty_policy.clone(),
        );
        let (client, server) = sdk_against_catalog(catalog, "local-secret-key").await;
        client
            .invoke_agent_runtime()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id(SESSION_ID)
            .payload(Blob::new(b"signed payload"))
            .send()
            .await
            .expect("valid official SDK signature");
        server.abort();

        let catalog =
            RuntimeCatalog::test_catalog_with_security(AuthenticationMode::Signature, empty_policy);
        let (wrong_client, wrong_server) = sdk_against_catalog(catalog, "wrong-secret").await;
        let denied = wrong_client
            .invoke_agent_runtime()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id(SESSION_ID)
            .payload(Blob::new(b"signed payload"))
            .send()
            .await
            .expect_err("invalid signature is denied");
        assert!(
            denied
                .as_service_error()
                .is_some_and(|error| error.is_access_denied_exception())
        );
        wrong_server.abort();
    }

    #[tokio::test]
    async fn policy_mode_requires_invoke_for_user_permission() {
        let _guard = SDK_TEST_LOCK.lock().await;
        let policy = AuthorizationPolicy {
            identity_statements: vec![PolicyStatement {
                effect: PolicyEffect::Allow,
                actions: vec!["bedrock-agentcore:InvokeAgentRuntime".to_owned()],
                resources: vec![RUNTIME_ARN.to_owned()],
                principals: Vec::new(),
                conditions: HashMap::new(),
            }],
            resource_statements: Vec::new(),
        };
        let catalog =
            RuntimeCatalog::test_catalog_with_security(AuthenticationMode::Policy, policy);
        let (client, server) = sdk_against_catalog(catalog, "local-secret-key").await;

        client
            .invoke_agent_runtime()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id(SESSION_ID)
            .payload(Blob::new(b"allowed"))
            .send()
            .await
            .expect("base invocation permission is allowed");
        let denied = client
            .invoke_agent_runtime()
            .agent_runtime_arn(RUNTIME_ARN)
            .runtime_session_id(SESSION_ID)
            .runtime_user_id("local-user")
            .payload(Blob::new(b"requires second permission"))
            .send()
            .await
            .expect_err("runtime user requires InvokeAgentRuntimeForUser");
        assert!(
            denied
                .as_service_error()
                .is_some_and(|error| error.is_access_denied_exception())
        );
        server.abort();
    }

    #[derive(Clone)]
    struct ProxySessionBackend {
        endpoint: String,
        starts: Arc<AtomicUsize>,
        stops: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SessionBackend for ProxySessionBackend {
        async fn start(
            &self,
            _key: &SessionKey,
            _runtime: Arc<crate::catalog::ResolvedRuntime>,
            _cancellation: CancellationToken,
        ) -> Result<SessionContainer, String> {
            let number = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(SessionContainer {
                id: format!("proxy-container-{number}"),
                endpoint: self.endpoint.clone(),
                age: Duration::ZERO,
            })
        }

        async fn ping(&self, _container: &SessionContainer) -> SessionHealth {
            SessionHealth::Healthy
        }

        async fn stop(&self, _container: &SessionContainer) -> Result<(), String> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn echo_runtime(request: Request) -> axum::response::Response {
        let content_type = request
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| "application/octet-stream".parse().expect("content type"));
        let body = to_bytes(request.into_body(), 1024)
            .await
            .expect("runtime request body");
        axum::response::Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .expect("runtime response")
    }

    #[tokio::test]
    async fn aws_edge_proxies_to_and_reuses_the_session_container() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("runtime fixture listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("runtime address"));
        let runtime_server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/invocations", post(echo_runtime)),
            )
            .await
            .expect("serve runtime fixture");
        });
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        let sessions = SessionManager::new(Arc::new(ProxySessionBackend {
            endpoint,
            starts: Arc::clone(&starts),
            stops: Arc::clone(&stops),
        }));
        let app = runtime_router_with_sessions(RuntimeCatalog::test_catalog(), sessions);
        let invoke = |payload: &'static [u8]| {
            Request::builder()
                .method("POST")
                .uri("/runtimes/arn%3Aaws%3Abedrock-agentcore%3Aus-east-1%3A000000000000%3Aruntime%2Fflint_local/invocations?qualifier=DEFAULT")
                .header(RUNTIME_SESSION_ID_HEADER, SESSION_ID)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from(payload))
                .expect("invoke request")
        };
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            let response = app
                .clone()
                .oneshot(invoke(payload))
                .await
                .expect("invoke response");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), 1024)
                    .await
                    .expect("invoke body"),
                payload,
            );
        }
        assert_eq!(starts.load(Ordering::SeqCst), 1);

        let stopped = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/runtimes/arn%3Aaws%3Abedrock-agentcore%3Aus-east-1%3A000000000000%3Aruntime%2Fflint_local/stopruntimesession?qualifier=DEFAULT")
                    .header(RUNTIME_SESSION_ID_HEADER, SESSION_ID)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"clientToken":"10000000-0000-0000-0000-000000000001"}"#))
                    .expect("stop request"),
            )
            .await
            .expect("stop response");
        assert_eq!(stopped.status(), StatusCode::OK);
        assert_eq!(stops.load(Ordering::SeqCst), 1);

        let response = app
            .oneshot(invoke(b"third"))
            .await
            .expect("reinvoke response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        runtime_server.abort();
    }
}
