use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn health_reports_flint_ready() {
    let response = flint::health_app()
        .oneshot(
            Request::builder()
                .uri("/_local/health")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("health response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024)
        .await
        .expect("health body");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).expect("health JSON"),
        json!({"service": "flint", "status": "ready"}),
    );
}
