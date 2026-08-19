use std::{collections::HashMap, sync::Arc};

#[cfg(test)]
use async_trait::async_trait;
use axum::{Json, Router, extract::State, routing::get};
#[cfg(test)]
use axum::{extract::Path, http::StatusCode};
use catalog::{RuntimeCatalog, RuntimeRegistry, RuntimeRegistryHealth};
use config::{RuntimeConfig, RuntimeSourceConfig};
use docker::{DockerBackendError, DockerSessionBackend};
use edge::RuntimeApiState;
#[cfg(test)]
use runtime::{
    ContainerFailure, ContainerInvocation, ContainerOutput, ContainerRuntime, InvocationError,
    InvocationRuntime,
};
use serde::Serialize;
use serde_json::{Value, json};
use session::{SessionBackend, SessionError, SessionManager};
use session_store::SessionStore;
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
mod session_store;

#[cfg(test)]
mod tests;

#[derive(Serialize)]
struct Health {
    service: &'static str,
    status: &'static str,
}

#[derive(Serialize)]
struct RuntimeHealth {
    service: &'static str,
    status: &'static str,
    #[serde(flatten)]
    registry: RuntimeRegistryHealth,
}

pub async fn app() -> Result<Router, StartupError> {
    production_router(RuntimeConfig::from_env()?).await
}

async fn production_router(config: RuntimeConfig) -> Result<Router, StartupError> {
    let RuntimeConfig {
        runtime_owner,
        runtime_source,
        state_path,
        session_storage_mount_path,
        health_check_interval,
    } = config;
    let store = SessionStore::open(&state_path)?;
    tracing::info!(path = %store.path().display(), "opened runtime session state");
    let (catalog, discovery) = match runtime_source {
        RuntimeSourceConfig::Catalog { catalog } => (catalog, None),
        RuntimeSourceConfig::Docker(discovery) => {
            tracing::info!(
                refresh_seconds = discovery.refresh_interval.as_secs(),
                "configured Docker runtime discovery"
            );
            (RuntimeCatalog::empty_discovery(), Some(discovery))
        }
    };
    let registry = RuntimeRegistry::new(catalog);
    let session_backend = Arc::new(
        DockerSessionBackend::connect_with_registry(
            runtime_owner,
            registry.clone(),
            discovery,
            session_storage_mount_path,
        )
        .await?,
    );
    let catalog = registry.snapshot();
    if let Some(deployment) = catalog.default_snapshot_opt() {
        tracing::info!(
            source = %catalog.source(),
            runtime_arn = %deployment.runtime_arn,
            qualifier = %deployment.qualifier,
            "loaded runtime catalog snapshot"
        );
    } else {
        tracing::info!(
            source = %catalog.source(),
            deployment_count = catalog.len(),
            "started with an empty runtime registry"
        );
    }
    let mut adopted = session_backend
        .take_adopted_sessions()
        .await
        .into_iter()
        .map(|(key, deployment, container)| (key, (deployment, container)))
        .collect::<HashMap<_, _>>();
    let records = store.load_all()?;
    let sessions = SessionManager::with_store(
        Arc::clone(&session_backend) as Arc<dyn SessionBackend>,
        store,
        health_check_interval,
    );
    for record in records {
        let deployment = registry
            .resolve_stored(&record.key.runtime_arn, Some(&record.key.qualifier))
            .map_err(|error| {
                format!(
                    "stored runtime session {} cannot resolve its deployment pin: {error}",
                    record.key.runtime_session_id
                )
            })
            .and_then(|deployment| {
                sessions
                    .validate_record(&deployment, &record)
                    .map(|()| deployment)
                    .map_err(|error| match error {
                        SessionError::DeploymentDrift(message) => message,
                        error => error.to_string(),
                    })
            });
        let deployment = match deployment {
            Ok(deployment) => deployment,
            Err(message) => {
                tracing::warn!(
                    runtime_session_id = %record.key.runtime_session_id,
                    %message,
                    "stored runtime session is blocked by deployment drift"
                );
                if let Some((_, container)) = adopted.remove(&record.key) {
                    session_backend
                        .stop(&container)
                        .await
                        .map_err(|error| StartupError {
                            message: format!(
                                "failed to remove drifted runtime session compute: {error}"
                            ),
                        })?;
                }
                sessions
                    .restore_drifted(&record, message)
                    .map_err(|error| StartupError {
                        message: format!("failed to quarantine drifted runtime session: {error}"),
                    })?;
                continue;
            }
        };
        if let Some((adopted_deployment, container)) = adopted.remove(&record.key) {
            sessions
                .adopt(adopted_deployment, record.key, container)
                .map_err(|error| StartupError {
                    message: format!("failed to adopt stored runtime session: {error}"),
                })?;
        } else {
            sessions
                .restore_stopped(deployment, &record)
                .map_err(|error| StartupError {
                    message: format!("failed to restore stopped runtime session: {error}"),
                })?;
        }
    }
    for (key, (deployment, container)) in adopted {
        sessions
            .adopt(deployment, key, container)
            .map_err(|error| StartupError {
                message: format!("failed to adopt legacy runtime session: {error}"),
            })?;
    }
    Ok(runtime_router_with_registry(registry, sessions))
}

pub fn health_app() -> Router {
    Router::new().route("/_local/health", get(health))
}

#[cfg(test)]
fn runtime_router(catalog: RuntimeCatalog, runtime: InvocationRuntime) -> Router {
    runtime_router_with_state(RuntimeApiState::new(RuntimeRegistry::new(catalog), runtime))
}

#[cfg(test)]
fn runtime_router_with_sessions(catalog: RuntimeCatalog, sessions: SessionManager) -> Router {
    runtime_router_with_registry(RuntimeRegistry::new(catalog), sessions)
}

fn runtime_router_with_registry(registry: RuntimeRegistry, sessions: SessionManager) -> Router {
    runtime_router_with_state(RuntimeApiState::with_sessions(registry, sessions))
}

fn runtime_router_with_state(state: RuntimeApiState) -> Router {
    let router = Router::new()
        .merge(edge::routes())
        .route("/_local/health", get(runtime_health))
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

async fn runtime_health(State(state): State<RuntimeApiState>) -> Json<RuntimeHealth> {
    let registry = state.registry_health();
    Json(RuntimeHealth {
        service: "flint",
        status: registry.discovery_status,
        registry,
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

impl From<session_store::SessionStoreError> for StartupError {
    fn from(error: session_store::SessionStoreError) -> Self {
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
