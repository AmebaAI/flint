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
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use bollard::query_parameters::ListContainersOptionsBuilder;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{InvocationRuntime, ScaffoldContainerRuntime, router};
use crate::runtime::{
    ContainerFailure, ContainerInvocation, ContainerOutput, ContainerRuntime, RuntimeLimits,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn invocation_request(invocation_id: &str, identity_id: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/runtimes/arn%3Aaws%3Abedrock-agentcore%3Aus-east-1%3A000000000000%3Aruntime%2Fflint_local/invocations")
        .header("content-type", "application/json")
        .header("x-amzn-bedrock-agentcore-runtime-session-id", identity_id)
        .body(Body::from(
            json!({
                "invocationId": invocation_id,
                "workspaceId": "10000000-0000-0000-0000-000000000001",
                "fencingToken": 1,
                "backendCredential": "secret-test-credential",
                "input": {"assignment": "fixture"},
            })
            .to_string(),
        ))
        .expect("invocation request")
}

fn stop_session_request(identity_id: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/runtimes/arn%3Aaws%3Abedrock-agentcore%3Aus-east-1%3A000000000000%3Aruntime%2Fflint_local/stopruntimesession")
        .header("content-type", "application/json")
        .header("x-amzn-bedrock-agentcore-runtime-session-id", identity_id)
        .body(Body::from(
            json!({"clientToken": "10000000-0000-0000-0000-000000000001"}).to_string(),
        ))
        .expect("stop runtime session request")
}

async fn json_body(response: axum::response::Response) -> Value {
    let body = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("response body");
    serde_json::from_slice(&body).expect("response JSON")
}

#[tokio::test]
async fn product_specific_internal_runtime_control_routes_are_not_exposed() {
    let app = router(InvocationRuntime::new(Arc::new(ScaffoldContainerRuntime)));
    for request in [
        Request::builder()
            .uri("/internal/invocations")
            .body(Body::empty())
            .expect("internal diagnostics request"),
        Request::builder()
            .method("POST")
            .uri("/internal/runtime/workspaces/10000000-0000-0000-0000-000000000001/pause")
            .body(Body::empty())
            .expect("internal pause request"),
        Request::builder()
            .method("POST")
            .uri("/internal/runtime/workspaces/10000000-0000-0000-0000-000000000001/resume")
            .body(Body::empty())
            .expect("internal resume request"),
    ] {
        let response = app.clone().oneshot(request).await.expect("route response");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

struct CancellableContainerRuntime {
    started: Notify,
}

#[async_trait]
impl ContainerRuntime for CancellableContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.started.notify_one();
        cancellation.cancelled().await;
        Err(ContainerFailure::Retryable)
    }
}

struct CostlyContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for CostlyContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContainerOutput {
            stdout: br#"{"status":"completed","usage":{"costUsdMicros":101}}"#.to_vec(),
        })
    }
}

struct OversizedOutputContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for OversizedOutputContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContainerOutput {
            stdout: format!(
                "{{\"status\":\"completed\",\"padding\":\"{}\"}}",
                "x".repeat(2 * 1024 * 1024),
            )
            .into_bytes(),
        })
    }
}

struct NeverCompletesContainerRuntime;

#[async_trait]
impl ContainerRuntime for NeverCompletesContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        cancellation.cancelled().await;
        Err(ContainerFailure::Retryable)
    }
}

struct CleanupFailureContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for CleanupFailureContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ContainerFailure::CleanupFailed)
    }
}

struct AlwaysFailContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for AlwaysFailContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ContainerFailure::Retryable)
    }
}

struct RejectingContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for RejectingContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ContainerFailure::Rejected {
            code: "model_not_found".to_owned(),
            message: "configured model is unavailable".to_owned(),
        })
    }
}

struct UnknownStatusContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for UnknownStatusContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContainerOutput {
            stdout: br#"{"status":"not-a-terminal-status"}"#.to_vec(),
        })
    }
}

struct InvalidOnceContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for InvalidOnceContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        let stdout = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            b"not-json".to_vec()
        } else {
            br#"{"status":"completed","result":{"kind":"valid-after-retry"}}"#.to_vec()
        };
        Ok(ContainerOutput { stdout })
    }
}

