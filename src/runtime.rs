use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InvocationRequest {
    pub(crate) invocation_id: Uuid,
    pub(crate) workspace_id: Uuid,
    pub(crate) fencing_token: i64,
    pub(crate) backend_credential: String,
    pub(crate) input: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct ContainerInvocation {
    pub(crate) invocation_id: Uuid,
    pub(crate) attempt_id: Uuid,
    pub(crate) agent_identity_id: Uuid,
    pub(crate) fencing_token: i64,
    pub(crate) backend_credential: String,
    pub(crate) input: Value,
    pub(crate) max_cost_usd_micros: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct ContainerOutput {
    pub(crate) stdout: Vec<u8>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub(crate) enum ContainerFailure {
    #[error("container execution failed after cleanup")]
    Retryable,
    #[error("agent rejected its configuration or invocation: {code}")]
    Rejected { code: String, message: String },
    #[error("container cleanup could not be confirmed")]
    CleanupFailed,
}

#[async_trait]
pub(crate) trait ContainerRuntime: Send + Sync {
    async fn run(
        &self,
        invocation: ContainerInvocation,
        cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure>;

    #[allow(dead_code)]
    async fn steer(&self, _invocation_id: Uuid, _message: Value) -> Result<(), ContainerFailure> {
        Err(ContainerFailure::Retryable)
    }

    async fn infrastructure_health(&self) -> RuntimeInfrastructureHealth {
        RuntimeInfrastructureHealth::default()
    }
}

#[derive(Clone)]
pub(crate) struct InvocationRuntime {
    containers: Arc<dyn ContainerRuntime>,
    active_identities: Arc<Mutex<HashSet<Uuid>>>,
    active_invocations: Arc<Mutex<HashMap<Uuid, ActiveInvocation>>>,
    diagnostics: Arc<Mutex<Vec<InvocationDiagnostic>>>,
    concurrency: Arc<Semaphore>,
    limits: RuntimeLimits,
}

#[derive(Clone)]
struct ActiveInvocation {
    cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeLimits {
    pub(crate) attempt_timeout: Duration,
    pub(crate) cleanup_timeout: Duration,
    pub(crate) max_attempts: usize,
    pub(crate) max_concurrency: usize,
    pub(crate) max_output_bytes: usize,
    pub(crate) max_cost_usd_micros: Option<u64>,
    pub(crate) max_diagnostic_entries: usize,
}

impl Default for RuntimeLimits {
    fn default() -> Self {
        Self {
            attempt_timeout: Duration::from_secs(5 * 60),
            cleanup_timeout: Duration::from_secs(10),
            max_attempts: 3,
            max_concurrency: 4,
            max_output_bytes: 2 * 1024 * 1024,
            max_cost_usd_micros: None,
            max_diagnostic_entries: 100,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InvocationDiagnostic {
    invocation_id: Uuid,
    workspace_id: Uuid,
    agent_identity_id: Uuid,
    fencing_token: i64,
    status: &'static str,
    attempts: usize,
    containers_removed: usize,
    attempt_ids: Vec<Uuid>,
    last_failure: Option<&'static str>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeInfrastructureHealth {
    pub(crate) docker_available: bool,
    pub(crate) open_ai_configured: bool,
    pub(crate) agent_image_available: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeDiagnostics {
    active_count: usize,
    invocations: Vec<InvocationDiagnostic>,
    infrastructure: RuntimeInfrastructureHealth,
}

impl InvocationRuntime {
    #[cfg(test)]
    pub(crate) fn new(containers: Arc<dyn ContainerRuntime>) -> Self {
        Self::with_limits(containers, RuntimeLimits::default())
    }

    pub(crate) fn with_limits(
        containers: Arc<dyn ContainerRuntime>,
        limits: RuntimeLimits,
    ) -> Self {
        Self {
            containers,
            active_identities: Arc::new(Mutex::new(HashSet::new())),
            active_invocations: Arc::new(Mutex::new(HashMap::new())),
            diagnostics: Arc::new(Mutex::new(Vec::new())),
            concurrency: Arc::new(Semaphore::new(limits.max_concurrency)),
            limits,
        }
    }

    pub(crate) async fn invoke(
        &self,
        agent_identity_id: Uuid,
        request: InvocationRequest,
    ) -> Result<Value, InvocationError> {
        if request.fencing_token < 1 {
            return Err(InvocationError::InvalidRequest);
        }
        if request.backend_credential.trim().is_empty() || !request.input.is_object() {
            return Err(InvocationError::InvalidRequest);
        }
        {
            let mut active_identities = self.active_identities.lock().await;
            if !active_identities.insert(agent_identity_id) {
                return Err(InvocationError::IdentityAlreadyActive);
            }
        }
        let invocation_id = request.invocation_id;
        let fencing_token = request.fencing_token;
        let workspace_id = request.workspace_id;
        let cancellation = CancellationToken::new();
        {
            let mut active_invocations = self.active_invocations.lock().await;
            if active_invocations.contains_key(&invocation_id) {
                self.active_identities
                    .lock()
                    .await
                    .remove(&agent_identity_id);
                return Err(InvocationError::IdentityAlreadyActive);
            }
            active_invocations.insert(
                invocation_id,
                ActiveInvocation {
                    cancellation: cancellation.clone(),
                },
            );
        }
        let base_invocation = ContainerInvocation {
            invocation_id,
            attempt_id: Uuid::nil(),
            agent_identity_id,
            fencing_token,
            backend_credential: request.backend_credential,
            input: request.input,
            max_cost_usd_micros: self.limits.max_cost_usd_micros,
        };
        let mut attempts = 0;
        let mut attempt_ids = Vec::new();
        let result = loop {
            let permit = tokio::select! {
                permit = self.concurrency.acquire() => {
                    permit.expect("runtime concurrency semaphore remains open")
                }
                () = cancellation.cancelled() => break Err(InvocationError::Cancelled),
            };
            attempts += 1;
            let attempt_id = Uuid::new_v4();
            attempt_ids.push(attempt_id);
            let mut container_invocation = base_invocation.clone();
            container_invocation.attempt_id = attempt_id;
            let execution = self
                .containers
                .run(container_invocation, cancellation.clone());
            tokio::pin!(execution);
            let attempt =
                match tokio::time::timeout(self.limits.attempt_timeout, &mut execution).await {
                    Ok(result) => Ok(result),
                    Err(_) => {
                        cancellation.cancel();
                        match tokio::time::timeout(self.limits.cleanup_timeout, execution).await {
                            Ok(Ok(_))
                            | Ok(Err(ContainerFailure::Retryable))
                            | Ok(Err(ContainerFailure::Rejected { .. })) => Err(()),
                            Ok(Err(ContainerFailure::CleanupFailed)) | Err(_) => {
                                Ok(Err(ContainerFailure::CleanupFailed))
                            }
                        }
                    }
                };
            drop(permit);
            match attempt {
                Ok(Ok(output)) => {
                    let outcome = (output.stdout.len() <= self.limits.max_output_bytes)
                        .then(|| serde_json::from_slice::<Value>(&output.stdout).ok())
                        .flatten();
                    let outcome = outcome.filter(|value| {
                        value.is_object()
                            && value.get("status").and_then(Value::as_str) == Some("completed")
                    });
                    match outcome {
                        Some(outcome)
                            if exceeds_cost_limit(&outcome, self.limits.max_cost_usd_micros) =>
                        {
                            break Err(InvocationError::CostLimitExceeded);
                        }
                        Some(outcome) => break Ok(outcome),
                        None if attempts < self.limits.max_attempts => continue,
                        None => break Err(InvocationError::InvalidOutcome),
                    }
                }
                Ok(Err(ContainerFailure::CleanupFailed)) => {
                    break Err(InvocationError::CleanupFailed);
                }
                Ok(Err(ContainerFailure::Rejected { code, message })) => {
                    break Err(InvocationError::AgentRejected { code, message });
                }
                Ok(Err(ContainerFailure::Retryable)) if cancellation.is_cancelled() => {
                    break Err(InvocationError::Cancelled);
                }
                Ok(Err(ContainerFailure::Retryable)) if attempts < self.limits.max_attempts => {
                    continue;
                }
                Ok(Err(ContainerFailure::Retryable)) => {
                    break Err(InvocationError::RuntimeFailed);
                }
                Err(()) => break Err(InvocationError::TimedOut),
            }
        };
        let cleanup_failed = matches!(result, Err(InvocationError::CleanupFailed));
        if !cleanup_failed {
            self.active_invocations.lock().await.remove(&invocation_id);
            self.active_identities
                .lock()
                .await
                .remove(&agent_identity_id);
        }
        let (status, last_failure) = match &result {
            Ok(_) => ("completed", None),
            Err(InvocationError::RuntimeFailed) => ("failed", Some("container_crash")),
            Err(InvocationError::InvalidOutcome) => ("failed", Some("invalid_outcome")),
            Err(InvocationError::TimedOut) => ("failed", Some("timeout")),
            Err(InvocationError::Cancelled) => ("cancelled", Some("cancelled")),
            Err(InvocationError::CostLimitExceeded) => ("failed", Some("cost_limit_exceeded")),
            Err(InvocationError::AgentRejected { .. }) => ("failed", Some("agent_rejected")),
            Err(InvocationError::CleanupFailed) => {
                ("cleanup_failed", Some("container_cleanup_failed"))
            }
            Err(_) => ("failed", None),
        };
        let mut diagnostics = self.diagnostics.lock().await;
        if diagnostics.len() >= self.limits.max_diagnostic_entries {
            diagnostics.remove(0);
        }
        diagnostics.push(InvocationDiagnostic {
            invocation_id,
            workspace_id,
            agent_identity_id,
            fencing_token,
            status,
            attempts,
            containers_removed: attempts - usize::from(cleanup_failed),
            attempt_ids,
            last_failure,
        });
        result
    }

    #[allow(dead_code)]
    pub(crate) async fn steer(
        &self,
        invocation_id: Uuid,
        message: Value,
    ) -> Result<(), InvocationError> {
        if !self
            .active_invocations
            .lock()
            .await
            .contains_key(&invocation_id)
        {
            return Err(InvocationError::InvocationNotActive);
        }
        self.containers
            .steer(invocation_id, message)
            .await
            .map_err(|_| InvocationError::RuntimeFailed)
    }

    pub(crate) async fn cancel(&self, invocation_id: Uuid) -> bool {
        let cancellation = self
            .active_invocations
            .lock()
            .await
            .get(&invocation_id)
            .map(|active| active.cancellation.clone());
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            false
        }
    }

    pub(crate) async fn active_count(&self) -> usize {
        self.active_identities.lock().await.len()
    }

    pub(crate) async fn diagnostics(&self) -> RuntimeDiagnostics {
        RuntimeDiagnostics {
            active_count: self.active_count().await,
            invocations: self.diagnostics.lock().await.clone(),
            infrastructure: self.containers.infrastructure_health().await,
        }
    }
}

#[derive(Clone, Debug, Error)]
pub(crate) enum InvocationError {
    #[error("invalid invocation request")]
    InvalidRequest,
    #[error("durable identity already has an active invocation")]
    IdentityAlreadyActive,
    #[error("agent container failed")]
    RuntimeFailed,
    #[error("agent rejected the invocation ({code}): {message}")]
    AgentRejected { code: String, message: String },
    #[error("agent returned an invalid outcome")]
    InvalidOutcome,
    #[error("agent invocation timed out")]
    TimedOut,
    #[error("agent invocation was cancelled")]
    Cancelled,
    #[error("invocation is not active")]
    InvocationNotActive,
    #[error("agent reported cost over the configured limit")]
    CostLimitExceeded,
    #[error("container cleanup could not be confirmed")]
    CleanupFailed,
}

fn exceeds_cost_limit(outcome: &Value, limit: Option<u64>) -> bool {
    let Some(limit) = limit else {
        return false;
    };
    outcome
        .get("usage")
        .and_then(|usage| usage.get("costUsdMicros"))
        .and_then(Value::as_u64)
        .is_some_and(|cost| cost > limit)
}
