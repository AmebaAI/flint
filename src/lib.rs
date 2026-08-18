use std::sync::Arc;

#[cfg(test)]
use async_trait::async_trait;
#[cfg(test)]
use axum::extract::State;
use axum::{Json, Router, routing::get};
#[cfg(test)]
use axum::{extract::Path, http::StatusCode};
use catalog::RuntimeCatalog;
use config::RuntimeConfig;
use docker::{DockerBackendError, DockerSessionBackend};
use edge::RuntimeApiState;
#[cfg(test)]
use runtime::{
    ContainerFailure, ContainerInvocation, ContainerOutput, ContainerRuntime, InvocationError,
    InvocationRuntime,
};
use serde::Serialize;
use serde_json::{Value, json};
use session::SessionManager;
#[cfg(test)]
use tokio_util::sync::CancellationToken;
#[cfg(test)]
use uuid::Uuid;

mod auth;
mod catalog;
mod command;
mod config;
mod docker;
mod edge;
mod proxy;
#[cfg(test)]
mod runtime;
mod session;

#[cfg(test)]
mod tests;

#[derive(Serialize)]
struct Health {
    service: &'static str,
    status: &'static str,
}

pub async fn app() -> Result<Router, StartupError> {
    production_router(RuntimeConfig::from_env()?).await
}

async fn production_router(config: RuntimeConfig) -> Result<Router, StartupError> {
    let catalog = config.catalog;
    let deployment = catalog.default_snapshot();
    tracing::info!(
        catalog = %catalog.source().display(),
        runtime_arn = %deployment.runtime_arn,
        qualifier = %deployment.qualifier,
        "loaded immutable runtime catalog snapshot"
    );
    let runtime_owner = config.runtime_owner;
    let session_backend =
        Arc::new(DockerSessionBackend::connect(runtime_owner, catalog.clone()).await?);
    let adopted = session_backend.take_adopted_sessions().await;
    let sessions = SessionManager::new(session_backend);
    for (key, deployment, container) in adopted {
        sessions
            .adopt(deployment, key, container)
            .map_err(|error| StartupError {
                message: format!("failed to adopt runtime session: {error}"),
            })?;
    }
    Ok(runtime_router_with_sessions(catalog, sessions))
}

pub fn health_app() -> Router {
    Router::new().route("/_local/health", get(health))
}

#[cfg(test)]
fn runtime_router(catalog: RuntimeCatalog, runtime: InvocationRuntime) -> Router {
    runtime_router_with_state(RuntimeApiState::new(catalog, runtime))
}

fn runtime_router_with_sessions(catalog: RuntimeCatalog, sessions: SessionManager) -> Router {
    runtime_router_with_state(RuntimeApiState::with_sessions(catalog, sessions))
}

fn runtime_router_with_state(state: RuntimeApiState) -> Router {
    let router = Router::new()
        .merge(edge::routes())
        .route("/_local/health", get(health))
        .route("/_local/ping", get(ping));
    #[cfg(test)]
    let router = router
        .route(
            "/_local/invocations/{invocation_id}",
            axum::routing::delete(cancel),
        )
        .route("/_local/invocations", get(diagnostics));
    router.with_state(state)
}

#[cfg(test)]
fn router(runtime: InvocationRuntime) -> Router {
    runtime_router(RuntimeCatalog::test_catalog(), runtime)
}

async fn health() -> Json<Health> {
    Json(Health {
        service: "flint",
        status: "ready",
    })
}

#[cfg(test)]
async fn ping(State(state): State<RuntimeApiState>) -> Json<Value> {
    let active_count = match state.runtime() {
        Some(runtime) => runtime.active_count().await,
        None => 0,
    };
    let status = if active_count == 0 {
        "Healthy"
    } else {
        "HealthyBusy"
    };
    Json(json!({"status": status}))
}

#[cfg(not(test))]
async fn ping() -> Json<Value> {
    Json(json!({"status": "Healthy"}))
}

#[cfg(test)]
async fn cancel(
    State(state): State<RuntimeApiState>,
    Path(invocation_id): Path<Uuid>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if let Some(runtime) = state.runtime()
        && runtime.cancel(invocation_id).await
    {
        return Ok((StatusCode::ACCEPTED, Json(json!({"status": "cancelling"}))));
    }
    Err((
        StatusCode::NOT_FOUND,
        Json(json!({
            "code": "invocation_not_active",
            "detail": InvocationError::InvocationNotActive.to_string(),
        })),
    ))
}

#[cfg(test)]
async fn diagnostics(State(state): State<RuntimeApiState>) -> Json<runtime::RuntimeDiagnostics> {
    Json(
        state
            .runtime()
            .expect("legacy diagnostics require the test runtime")
            .diagnostics()
            .await,
    )
}

#[cfg(test)]
struct ScaffoldContainerRuntime;

#[cfg(test)]
#[async_trait]
impl ContainerRuntime for ScaffoldContainerRuntime {
    async fn run(
        &self,
        invocation: ContainerInvocation,
        _cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        let _ = (
            invocation.invocation_id,
            invocation.attempt_id,
            invocation.agent_identity_id,
            invocation.fencing_token,
            invocation.backend_credential,
            invocation.input,
            invocation.max_cost_usd_micros,
        );
        Ok(ContainerOutput {
            stdout: br#"{"status":"completed","result":{"kind":"fixture"}}"#.to_vec(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StartupError {
    message: String,
}

impl From<config::ConfigError> for StartupError {
    fn from(error: config::ConfigError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

impl From<DockerBackendError> for StartupError {
    fn from(error: DockerBackendError) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}