struct FailOnceContainerRuntime {
    calls: AtomicUsize,
}

#[async_trait]
impl ContainerRuntime for FailOnceContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(ContainerFailure::Retryable);
        }
        Ok(ContainerOutput {
            stdout: br#"{"status":"completed","result":{"kind":"recovered"}}"#.to_vec(),
        })
    }
}

struct BlockingFirstContainerRuntime {
    calls: AtomicUsize,
    first_started: Notify,
    release_first: Notify,
}

impl BlockingFirstContainerRuntime {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
        }
    }
}

#[async_trait]
impl ContainerRuntime for BlockingFirstContainerRuntime {
    async fn run(
        &self,
        _invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            self.release_first.notified().await;
        }
        Ok(ContainerOutput {
            stdout: br#"{"status":"completed","result":{"kind":"fixture"}}"#.to_vec(),
        })
    }
}

#[tokio::test]
async fn private_diagnostics_retains_only_the_configured_history() {
    let runtime = InvocationRuntime::with_limits(
        Arc::new(ScaffoldContainerRuntime),
        RuntimeLimits {
            max_diagnostic_entries: 2,
            ..RuntimeLimits::default()
        },
    );
    let app = router(runtime);
    for invocation_id in [
        "50000000-0000-0000-0000-000000000001",
        "50000000-0000-0000-0000-000000000002",
        "50000000-0000-0000-0000-000000000003",
    ] {
        let response = app
            .clone()
            .oneshot(invocation_request(
                invocation_id,
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("invocation response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");
    let diagnostics = json_body(diagnostics).await;
    let invocations = diagnostics["invocations"]
        .as_array()
        .expect("invocation diagnostics");
    assert_eq!(invocations.len(), 2);
    assert_ne!(
        invocations[0]["invocationId"],
        invocations[1]["invocationId"],
    );
}

#[tokio::test]
async fn completed_invocation_is_visible_in_private_diagnostics() {
    let app = router(InvocationRuntime::new(Arc::new(ScaffoldContainerRuntime)));
    let response = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000001",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("invocation response");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"status": "completed", "result": {"kind": "fixture"}}),
    );

    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");

    assert_eq!(diagnostics.status(), StatusCode::OK);
    let diagnostics = json_body(diagnostics).await;
    assert_eq!(diagnostics["activeCount"], json!(0));
    assert_eq!(diagnostics["invocations"][0]["status"], json!("completed"));
    assert_eq!(diagnostics["invocations"][0]["attempts"], json!(1));
    assert_eq!(diagnostics["invocations"][0]["containersRemoved"], json!(1),);
    assert_eq!(
        diagnostics["invocations"][0]["attemptIds"]
            .as_array()
            .expect("attempt ids")
            .len(),
        1,
    );
    assert!(!diagnostics.to_string().contains("secret-test-credential"));
}

#[tokio::test]
async fn active_invocation_can_be_cancelled() {
    let containers = Arc::new(CancellableContainerRuntime {
        started: Notify::new(),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let invocation_app = app.clone();
    let invocation = tokio::spawn(async move {
        invocation_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000017",
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("invocation response")
    });
    containers.started.notified().await;

    let cancellation = app
        .clone()
        .oneshot(stop_session_request("20000000-0000-0000-0000-000000000001"))
        .await
        .expect("cancellation response");

    assert_eq!(cancellation.status(), StatusCode::OK);
    assert!(
        to_bytes(cancellation.into_body(), 1024)
            .await
            .expect("stop response body")
            .is_empty()
    );
    let invocation = invocation.await.expect("invocation task");
    assert_eq!(invocation.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(
        json_body(invocation).await["code"],
        json!("invocation_cancelled")
    );

    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");
    let diagnostics = json_body(diagnostics).await;
    assert_eq!(diagnostics["activeCount"], json!(0));
    assert_eq!(diagnostics["invocations"][0]["status"], json!("cancelled"));
    assert_eq!(
        diagnostics["invocations"][0]["lastFailure"],
        json!("cancelled"),
    );
}

#[tokio::test]
async fn reported_cost_over_the_configured_limit_fails_without_retrying() {
    let containers = Arc::new(CostlyContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let runtime = InvocationRuntime::with_limits(
        containers.clone(),
        RuntimeLimits {
            max_cost_usd_micros: Some(100),
            ..RuntimeLimits::default()
        },
    );
    let app = router(runtime);
    let response = app
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000015",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("invocation response");

    assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(
        json_body(response).await["code"],
        json!("cost_limit_exceeded")
    );
    assert_eq!(containers.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn oversized_outcome_retries_then_fails_clearly() {
    let containers = Arc::new(OversizedOutputContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let response = app
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000016",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("invocation response");

    assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(json_body(response).await["code"], json!("invalid_outcome"));
    assert_eq!(containers.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn invocation_timeout_fails_without_retrying_and_releases_the_identity() {
    let runtime = InvocationRuntime::with_limits(
        Arc::new(NeverCompletesContainerRuntime),
        RuntimeLimits {
            attempt_timeout: Duration::from_millis(10),
            ..RuntimeLimits::default()
        },
    );
    let app = router(runtime);
    let response = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000018",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("timeout response");

    assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(
        json_body(response).await["code"],
        json!("invocation_timed_out")
    );
    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");
    let diagnostics = json_body(diagnostics).await;
    assert_eq!(diagnostics["activeCount"], json!(0));
    assert_eq!(diagnostics["invocations"][0]["attempts"], json!(1));
    assert_eq!(
        diagnostics["invocations"][0]["lastFailure"],
        json!("timeout"),
    );
}

#[tokio::test]
async fn cleanup_failure_blocks_new_containers_for_the_identity() {
    let containers = Arc::new(CleanupFailureContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let failed = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000013",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("cleanup failure response");
    assert_eq!(failed.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(
        json_body(failed).await["code"],
        json!("container_cleanup_failed")
    );

    let blocked = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000014",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("blocked response");
    assert_eq!(blocked.status(), StatusCode::CONFLICT);
    assert_eq!(containers.calls.load(Ordering::SeqCst), 1);

    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");
    let diagnostics = json_body(diagnostics).await;
    assert_eq!(diagnostics["activeCount"], json!(1));
    assert_eq!(
        diagnostics["invocations"][0]["status"],
        json!("cleanup_failed")
    );
    assert_eq!(diagnostics["invocations"][0]["containersRemoved"], json!(0),);
}

#[tokio::test]
async fn deterministic_agent_failure_is_clear_and_not_retried() {
    let containers = Arc::new(RejectingContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let failed = app
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000025",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("failed response");

    assert_eq!(failed.status(), StatusCode::FAILED_DEPENDENCY);
    let failure = json_body(failed).await;
    assert_eq!(failure["code"], json!("model_not_found"));
    assert_eq!(
        failure["detail"],
        json!("agent rejected the invocation (model_not_found): configured model is unavailable"),
    );
    assert_eq!(containers.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exhausted_crash_retries_fail_clearly_and_release_the_identity() {
    let containers = Arc::new(AlwaysFailContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let failed = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000019",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("failed response");

    assert_eq!(failed.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(json_body(failed).await["code"], json!("attempts_exhausted"));
    assert_eq!(containers.calls.load(Ordering::SeqCst), 3);

    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");
    let diagnostics = json_body(diagnostics).await;
    assert_eq!(diagnostics["activeCount"], json!(0));
    assert_eq!(diagnostics["invocations"][0]["status"], json!("failed"));
    assert_eq!(diagnostics["invocations"][0]["attempts"], json!(3));
    assert_eq!(diagnostics["invocations"][0]["containersRemoved"], json!(3),);
    assert_eq!(
        diagnostics["invocations"][0]["lastFailure"],
        json!("container_crash"),
    );
    assert_eq!(
        diagnostics["invocations"][0]["attemptIds"]
            .as_array()
            .expect("attempt ids")
            .len(),
        3,
    );
}

#[tokio::test]
async fn unknown_outcome_status_retries_then_fails_validation() {
    let containers = Arc::new(UnknownStatusContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let response = app
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000012",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("invocation response");

    assert_eq!(response.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(json_body(response).await["code"], json!("invalid_outcome"));
    assert_eq!(containers.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn invalid_outcome_retries_in_a_fresh_container() {
    let containers = Arc::new(InvalidOnceContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let response = app
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000020",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("invocation response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"status": "completed", "result": {"kind": "valid-after-retry"}}),
    );
    assert_eq!(containers.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn crashed_attempt_retries_in_a_fresh_container() {
    let containers = Arc::new(FailOnceContainerRuntime {
        calls: AtomicUsize::new(0),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let response = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000021",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("invocation response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!({"status": "completed", "result": {"kind": "recovered"}}),
    );
    assert_eq!(containers.calls.load(Ordering::SeqCst), 2);

    let diagnostics = app
        .oneshot(
            Request::builder()
                .uri("/_local/invocations")
                .body(Body::empty())
                .expect("diagnostics request"),
        )
        .await
        .expect("diagnostics response");
    let diagnostics = json_body(diagnostics).await;
    assert_eq!(diagnostics["invocations"][0]["attempts"], json!(2));
    assert_eq!(diagnostics["invocations"][0]["containersRemoved"], json!(2),);
}

#[tokio::test]
async fn agentcore_ping_reports_busy_while_an_invocation_is_active() {
    let containers = Arc::new(BlockingFirstContainerRuntime::new());
    let app = router(InvocationRuntime::new(containers.clone()));
    let invocation_app = app.clone();
    let invocation = tokio::spawn(async move {
        invocation_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000030",
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("invocation response")
    });
    containers.first_started.notified().await;

    let busy = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/_local/ping")
                .body(Body::empty())
                .expect("busy ping request"),
        )
        .await
        .expect("busy ping response");
    assert_eq!(busy.status(), StatusCode::OK);
    assert_eq!(json_body(busy).await, json!({"status": "HealthyBusy"}));

    containers.release_first.notify_one();
    assert_eq!(
        invocation.await.expect("invocation task").status(),
        StatusCode::OK
    );
    let healthy = app
        .oneshot(
            Request::builder()
                .uri("/_local/ping")
                .body(Body::empty())
                .expect("healthy ping request"),
        )
        .await
        .expect("healthy ping response");
    assert_eq!(json_body(healthy).await, json!({"status": "Healthy"}));
}

#[tokio::test]
async fn queued_invocation_cancels_before_starting_a_container() {
    let containers = Arc::new(BlockingFirstContainerRuntime::new());
    let runtime = InvocationRuntime::with_limits(
        containers.clone(),
        RuntimeLimits {
            max_concurrency: 1,
            ..RuntimeLimits::default()
        },
    );
    let app = router(runtime);
    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000061",
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("first response")
    });
    containers.first_started.notified().await;

    let queued_app = app.clone();
    let queued = tokio::spawn(async move {
        queued_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000062",
                "20000000-0000-0000-0000-000000000002",
            ))
            .await
            .expect("queued response")
    });
    tokio::task::yield_now().await;
    let cancellation = app
        .oneshot(stop_session_request("20000000-0000-0000-0000-000000000002"))
        .await
        .expect("cancellation response");
    assert_eq!(cancellation.status(), StatusCode::OK);

    let queued = tokio::time::timeout(Duration::from_millis(100), queued)
        .await
        .expect("queued invocation cancels before a permit is available")
        .expect("queued task");
    assert_eq!(queued.status(), StatusCode::FAILED_DEPENDENCY);
    assert_eq!(containers.calls.load(Ordering::SeqCst), 1);

    containers.release_first.notify_one();
    assert_eq!(first.await.expect("first task").status(), StatusCode::OK);
}

#[tokio::test]
async fn configured_global_concurrency_queues_other_identities() {
    let containers = Arc::new(BlockingFirstContainerRuntime::new());
    let runtime = InvocationRuntime::with_limits(
        containers.clone(),
        RuntimeLimits {
            max_concurrency: 1,
            ..RuntimeLimits::default()
        },
    );
    let app = router(runtime);
    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000031",
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("first response")
    });
    containers.first_started.notified().await;

    let second_app = app.clone();
    let second = tokio::spawn(async move {
        second_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000032",
                "20000000-0000-0000-0000-000000000002",
            ))
            .await
            .expect("second response")
    });
    tokio::task::yield_now().await;
    assert_eq!(containers.calls.load(Ordering::SeqCst), 1);

    containers.release_first.notify_one();
    assert_eq!(first.await.expect("first task").status(), StatusCode::OK);
    assert_eq!(second.await.expect("second task").status(), StatusCode::OK);
    assert_eq!(containers.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
#[ignore = "requires local Docker and the flint-runtime-fixture image"]
async fn real_docker_edge_reuses_session_and_stop_removes_it() {
    let config = super::RuntimeConfig::test_defaults();
    let runtime_owner = config.runtime_owner.clone();
    let app = super::production_router(config)
        .await
        .expect("production router");
    let runtime_session_id = "20000000-0000-0000-0000-000000000001";
    let mut responses = Vec::new();
    for invocation_id in [
        "50000000-0000-0000-0000-000000000041",
        "50000000-0000-0000-0000-000000000042",
    ] {
        let response = app
            .clone()
            .oneshot(invocation_request(invocation_id, runtime_session_id))
            .await
            .expect("Docker invocation response");
        let status = response.status();
        let body = json_body(response).await;
        responses.push((status, body["code"].clone()));
    }

    let docker = bollard::Docker::connect_with_local_defaults().expect("Docker connection");
    let filters = HashMap::from([(
        "label".to_owned(),
        vec![
            "agentcore.emulator.managed=true".to_owned(),
            format!("agentcore.emulator.owner={runtime_owner}"),
            format!("agentcore.emulator.runtime-session-id={runtime_session_id}"),
        ],
    )]);
    let options = ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let container_ids = docker
        .list_containers(Some(options))
        .await
        .expect("list managed session containers")
        .into_iter()
        .map(|container| container.id.expect("managed container ID"))
        .collect::<Vec<_>>();

    let stop = app
        .oneshot(stop_session_request(runtime_session_id))
        .await
        .expect("stop runtime session response");

    assert_eq!(
        responses,
        vec![(StatusCode::OK, json!(null)), (StatusCode::OK, json!(null)),]
    );
    assert_eq!(
        container_ids.len(),
        1,
        "same session must reuse one container"
    );
    assert_eq!(stop.status(), StatusCode::OK);
    for container_id in container_ids {
        let error = docker
            .inspect_container(&container_id, None)
            .await
            .expect_err("stopped session container must be removed");
        assert!(
            matches!(
                error,
                bollard::errors::Error::DockerResponseServerError {
                    status_code: 404,
                    ..
                }
            ),
            "unexpected Docker inspection error: {error}",
        );
    }
}

#[tokio::test]
async fn duplicate_session_cannot_replace_the_active_cancellation_handle() {
    let containers = Arc::new(CancellableContainerRuntime {
        started: Notify::new(),
    });
    let app = router(InvocationRuntime::new(containers.clone()));
    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000051",
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("first invocation response")
    });
    containers.started.notified().await;

    let duplicate = app
        .clone()
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000051",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("duplicate response");
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);

    let cancellation = app
        .oneshot(stop_session_request("20000000-0000-0000-0000-000000000001"))
        .await
        .expect("cancellation response");
    assert_eq!(cancellation.status(), StatusCode::OK);
    let first = tokio::time::timeout(Duration::from_millis(100), first)
        .await
        .expect("original invocation responds to cancellation")
        .expect("original invocation task");
    assert_eq!(first.status(), StatusCode::FAILED_DEPENDENCY);
}

#[tokio::test]
async fn concurrent_delivery_for_one_identity_is_rejected() {
    let containers = Arc::new(BlockingFirstContainerRuntime::new());
    let app = router(InvocationRuntime::new(containers.clone()));
    let first_app = app.clone();
    let first = tokio::spawn(async move {
        first_app
            .oneshot(invocation_request(
                "50000000-0000-0000-0000-000000000011",
                "20000000-0000-0000-0000-000000000001",
            ))
            .await
            .expect("first invocation response")
    });
    containers.first_started.notified().await;

    let duplicate = app
        .oneshot(invocation_request(
            "50000000-0000-0000-0000-000000000012",
            "20000000-0000-0000-0000-000000000001",
        ))
        .await
        .expect("duplicate response");

    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    assert_eq!(
        json_body(duplicate).await["code"],
        json!("identity_already_active"),
    );
    containers.release_first.notify_one();
    assert_eq!(
        first.await.expect("first invocation task").status(),
        StatusCode::OK
    );
    assert_eq!(containers.calls.load(Ordering::SeqCst), 1);
}
