use std::sync::Arc;

use aws_sdk_bedrockagentcore::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::Blob,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, Uri},
};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};

const RUNTIME_ARN: &str = "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/flint-local";
const RUNTIME_SESSION_ID: &str = "20000000-0000-0000-0000-000000000001";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompatibilityStatus {
    Core,
    DeferredModelled,
    DeferredPendingModel,
}

const RUNTIME_OPERATION_SCOPE: &[(&str, CompatibilityStatus)] = &[
    ("InvokeAgentRuntime", CompatibilityStatus::Core),
    ("InvokeAgentRuntimeCommand", CompatibilityStatus::Core),
    ("StopRuntimeSession", CompatibilityStatus::Core),
    ("GetAgentCard", CompatibilityStatus::Core),
    (
        "DeleteCapacityProviderSession",
        CompatibilityStatus::DeferredModelled,
    ),
    (
        "InvokeAgentRuntimeWithWebSocketStream",
        CompatibilityStatus::DeferredPendingModel,
    ),
    (
        "InvokeAgentRuntimeCommandShell",
        CompatibilityStatus::DeferredPendingModel,
    ),
    (
        "GetRuntimeProtectedResourceMetadata",
        CompatibilityStatus::DeferredPendingModel,
    ),
];

#[derive(Debug)]
struct CapturedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct ResponseSpec {
    status: StatusCode,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static [u8],
}

#[derive(Clone)]
struct CaptureState {
    requests: mpsc::Sender<CapturedRequest>,
    response: ResponseSpec,
}

struct CaptureServer {
    endpoint: String,
    requests: mpsc::Receiver<CapturedRequest>,
    task: JoinHandle<()>,
}

impl CaptureServer {
    async fn start(response: ResponseSpec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind SDK capture server");
        let address = listener.local_addr().expect("capture server address");
        let (requests, receiver) = mpsc::channel(1);
        let app = Router::new()
            .fallback(capture)
            .with_state(Arc::new(CaptureState { requests, response }));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve SDK capture server");
        });
        Self {
            endpoint: format!("http://{address}"),
            requests: receiver,
            task,
        }
    }

    async fn next_request(&mut self) -> CapturedRequest {
        self.requests
            .recv()
            .await
            .expect("SDK sent one captured request")
    }
}

impl Drop for CaptureServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn capture(
    State(state): State<Arc<CaptureState>>,
    request: Request,
) -> axum::response::Response {
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, 100_000_000)
        .await
        .expect("capture SDK request body")
        .to_vec();
    state
        .requests
        .send(CapturedRequest {
            method: parts.method,
            uri: parts.uri,
            headers: parts.headers,
            body,
        })
        .await
        .expect("capture request receiver remains open");

    let mut response = axum::response::Response::builder().status(state.response.status);
    for (name, value) in &state.response.headers {
        response = response.header(*name, *value);
    }
    response
        .body(Body::from(state.response.body))
        .expect("valid capture response")
}

fn sdk_client(endpoint: &str) -> Client {
    let config = aws_sdk_bedrockagentcore::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-west-2"))
        .credentials_provider(Credentials::new(
            "local-access-key",
            "local-secret-key",
            None,
            None,
            "flint-conformance",
        ))
        .endpoint_url(endpoint)
        .build();
    Client::from_conf(config)
}

fn success_response() -> ResponseSpec {
    ResponseSpec {
        status: StatusCode::OK,
        headers: vec![
            ("content-type", "application/json"),
            (
                "x-amzn-bedrock-agentcore-runtime-session-id",
                RUNTIME_SESSION_ID,
            ),
            ("x-amzn-requestid", "10000000-0000-0000-0000-000000000001"),
        ],
        body: br#"{"ok":true}"#,
    }
}

#[test]
fn compatibility_target_is_pinned_and_runtime_scope_is_explicit() {
    assert_eq!(aws_sdk_bedrockagentcore::meta::PKG_VERSION, "1.60.0");

    let core = RUNTIME_OPERATION_SCOPE
        .iter()
        .filter_map(|(operation, status)| {
            (*status == CompatibilityStatus::Core).then_some(*operation)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        core,
        [
            "InvokeAgentRuntime",
            "InvokeAgentRuntimeCommand",
            "StopRuntimeSession",
            "GetAgentCard",
        ]
    );

    let client = sdk_client("http://127.0.0.1:9");
    let _ = client.invoke_agent_runtime();
    let _ = client.invoke_agent_runtime_command();
    let _ = client.stop_runtime_session();
    let _ = client.get_agent_card();
    let _ = client.delete_capacity_provider_session();
}

#[tokio::test]
async fn official_sdk_encodes_full_arn_and_preserves_opaque_invocation_payload() {
    let mut server = CaptureServer::start(success_response()).await;
    let payload = b"\x00opaque\xffpayload";

    let output = sdk_client(&server.endpoint)
        .invoke_agent_runtime()
        .agent_runtime_arn(RUNTIME_ARN)
        .runtime_session_id(RUNTIME_SESSION_ID)
        .qualifier("dev")
        .content_type("application/octet-stream")
        .accept("application/json")
        .payload(Blob::new(payload))
        .send()
        .await
        .expect("SDK decodes a successful invocation response");

    assert_eq!(output.runtime_session_id(), Some(RUNTIME_SESSION_ID));
    assert_eq!(output.content_type(), "application/json");
    assert_eq!(output.status_code(), Some(200));
    assert_eq!(
        output
            .response
            .collect()
            .await
            .expect("collect SDK response")
            .into_bytes()
            .as_ref(),
        br#"{"ok":true}"#,
    );

    let request = server.next_request().await;
    assert_eq!(request.method, Method::POST);
    assert_eq!(
        request.uri.path(),
        "/runtimes/arn%3Aaws%3Abedrock-agentcore%3Aus-west-2%3A123456789012%3Aruntime%2Fflint-local/invocations"
    );
    assert_eq!(request.uri.query(), Some("qualifier=dev"));
    assert_eq!(request.body, payload);
    assert_eq!(
        request
            .headers
            .get("x-amzn-bedrock-agentcore-runtime-session-id")
            .and_then(|value| value.to_str().ok()),
        Some(RUNTIME_SESSION_ID)
    );
    assert_eq!(
        request
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/octet-stream")
    );
    let authorization = request
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("official SDK signs the invocation");
    assert!(authorization.starts_with("AWS4-HMAC-SHA256 "));
    assert!(authorization.contains("Credential=local-access-key/"));
    assert!(authorization.contains("/us-west-2/bedrock-agentcore/aws4_request"));
}

#[tokio::test]
async fn official_sdk_uses_agent_id_with_account_id_query_parameter() {
    let mut server = CaptureServer::start(success_response()).await;

    sdk_client(&server.endpoint)
        .invoke_agent_runtime()
        .agent_runtime_arn("flint-local")
        .account_id("123456789012")
        .runtime_session_id(RUNTIME_SESSION_ID)
        .payload(Blob::new(Vec::new()))
        .send()
        .await
        .expect("SDK invokes by agent ID");

    let request = server.next_request().await;
    assert_eq!(request.uri.path(), "/runtimes/flint-local/invocations");
    assert_eq!(request.uri.query(), Some("accountId=123456789012"));
    assert!(request.body.is_empty());
}

#[tokio::test]
async fn official_sdk_uses_agent_id_and_account_id_for_stop_and_agent_card() {
    let mut stop_server = CaptureServer::start(success_response()).await;
    sdk_client(&stop_server.endpoint)
        .stop_runtime_session()
        .agent_runtime_arn("flint-local")
        .runtime_session_id(RUNTIME_SESSION_ID)
        .client_token("10000000-0000-0000-0000-000000000001")
        .customize()
        .mutate_request(|request| {
            let uri = format!("{}?accountId=123456789012", request.uri());
            request.set_uri(uri).expect("valid account ID query URI");
        })
        .send()
        .await
        .expect("SDK stops by agent ID");

    let stop = stop_server.next_request().await;
    assert_eq!(stop.method, Method::POST);
    assert_eq!(stop.uri.path(), "/runtimes/flint-local/stopruntimesession");
    assert_eq!(stop.uri.query(), Some("accountId=123456789012"));

    let mut card_server = CaptureServer::start(success_response()).await;
    sdk_client(&card_server.endpoint)
        .get_agent_card()
        .agent_runtime_arn("flint-local")
        .runtime_session_id(RUNTIME_SESSION_ID)
        .customize()
        .mutate_request(|request| {
            let uri = format!("{}?accountId=123456789012", request.uri());
            request.set_uri(uri).expect("valid account ID query URI");
        })
        .send()
        .await
        .expect("SDK gets an agent card by agent ID");

    let card = card_server.next_request().await;
    assert_eq!(card.method, Method::GET);
    assert_eq!(
        card.uri.path(),
        "/runtimes/flint-local/invocations/.well-known/agent-card.json"
    );
    assert_eq!(card.uri.query(), Some("accountId=123456789012"));
}

#[tokio::test]
async fn official_sdk_deserializes_agentcore_validation_errors() {
    let mut server = CaptureServer::start(ResponseSpec {
        status: StatusCode::BAD_REQUEST,
        headers: vec![
            ("content-type", "application/json"),
            ("x-amzn-errortype", "ValidationException"),
            ("x-amzn-requestid", "10000000-0000-0000-0000-000000000002"),
        ],
        body: br#"{"message":"runtime session id is invalid"}"#,
    })
    .await;

    let error = sdk_client(&server.endpoint)
        .invoke_agent_runtime()
        .agent_runtime_arn(RUNTIME_ARN)
        .runtime_session_id(RUNTIME_SESSION_ID)
        .payload(Blob::new(b"{}"))
        .send()
        .await
        .expect_err("SDK returns the modeled validation error");

    assert!(
        error
            .as_service_error()
            .is_some_and(|error| error.is_validation_exception())
    );
    let request = server.next_request().await;
    assert_eq!(request.method, Method::POST);
}
