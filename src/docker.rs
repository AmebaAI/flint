use std::{
    collections::{HashMap, HashSet},
    env,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use bollard::{
    Docker,
    container::LogOutput,
    exec::{CreateExecOptions, StartExecOptions, StartExecResults},
    models::{
        ContainerCreateBody, HostConfig, Mount, MountTypeEnum, MountVolumeOptions, PortBinding,
        VolumeCreateRequest,
    },
    query_parameters::{
        CreateContainerOptionsBuilder, EventsOptionsBuilder, ListContainersOptionsBuilder,
        ListImagesOptionsBuilder, RemoveContainerOptionsBuilder, StartContainerOptions,
    },
};
use futures_util::StreamExt;
use serde::Deserialize;
#[cfg(test)]
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[cfg(test)]
use bollard::query_parameters::{
    AttachContainerOptionsBuilder, LogsOptionsBuilder, WaitContainerOptionsBuilder,
};
#[cfg(test)]
use tokio::io::AsyncWriteExt;

#[cfg(test)]
use crate::runtime::{
    ContainerFailure, ContainerInvocation, ContainerOutput, ContainerRuntime,
    RuntimeInfrastructureHealth, RuntimeLimits,
};
use crate::{
    catalog::{
        Connectivity, ConnectivityMode, DiscoveredRuntimeImage, DiscoveryPolicy,
        RUNTIME_PROTOCOL_LABEL, ResolvedImage, ResolvedRuntime, RuntimeCatalog, RuntimeRegistry,
        parse_runtime_descriptor, runtime_name_from_image,
    },
    config::DockerDiscoveryConfig,
    session::{
        CommandEvent, CommandExecution, SessionBackend, SessionContainer, SessionHealth, SessionKey,
    },
};

const MANAGED_LABEL: &str = "agentcore.emulator.managed";
const OWNER_LABEL: &str = "agentcore.emulator.owner";
const RUNTIME_LABEL: &str = "agentcore.emulator.runtime-arn";
const QUALIFIER_LABEL: &str = "agentcore.emulator.qualifier";
#[cfg(test)]
const INVOCATION_LABEL: &str = "agentcore.emulator.invocation-id";
#[cfg(test)]
const IDENTITY_LABEL: &str = "agentcore.emulator.agent-identity-id";
#[cfg(test)]
const ATTEMPT_LABEL: &str = "agentcore.emulator.attempt-id";
const SESSION_LABEL: &str = "agentcore.emulator.runtime-session-id";
const CATALOG_GENERATION_LABEL: &str = "agentcore.emulator.catalog-generation";
const IMAGE_LABEL: &str = "agentcore.emulator.image";
const IMAGE_ID_LABEL: &str = "agentcore.emulator.image-id";
const CREATED_AT_LABEL: &str = "agentcore.emulator.created-at-unix-seconds";

struct CreatedContainerGuard {
    docker: Docker,
    id: String,
    armed: bool,
}

impl Drop for CreatedContainerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let docker = self.docker.clone();
        let id = self.id.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for attempt in 1..=3 {
                    match docker
                        .remove_container(
                            &id,
                            Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                        )
                        .await
                    {
                        Ok(())
                        | Err(bollard::errors::Error::DockerResponseServerError {
                            status_code: 404,
                            ..
                        }) => return,
                        Err(_) if attempt < 3 => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Err(error) => {
                            warn!(container_id = %id, %error, "failed to clean interrupted session startup");
                        }
                    }
                }
            });
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
enum DockerAttemptError {
    Execution(bollard::errors::Error),
    Rejected { code: String, message: String },
    Cleanup(bollard::errors::Error),
}

#[cfg(test)]
#[derive(Deserialize)]
struct AgentFailure {
    code: String,
    message: String,
}

#[cfg(test)]
impl From<bollard::errors::Error> for DockerAttemptError {
    fn from(error: bollard::errors::Error) -> Self {
        Self::Execution(error)
    }
}

#[cfg(test)]
impl From<std::io::Error> for DockerAttemptError {
    fn from(error: std::io::Error) -> Self {
        Self::Execution(bollard::errors::Error::DockerStreamError {
            error: format!("container input stream failed: {error}"),
        })
    }
}

#[cfg(test)]
pub(crate) struct DockerContainerRuntime {
    docker: Docker,
    runtime_owner: String,
    deployment: Arc<ResolvedRuntime>,
    limits: RuntimeLimits,
    steering: Arc<Mutex<HashMap<uuid::Uuid, mpsc::Sender<Value>>>>,
}

#[cfg(test)]
impl DockerContainerRuntime {
    async fn remove(&self, container: &str) -> Result<(), bollard::errors::Error> {
        for attempt in 1..=3 {
            match self
                .docker
                .remove_container(
                    container,
                    Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(()),
                Err(error) if attempt == 3 => return Err(error),
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
        unreachable!("bounded cleanup loop returns on its final attempt")
    }

    async fn execute(
        &self,
        invocation: &ContainerInvocation,
        cancellation: CancellationToken,
    ) -> Result<Vec<u8>, DockerAttemptError> {
        let container_name = container_name(invocation);
        let mut labels = HashMap::new();
        labels.insert(MANAGED_LABEL.to_owned(), "true".to_owned());
        labels.insert(OWNER_LABEL.to_owned(), self.runtime_owner.clone());
        labels.insert(
            RUNTIME_LABEL.to_owned(),
            self.deployment.runtime_arn.clone(),
        );
        labels.insert(
            QUALIFIER_LABEL.to_owned(),
            self.deployment.qualifier.clone(),
        );
        labels.insert(
            INVOCATION_LABEL.to_owned(),
            invocation.invocation_id.to_string(),
        );
        labels.insert(
            IDENTITY_LABEL.to_owned(),
            invocation.agent_identity_id.to_string(),
        );
        labels.insert(ATTEMPT_LABEL.to_owned(), invocation.attempt_id.to_string());
        let mut environment = self.deployment.container_environment();
        environment.extend([
            format!("FLINT_INVOCATION_ID={}", invocation.invocation_id),
            format!("FLINT_AGENT_IDENTITY_ID={}", invocation.agent_identity_id),
            format!("FLINT_FENCING_TOKEN={}", invocation.fencing_token),
        ]);
        if let Some(max_cost) = invocation.max_cost_usd_micros {
            environment.push(format!("FLINT_MAX_COST_USD_MICROS={max_cost}"));
        }
        let create_body = ContainerCreateBody {
            image: Some(self.deployment.image.clone()),
            env: Some(environment),
            labels: Some(labels),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            open_stdin: Some(true),
            stdin_once: Some(true),
            host_config: Some(agent_host_config(&self.deployment)),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&container_name)
                        .build(),
                ),
                create_body,
            )
            .await?;
        let container_id = created.id;
        let execution = async {
            let bollard::container::AttachContainerResults {
                mut output,
                mut input,
            } = self
                .docker
                .attach_container(
                    &container_id,
                    Some(
                        AttachContainerOptionsBuilder::default()
                            .stdin(true)
                            .stdout(true)
                            .stderr(true)
                            .stream(true)
                            .build(),
                    ),
                )
                .await?;
            self.docker
                .start_container(&container_id, None::<StartContainerOptions>)
                .await?;
            let input_payload = serde_json::to_vec(&json!({
                "schemaVersion": 1,
                "invocationId": invocation.invocation_id,
                "attemptId": invocation.attempt_id,
                "agentIdentityId": invocation.agent_identity_id,
                "fencingToken": invocation.fencing_token,
                "credentials": {
                    "openaiApiKey": self.deployment.environment_value("OPENAI_API_KEY").unwrap_or_default(),
                    "backendAccessToken": invocation.backend_credential,
                },
                "input": invocation.input,
            }))
            .expect("JSON values serialize");
            input.write_all(&input_payload).await?;
            input.write_all(b"\n").await?;
            let (steering_sender, mut steering_receiver) = mpsc::channel(32);
            self.steering
                .lock()
                .await
                .insert(invocation.invocation_id, steering_sender);

            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            loop {
                tokio::select! {
                    message = steering_receiver.recv() => {
                        let Some(message) = message else { continue };
                        let command = serde_json::to_vec(&json!({
                            "type": "steer",
                            "invocationId": invocation.invocation_id,
                            "text": message.get("text").and_then(Value::as_str).unwrap_or_default(),
                            "timestamp": SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis(),
                        }))
                        .expect("JSON values serialize");
                        input.write_all(&command).await?;
                        input.write_all(b"\n").await?;
                    }
                    frame = output.next() => {
                        let Some(frame) = frame else { break };
                        match frame? {
                            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                                stdout.extend_from_slice(&message);
                                if stdout.len() > self.limits.max_output_bytes {
                                    return Err(DockerAttemptError::Execution(
                                        bollard::errors::Error::DockerStreamError {
                                            error: "agent output exceeded configured limit".to_owned(),
                                        },
                                    ));
                                }
                            }
                            LogOutput::StdErr { message } => {
                                stderr.extend_from_slice(&message);
                                if stderr.len() > self.limits.max_output_bytes {
                                    return Err(DockerAttemptError::Execution(
                                        bollard::errors::Error::DockerStreamError {
                                            error: "agent error output exceeded configured limit"
                                                .to_owned(),
                                        },
                                    ));
                                }
                            }
                            LogOutput::StdIn { .. } => {}
                        }
                    }
                }
            }
            self.steering.lock().await.remove(&invocation.invocation_id);
            let _ = input.shutdown().await;
            let wait_options = WaitContainerOptionsBuilder::default()
                .condition("not-running")
                .build();
            let wait = self
                .docker
                .wait_container(&container_id, Some(wait_options))
                .next()
                .await
                .ok_or_else(|| bollard::errors::Error::DockerStreamError {
                    error: "Docker returned no container exit status".to_owned(),
                })?;
            let status_code = match wait {
                Ok(wait) => wait.status_code,
                Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => code,
                Err(error) => return Err(error.into()),
            };
            if status_code != 0 {
                stdout.clear();
                stderr.clear();
                let logs_options = LogsOptionsBuilder::default()
                    .stdout(true)
                    .stderr(true)
                    .build();
                let mut logs = self.docker.logs(&container_id, Some(logs_options));
                while let Some(frame) = logs.next().await {
                    match frame? {
                        LogOutput::StdOut { message } | LogOutput::Console { message } => {
                            stdout.extend_from_slice(&message);
                        }
                        LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                        LogOutput::StdIn { .. } => {}
                    }
                    if stdout.len() > self.limits.max_output_bytes
                        || stderr.len() > self.limits.max_output_bytes
                    {
                        return Err(DockerAttemptError::Execution(
                            bollard::errors::Error::DockerStreamError {
                                error: "agent failure output exceeded configured limit".to_owned(),
                            },
                        ));
                    }
                }
                if let Some(failure) = deterministic_agent_failure(&stderr)
                    .or_else(|| deterministic_agent_failure(&stdout))
                {
                    return Err(DockerAttemptError::Rejected {
                        code: failure.code,
                        message: failure.message,
                    });
                }
                return Err(DockerAttemptError::Execution(
                    bollard::errors::Error::DockerStreamError {
                        error: format!("agent container exited with status {status_code}"),
                    },
                ));
            }
            Ok(stdout)
        };
        tokio::pin!(execution);
        let result = tokio::select! {
            result = &mut execution => result,
            () = cancellation.cancelled() => Err(DockerAttemptError::Execution(
                bollard::errors::Error::DockerStreamError {
                    error: "agent invocation cancelled".to_owned(),
                },
            )),
        };
        let removal = self.remove(&container_id).await;
        match (result, removal) {
            (Ok(stdout), Ok(())) => Ok(stdout),
            (Err(error), Ok(())) => Err(error),
            (result, Err(cleanup_error)) => {
                if let Err(execution_error) = result {
                    warn!(
                        invocation_id = %invocation.invocation_id,
                        attempt_id = %invocation.attempt_id,
                        ?execution_error,
                        "agent attempt also failed before cleanup failed"
                    );
                }
                Err(DockerAttemptError::Cleanup(cleanup_error))
            }
        }
    }
}

#[cfg(test)]
#[async_trait]
impl ContainerRuntime for DockerContainerRuntime {
    async fn run(
        &self,
        invocation: ContainerInvocation,
        cancellation: CancellationToken,
    ) -> Result<ContainerOutput, ContainerFailure> {
        let result = self.execute(&invocation, cancellation).await;
        self.steering.lock().await.remove(&invocation.invocation_id);
        match result {
            Ok(stdout) => Ok(ContainerOutput { stdout }),
            Err(DockerAttemptError::Execution(error)) => {
                warn!(
                    invocation_id = %invocation.invocation_id,
                    attempt_id = %invocation.attempt_id,
                    %error,
                    "agent container attempt failed after cleanup"
                );
                Err(ContainerFailure::Retryable)
            }
            Err(DockerAttemptError::Rejected { code, message }) => {
                warn!(
                    invocation_id = %invocation.invocation_id,
                    attempt_id = %invocation.attempt_id,
                    %code,
                    "agent container rejected its configuration or invocation"
                );
                Err(ContainerFailure::Rejected { code, message })
            }
            Err(DockerAttemptError::Cleanup(error)) => {
                warn!(
                    invocation_id = %invocation.invocation_id,
                    attempt_id = %invocation.attempt_id,
                    %error,
                    "agent container cleanup could not be confirmed"
                );
                Err(ContainerFailure::CleanupFailed)
            }
        }
    }

    async fn steer(
        &self,
        invocation_id: uuid::Uuid,
        message: Value,
    ) -> Result<(), ContainerFailure> {
        let sender = self.steering.lock().await.get(&invocation_id).cloned();
        let Some(sender) = sender else {
            return Err(ContainerFailure::Retryable);
        };
        sender
            .send(message)
            .await
            .map_err(|_| ContainerFailure::Retryable)
    }

    async fn infrastructure_health(&self) -> RuntimeInfrastructureHealth {
        RuntimeInfrastructureHealth {
            docker_available: self.docker.ping().await.is_ok(),
            open_ai_configured: self
                .deployment
                .environment_value("OPENAI_API_KEY")
                .is_some_and(|value| !value.trim().is_empty()),
            agent_image_available: self
                .docker
                .inspect_image(&self.deployment.image)
                .await
                .is_ok(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DockerBackendError {
    #[error("Docker runtime is unavailable: {0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("Docker runtime discovery failed: {0}")]
    Discovery(String),
    #[error("Docker startup preflight failed: {0}")]
    Preflight(String),
    #[error("Docker session reconciliation failed: {0}")]
    Reconciliation(String),
}

#[derive(Debug)]
enum AdoptionError {
    Invalid(String),
    Transient(String),
}

async fn resolve_catalog_image_ids(
    docker: &Docker,
    catalog: &RuntimeCatalog,
) -> Result<RuntimeCatalog, DockerBackendError> {
    if catalog
        .snapshots()
        .all(|deployment| !deployment.image_id.is_empty())
    {
        return Ok(catalog.clone());
    }
    let mut image_ids = HashMap::new();
    for deployment in catalog.snapshots() {
        if image_ids.contains_key(&deployment.image) {
            continue;
        }
        let inspection = docker
            .inspect_image(&deployment.image)
            .await
            .map_err(|error| {
                DockerBackendError::Preflight(format!(
                    "runtime {} qualifier {} image {} is not present in the Docker daemon; build or load it before starting Flint: {error}",
                    deployment.runtime_arn, deployment.qualifier, deployment.image
                ))
            })?;
        let image_id =
            immutable_image_reference(&inspection, &deployment.image).map_err(|message| {
                DockerBackendError::Preflight(format!(
                    "runtime {} qualifier {} image {} {message}",
                    deployment.runtime_arn, deployment.qualifier, deployment.image
                ))
            })?;
        let platform = image_platform(&inspection, &deployment.image)
            .map_err(DockerBackendError::Preflight)?;
        let image_config = inspection.config.as_ref();
        image_ids.insert(
            deployment.image.clone(),
            ResolvedImage {
                immutable_reference: image_id,
                platform,
                entrypoint: image_config.and_then(|config| config.entrypoint.clone()),
                command: image_config.and_then(|config| config.cmd.clone()),
                environment: image_config
                    .and_then(|config| config.env.clone())
                    .unwrap_or_default(),
                working_directory: image_config
                    .and_then(|config| config.working_dir.clone())
                    .filter(|directory| !directory.is_empty()),
            },
        );
    }
    catalog
        .with_resolved_image_ids(&image_ids)
        .map_err(|error| DockerBackendError::Preflight(error.to_string()))
}

async fn discover_runtime_catalog(
    docker: &Docker,
    config: &DockerDiscoveryConfig,
) -> Result<RuntimeCatalog, DockerBackendError> {
    let mut candidates = HashMap::<String, DiscoveredRuntimeImage>::new();
    if config.image_allowlist.is_empty() {
        let images = docker
            .list_images(Some(ListImagesOptionsBuilder::default().all(true).build()))
            .await?;
        for image in images {
            if !image.labels.contains_key(RUNTIME_PROTOCOL_LABEL) {
                continue;
            }
            let inspection = docker.inspect_image(&image.id).await.map_err(|error| {
                DockerBackendError::Discovery(format!(
                    "could not inspect marked image {}: {error}",
                    image.id
                ))
            })?;
            let mut references = inspection.repo_tags.clone().unwrap_or_default();
            references.extend(inspection.repo_digests.clone().unwrap_or_default());
            references.sort_unstable();
            let Some(reference) = references.into_iter().next() else {
                continue;
            };
            insert_discovered_image(&mut candidates, inspection, reference)?;
        }
    } else {
        for reference in &config.image_allowlist {
            let inspection = docker.inspect_image(reference).await.map_err(|error| {
                DockerBackendError::Discovery(format!(
                    "allowlisted image {reference} is not present in the Docker daemon: {error}"
                ))
            })?;
            insert_discovered_image(&mut candidates, inspection, reference.clone())?;
        }
    }
    let policy = DiscoveryPolicy {
        connectivity: Connectivity {
            mode: config.connectivity_mode,
            docker_network: config.docker_network.clone(),
            add_host_gateway: false,
        },
        environment_allowlist: config.environment_allowlist.clone(),
        header_allowlist: config.header_allowlist.clone(),
    };
    RuntimeCatalog::from_discovered_images(candidates.into_values().collect(), &policy, |name| {
        env::var(name).ok()
    })
    .map_err(|error| DockerBackendError::Discovery(error.to_string()))
}

fn insert_discovered_image(
    candidates: &mut HashMap<String, DiscoveredRuntimeImage>,
    inspection: bollard::models::ImageInspect,
    image_reference: String,
) -> Result<(), DockerBackendError> {
    let content_id = inspection
        .id
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DockerBackendError::Discovery(format!(
                "marked image {image_reference} has no immutable image ID"
            ))
        })?;
    let image_id = immutable_image_reference(&inspection, &image_reference)
        .map_err(DockerBackendError::Discovery)?;
    let image_platform =
        image_platform(&inspection, &image_reference).map_err(DockerBackendError::Discovery)?;
    let image_config = inspection.config.as_ref();
    let image_entrypoint = image_config.and_then(|config| config.entrypoint.clone());
    let image_command = image_config.and_then(|config| config.cmd.clone());
    let image_environment = image_config
        .and_then(|config| config.env.clone())
        .unwrap_or_default();
    let image_working_directory = image_config
        .and_then(|config| config.working_dir.clone())
        .filter(|directory| !directory.is_empty());
    let labels = inspection
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .ok_or_else(|| {
            DockerBackendError::Discovery(format!(
                "selected image {image_reference} has no runtime labels"
            ))
        })?;
    let default_name = runtime_name_from_image(&image_reference);
    let descriptor = parse_runtime_descriptor(labels, &default_name).map_err(|error| {
        DockerBackendError::Discovery(format!(
            "image {image_reference} has invalid runtime labels: {error}"
        ))
    })?;
    match candidates.entry(content_id) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(DiscoveredRuntimeImage {
                image_id,
                image_platform,
                image_entrypoint,
                image_command,
                image_environment,
                image_working_directory,
                image_reference,
                descriptor,
            });
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if image_reference < entry.get().image_reference {
                entry.get_mut().image_reference = image_reference;
            }
        }
    }
    Ok(())
}

fn image_platform(
    inspection: &bollard::models::ImageInspect,
    image_reference: &str,
) -> Result<String, String> {
    match (
        inspection.os.as_deref(),
        inspection.architecture.as_deref(),
        inspection.variant.as_deref(),
    ) {
        (Some(os), Some(architecture), Some(variant)) if !variant.is_empty() => {
            Ok(format!("{os}/{architecture}/{variant}"))
        }
        (Some(os), Some(architecture), _) => Ok(format!("{os}/{architecture}")),
        _ => Err(format!("image {image_reference} has no platform metadata")),
    }
}

fn immutable_image_reference(
    inspection: &bollard::models::ImageInspect,
    image_reference: &str,
) -> Result<String, String> {
    let image_id = inspection
        .id
        .clone()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("image {image_reference} has no immutable image ID"))?;
    let mut repo_digests = inspection.repo_digests.clone().unwrap_or_default();
    repo_digests.sort_unstable();
    Ok(repo_digests.into_iter().next().unwrap_or(image_id))
}

fn spawn_discovery_task(
    docker: Docker,
    catalog: RuntimeRegistry,
    config: DockerDiscoveryConfig,
) -> DiscoveryTask {
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    tokio::spawn(async move {
        run_discovery_task(docker, catalog, config, task_cancellation).await;
    });
    DiscoveryTask { cancellation }
}

async fn run_discovery_task(
    docker: Docker,
    catalog: RuntimeRegistry,
    config: DockerDiscoveryConfig,
    cancellation: CancellationToken,
) {
    let mut interval = tokio::time::interval(config.refresh_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;

    loop {
        let filters = HashMap::from([("type", vec!["image"])]);
        let events = docker.events(Some(
            EventsOptionsBuilder::default().filters(&filters).build(),
        ));
        tokio::pin!(events);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    refresh_discovered_catalog(&docker, &catalog, &config).await;
                }
                event = events.next() => match event {
                    Some(Ok(_)) => {
                        tokio::select! {
                            () = cancellation.cancelled() => return,
                            () = tokio::time::sleep(Duration::from_millis(250)) => {}
                        }
                        refresh_discovered_catalog(&docker, &catalog, &config).await;
                    }
                    Some(Err(error)) => {
                        let message = format!("Docker image event stream failed: {error}");
                        warn!(%error, "Docker runtime discovery event stream failed");
                        catalog.mark_refresh_failure(message);
                        break;
                    }
                    None => {
                        catalog.mark_refresh_failure("Docker image event stream ended");
                        break;
                    }
                }
            }
        }
        tokio::select! {
            () = cancellation.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

async fn refresh_discovered_catalog(
    docker: &Docker,
    catalog: &RuntimeRegistry,
    config: &DockerDiscoveryConfig,
) {
    let result = match discover_runtime_catalog(docker, config).await {
        Ok(discovered) => DockerSessionBackend::preflight_catalog(docker, &discovered)
            .await
            .map(|()| discovered),
        Err(error) => Err(error),
    };
    match result {
        Ok(discovered) => catalog.replace(discovered),
        Err(error) => {
            warn!(%error, "Docker runtime discovery refresh is degraded");
            catalog.mark_refresh_failure(error.to_string());
        }
    }
}

struct DiscoveryTask {
    cancellation: CancellationToken,
}

impl Drop for DiscoveryTask {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct DockerSessionBackend {
    docker: Docker,
    runtime_owner: String,
    catalog: RuntimeRegistry,
    client: reqwest::Client,
    adoptable: Arc<Mutex<HashMap<SessionKey, SessionContainer>>>,
    deployments: Arc<Mutex<HashMap<String, Arc<ResolvedRuntime>>>>,
    command_limits: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    session_storage_mount_path: String,
    discovery_task: Option<Arc<DiscoveryTask>>,
}

impl DockerSessionBackend {
    #[cfg(test)]
    pub(crate) async fn connect(
        runtime_owner: String,
        catalog: RuntimeCatalog,
    ) -> Result<Self, DockerBackendError> {
        Self::connect_with_registry(
            runtime_owner,
            RuntimeRegistry::new(catalog),
            None,
            "/workspace".to_owned(),
        )
        .await
    }

    pub(crate) async fn connect_with_registry(
        runtime_owner: String,
        catalog: RuntimeRegistry,
        discovery: Option<DockerDiscoveryConfig>,
        session_storage_mount_path: String,
    ) -> Result<Self, DockerBackendError> {
        let docker = Docker::connect_with_local_defaults()?;
        docker.ping().await?;
        match discovery.as_ref() {
            Some(discovery) => match discover_runtime_catalog(&docker, discovery).await {
                Ok(resolved) => {
                    Self::preflight_catalog(&docker, &resolved).await?;
                    catalog.replace(resolved);
                }
                Err(error) => {
                    warn!(%error, "initial Docker runtime discovery failed");
                    return Err(error);
                }
            },
            None => {
                let resolved = resolve_catalog_image_ids(&docker, &catalog.snapshot()).await?;
                catalog.replace(resolved);
            }
        }
        let mut backend = Self {
            docker,
            runtime_owner,
            catalog,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(1))
                .timeout(Duration::from_secs(2))
                .build()
                .expect("static HTTP client configuration is valid"),
            adoptable: Arc::new(Mutex::new(HashMap::new())),
            deployments: Arc::new(Mutex::new(HashMap::new())),
            command_limits: Arc::new(Mutex::new(HashMap::new())),
            session_storage_mount_path,
            discovery_task: None,
        };
        backend.preflight().await?;
        backend.reconcile().await?;
        if let Some(discovery) = discovery {
            backend.discovery_task = Some(Arc::new(spawn_discovery_task(
                backend.docker.clone(),
                backend.catalog.clone(),
                discovery,
            )));
        }
        Ok(backend)
    }

    async fn preflight(&self) -> Result<(), DockerBackendError> {
        Self::preflight_catalog(&self.docker, &self.catalog.snapshot()).await
    }

    async fn preflight_catalog(
        docker: &Docker,
        catalog: &RuntimeCatalog,
    ) -> Result<(), DockerBackendError> {
        let mut deployments = catalog.snapshots().collect::<Vec<_>>();
        deployments.sort_by(|left, right| {
            (&left.runtime_arn, &left.qualifier).cmp(&(&right.runtime_arn, &right.qualifier))
        });

        let mut images = HashSet::new();
        for deployment in &deployments {
            if !images.insert(deployment.image_id.clone()) {
                continue;
            }
            let inspection = docker
                .inspect_image(&deployment.image)
                .await
                .map_err(|error| {
                    DockerBackendError::Preflight(format!(
                        "runtime {} qualifier {} image {} ({}) is not present in the Docker daemon; build or load it before starting Flint: {error}",
                        deployment.runtime_arn,
                        deployment.qualifier,
                        deployment.image,
                        deployment.image_id
                    ))
                })?;
            let immutable = immutable_image_reference(&inspection, &deployment.image)
                .map_err(DockerBackendError::Preflight)?;
            if immutable != deployment.image_id {
                return Err(DockerBackendError::Preflight(format!(
                    "runtime {} qualifier {} image {} changed after discovery",
                    deployment.runtime_arn, deployment.qualifier, deployment.image
                )));
            }
        }

        let mut networks = HashMap::new();
        for deployment in &deployments {
            let Some(network) = deployment.connectivity.docker_network.as_deref() else {
                continue;
            };
            networks
                .entry(network.to_owned())
                .or_insert_with(|| Arc::clone(deployment));
        }
        for (network, deployment) in &networks {
            let inspection = docker
                .inspect_network(network, None)
                .await
                .map_err(|error| {
                    DockerBackendError::Preflight(format!(
                        "runtime {} qualifier {} Docker network {network} is unavailable: {error}",
                        deployment.runtime_arn, deployment.qualifier
                    ))
                })?;
            if inspection.driver.as_deref() != Some("bridge")
                || inspection.scope.as_deref() != Some("local")
                || inspection.ingress.unwrap_or(false)
                || inspection.config_only.unwrap_or(false)
            {
                return Err(DockerBackendError::Preflight(format!(
                    "runtime {} qualifier {} Docker network {network} must be a local, non-ingress bridge network",
                    deployment.runtime_arn, deployment.qualifier
                )));
            }
        }

        if !networks.is_empty() {
            let container_id = env::var("HOSTNAME")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    DockerBackendError::Preflight(
                        "container connectivity requires Flint to run in a Docker container with a resolvable HOSTNAME"
                            .to_owned(),
                    )
                })?;
            let inspection = docker
                .inspect_container(&container_id, None)
                .await
                .map_err(|error| {
                    DockerBackendError::Preflight(format!(
                        "container connectivity could not resolve the Flint container from HOSTNAME {container_id}: {error}"
                    ))
                })?;
            let attached_networks = inspection
                .network_settings
                .and_then(|settings| settings.networks)
                .unwrap_or_default();
            for (network, deployment) in &networks {
                if !attached_networks.contains_key(network) {
                    return Err(DockerBackendError::Preflight(format!(
                        "runtime {} qualifier {} requires Docker network {network}, but Flint container {container_id} is not attached to it",
                        deployment.runtime_arn, deployment.qualifier
                    )));
                }
            }
        }
        Ok(())
    }

    async fn reconcile(&self) -> Result<(), DockerBackendError> {
        let filters = HashMap::from([("label".to_owned(), vec![format!("{MANAGED_LABEL}=true")])]);
        let options = ListContainersOptionsBuilder::default()
            .all(true)
            .filters(&filters)
            .build();
        for summary in self.docker.list_containers(Some(options)).await? {
            let Some(id) = summary.id else { continue };
            let Some(labels) = summary.labels.as_ref() else {
                continue;
            };
            if !is_owned_reconciliation_candidate(labels, &self.runtime_owner) {
                continue;
            }
            match self.adopt(&id).await {
                Ok(()) => {}
                Err(AdoptionError::Invalid(reason)) => {
                    warn!(container_id = %id, %reason, "removing invalid owned session container");
                    self.remove(&id).await?;
                }
                Err(AdoptionError::Transient(reason)) => {
                    return Err(DockerBackendError::Reconciliation(format!(
                        "could not safely adopt owned container {id}; it was left untouched: {reason}"
                    )));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn take_adopted_sessions(
        &self,
    ) -> Vec<(SessionKey, Arc<ResolvedRuntime>, SessionContainer)> {
        let adopted = std::mem::take(&mut *self.adoptable.lock().await);
        let deployments = self.deployments.lock().await;
        adopted
            .into_iter()
            .filter_map(|(key, container)| {
                deployments
                    .get(&container.id)
                    .cloned()
                    .map(|deployment| (key, deployment, container))
            })
            .collect()
    }

    async fn adopt(&self, id: &str) -> Result<(), AdoptionError> {
        let inspection = self
            .docker
            .inspect_container(id, None)
            .await
            .map_err(|error| AdoptionError::Transient(error.to_string()))?;
        if !inspection
            .state
            .as_ref()
            .and_then(|state| state.running)
            .unwrap_or(false)
        {
            return Err(AdoptionError::Invalid(
                "container is not running".to_owned(),
            ));
        }
        let labels = inspection
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .ok_or_else(|| AdoptionError::Invalid("container has no labels".to_owned()))?;
        let runtime_arn = labels
            .get(RUNTIME_LABEL)
            .ok_or_else(|| AdoptionError::Invalid("container has no runtime ARN".to_owned()))?;
        let qualifier = labels
            .get(QUALIFIER_LABEL)
            .ok_or_else(|| AdoptionError::Invalid("container has no qualifier".to_owned()))?;
        let runtime_session_id = labels.get(SESSION_LABEL).ok_or_else(|| {
            AdoptionError::Invalid("container has no runtime session ID".to_owned())
        })?;
        let deployment = self
            .catalog
            .resolve_stored(runtime_arn, Some(qualifier))
            .map_err(|error| AdoptionError::Invalid(error.to_string()))?;
        if labels.get(CATALOG_GENERATION_LABEL) != Some(&deployment.catalog_generation) {
            return Err(AdoptionError::Invalid(
                "container belongs to another catalog generation".to_owned(),
            ));
        }
        if labels.get(IMAGE_LABEL) != Some(&deployment.image) {
            return Err(AdoptionError::Invalid(
                "container image does not match the catalog".to_owned(),
            ));
        }
        if labels.get(IMAGE_ID_LABEL) != Some(&deployment.image_id) {
            return Err(AdoptionError::Invalid(
                "container uses a different immutable image ID".to_owned(),
            ));
        }
        let endpoint =
            container_endpoint(&inspection, &deployment).map_err(AdoptionError::Invalid)?;
        let created_at = labels
            .get(CREATED_AT_LABEL)
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                AdoptionError::Invalid("container has no valid creation timestamp".to_owned())
            })?;
        let age = SystemTime::now()
            .duration_since(UNIX_EPOCH + Duration::from_secs(created_at))
            .unwrap_or_default();
        let container = SessionContainer {
            id: id.to_owned(),
            endpoint,
            age,
        };
        let mut health = SessionHealth::Unhealthy;
        for attempt in 1..=3 {
            health = self.ping_endpoint(&container, &deployment).await;
            if health != SessionHealth::Unhealthy {
                break;
            }
            if attempt < 3 {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        if health == SessionHealth::Unhealthy {
            return Err(AdoptionError::Invalid(
                "container ping remained unhealthy during reconciliation".to_owned(),
            ));
        }
        self.deployments
            .lock()
            .await
            .insert(id.to_owned(), deployment);
        self.adoptable.lock().await.insert(
            SessionKey {
                runtime_arn: runtime_arn.clone(),
                qualifier: qualifier.clone(),
                runtime_session_id: runtime_session_id.clone(),
            },
            container,
        );
        Ok(())
    }

    async fn remove_failed_start(&self, key: &SessionKey) -> Result<(), String> {
        let name = session_container_name(&self.runtime_owner, key);
        let inspection = match self.docker.inspect_container(&name, None).await {
            Ok(inspection) => inspection,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => return Ok(()),
            Err(error) => return Err(format!("inspect prior runtime container: {error}")),
        };
        let labels = inspection
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .ok_or_else(|| {
                "runtime session name is occupied by an unmanaged container".to_owned()
            })?;
        let owned = labels
            .get(MANAGED_LABEL)
            .is_some_and(|value| value == "true")
            && labels.get(OWNER_LABEL) == Some(&self.runtime_owner)
            && labels.get(RUNTIME_LABEL) == Some(&key.runtime_arn)
            && labels.get(QUALIFIER_LABEL) == Some(&key.qualifier)
            && labels.get(SESSION_LABEL) == Some(&key.runtime_session_id);
        if !owned {
            return Err("runtime session name is occupied by another owner".to_owned());
        }
        let id = inspection
            .id
            .ok_or_else(|| "prior runtime container has no ID".to_owned())?;
        self.remove(&id)
            .await
            .map_err(|error| format!("clean prior runtime container: {error}"))
    }

    async fn ensure_session_volume(&self, key: &SessionKey) -> Result<String, String> {
        let name = session_volume_name(&self.runtime_owner, key);
        let labels = HashMap::from([
            (MANAGED_LABEL.to_owned(), "true".to_owned()),
            (OWNER_LABEL.to_owned(), self.runtime_owner.clone()),
            (RUNTIME_LABEL.to_owned(), key.runtime_arn.clone()),
            (QUALIFIER_LABEL.to_owned(), key.qualifier.clone()),
            (SESSION_LABEL.to_owned(), key.runtime_session_id.clone()),
        ]);
        let volume = match self.docker.inspect_volume(&name).await {
            Ok(volume) => volume,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => self
                .docker
                .create_volume(VolumeCreateRequest {
                    name: Some(name.clone()),
                    labels: Some(labels.clone()),
                    ..Default::default()
                })
                .await
                .map_err(|error| format!("create runtime session volume: {error}"))?,
            Err(error) => return Err(format!("inspect runtime session volume: {error}")),
        };
        let owned = labels
            .iter()
            .all(|(label, value)| volume.labels.get(label) == Some(value));
        if !owned {
            return Err("runtime session volume name is occupied by another owner".to_owned());
        }
        Ok(name)
    }

    async fn create(
        &self,
        key: &SessionKey,
        deployment: Arc<ResolvedRuntime>,
        cancellation: CancellationToken,
    ) -> Result<SessionContainer, String> {
        let name = session_container_name(&self.runtime_owner, key);
        let volume_name = self.ensure_session_volume(key).await?;
        let image_id = deployment.image_id.clone();
        let image_platform = deployment.image_platform.clone();
        let catalog_generation = deployment.catalog_generation.clone();
        let mut labels = HashMap::from([
            (MANAGED_LABEL.to_owned(), "true".to_owned()),
            (OWNER_LABEL.to_owned(), self.runtime_owner.clone()),
            (RUNTIME_LABEL.to_owned(), key.runtime_arn.clone()),
            (QUALIFIER_LABEL.to_owned(), key.qualifier.clone()),
            (SESSION_LABEL.to_owned(), key.runtime_session_id.clone()),
            (CATALOG_GENERATION_LABEL.to_owned(), catalog_generation),
            (IMAGE_LABEL.to_owned(), deployment.image.clone()),
            (IMAGE_ID_LABEL.to_owned(), image_id),
            (
                CREATED_AT_LABEL.to_owned(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .to_string(),
            ),
        ]);
        labels.shrink_to_fit();
        for warning in &deployment.environment_warnings {
            tracing::warn!(
                runtime = %deployment.runtime_id,
                image = %deployment.image,
                "{warning}"
            );
        }
        let port = format!("{}/tcp", deployment.protocol.port());
        let host_config =
            session_host_config(&deployment, &volume_name, &self.session_storage_mount_path);
        let create_body = ContainerCreateBody {
            image: Some(deployment.image_id.clone()),
            user: Some("10001:10001".to_owned()),
            env: Some(deployment.container_environment()),
            entrypoint: deployment.image_entrypoint.clone(),
            cmd: deployment.image_command.clone(),
            working_dir: deployment.image_working_directory.clone(),
            labels: Some(labels),
            exposed_ports: Some(vec![port]),
            attach_stdin: Some(false),
            attach_stdout: Some(false),
            attach_stderr: Some(false),
            open_stdin: Some(false),
            host_config: Some(host_config),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(
                Some(
                    CreateContainerOptionsBuilder::default()
                        .name(&name)
                        .platform(&image_platform)
                        .build(),
                ),
                create_body,
            )
            .await
            .map_err(|error| format!("create runtime container: {error}"))?;
        let id = created.id;
        let mut created_guard = CreatedContainerGuard {
            docker: self.docker.clone(),
            id: id.clone(),
            armed: true,
        };
        let startup = async {
            self.docker
                .start_container(&id, None::<StartContainerOptions>)
                .await
                .map_err(|error| format!("start runtime container: {error}"))?;
            let inspection = self
                .docker
                .inspect_container(&id, None)
                .await
                .map_err(|error| format!("inspect runtime container: {error}"))?;
            let container = SessionContainer {
                id: id.clone(),
                endpoint: container_endpoint(&inspection, &deployment)?,
                age: Duration::ZERO,
            };
            self.deployments
                .lock()
                .await
                .insert(id.clone(), Arc::clone(&deployment));
            loop {
                if self.ping_endpoint(&container, &deployment).await != SessionHealth::Unhealthy {
                    return Ok(container);
                }
                let inspection = self
                    .docker
                    .inspect_container(&id, None)
                    .await
                    .map_err(|error| format!("inspect starting runtime container: {error}"))?;
                if !inspection
                    .state
                    .as_ref()
                    .and_then(|state| state.running)
                    .unwrap_or(false)
                {
                    return Err("runtime container exited before becoming ready".to_owned());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        let result = tokio::select! {
            result = tokio::time::timeout(
                Duration::from_secs(deployment.lifecycle.startup_timeout_seconds),
                startup,
            ) => result.map_err(|_| "runtime container readiness timed out".to_owned())?,
            () = cancellation.cancelled() => Err("runtime container startup was cancelled".to_owned()),
        };
        match result {
            Ok(container) => {
                created_guard.armed = false;
                Ok(container)
            }
            Err(startup_error) => match self.remove(&id).await {
                Ok(()) => {
                    created_guard.armed = false;
                    Err(startup_error)
                }
                Err(cleanup_error) => Err(format!(
                    "{startup_error}; runtime container cleanup also failed: {cleanup_error}"
                )),
            },
        }
    }

    async fn ping_endpoint(
        &self,
        container: &SessionContainer,
        deployment: &ResolvedRuntime,
    ) -> SessionHealth {
        let Some(ping_path) = deployment.protocol.ping_path() else {
            return tcp_endpoint_health(&container.endpoint).await;
        };
        http_endpoint_health(&self.client, &format!("{}{ping_path}", container.endpoint)).await
    }

    pub(crate) async fn execute_command(
        &self,
        container: &SessionContainer,
        command: String,
        timeout_seconds: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<CommandExecution, String> {
        let deployment = self
            .deployments
            .lock()
            .await
            .get(&container.id)
            .cloned()
            .ok_or_else(|| "runtime session container is not registered".to_owned())?;
        if !deployment.command.enabled {
            return Err("commands are disabled for this runtime".to_owned());
        }
        let semaphore = {
            let mut limits = self.command_limits.lock().await;
            limits
                .entry(container.id.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(deployment.command.max_concurrency)))
                .clone()
        };
        let permit = tokio::select! {
            permit = semaphore.acquire_owned() => permit
                .map_err(|_| "command concurrency gate is closed".to_owned())?,
            () = cancellation.cancelled() => {
                return Err("runtime command was cancelled while queued".to_owned());
            }
        };
        let pid_file = format!(
            "{}/.agentcore-command-{}.pid",
            self.session_storage_mount_path.trim_end_matches('/'),
            uuid::Uuid::new_v4()
        );
        let mut configured_command = deployment.command.shell.clone();
        configured_command.push(command);
        let mut command_line = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo $$ > \"$1\"; shift; exec setsid \"$@\"".to_owned(),
            "agentcore-command".to_owned(),
            pid_file.clone(),
        ];
        command_line.extend(configured_command);
        let create_exec = self.docker.create_exec(
            &container.id,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(command_line),
                user: Some("10001:10001".to_owned()),
                working_dir: Some(self.session_storage_mount_path.clone()),
                ..Default::default()
            },
        );
        let exec = tokio::select! {
            result = create_exec => result
                .map_err(|error| format!("create runtime command: {error}"))?,
            () = cancellation.cancelled() => {
                return Err("runtime command was cancelled before execution".to_owned());
            }
        };
        let docker = self.docker.clone();
        let container_id = container.id.clone();
        let timeout = Duration::from_secs(
            timeout_seconds
                .unwrap_or(deployment.command.timeout_seconds)
                .min(deployment.command.timeout_seconds),
        );
        let max_output_bytes = deployment.command.max_output_bytes;
        let (sender, events) = mpsc::channel(32);
        tokio::spawn(async move {
            let _permit = permit;
            let execution = async {
                let started = docker
                    .start_exec(
                        &exec.id,
                        Some(StartExecOptions {
                            output_capacity: Some(max_output_bytes.min(64 * 1024)),
                            ..Default::default()
                        }),
                    )
                    .await
                    .map_err(|error| format!("start runtime command: {error}"))?;
                let StartExecResults::Attached { mut output, .. } = started else {
                    return Err("runtime command unexpectedly started detached".to_owned());
                };
                let mut output_bytes = 0usize;
                while let Some(frame) = output.next().await {
                    let event =
                        match frame.map_err(|error| format!("read runtime command: {error}"))? {
                            LogOutput::StdOut { message } | LogOutput::Console { message } => {
                                output_bytes = output_bytes.saturating_add(message.len());
                                CommandEvent::Stdout(message.to_vec())
                            }
                            LogOutput::StdErr { message } => {
                                output_bytes = output_bytes.saturating_add(message.len());
                                CommandEvent::Stderr(message.to_vec())
                            }
                            LogOutput::StdIn { .. } => continue,
                        };
                    if output_bytes > max_output_bytes {
                        return Err(
                            "runtime command output exceeded its configured limit".to_owned()
                        );
                    }
                    sender
                        .send(Ok(event))
                        .await
                        .map_err(|_| "runtime command response was dropped".to_owned())?;
                }
                let inspection = docker
                    .inspect_exec(&exec.id)
                    .await
                    .map_err(|error| format!("inspect runtime command: {error}"))?;
                remove_command_pid_file(&docker, &container_id, &pid_file).await;
                Ok(inspection.exit_code.unwrap_or(-1))
            };
            enum Completion {
                Finished(Result<i64, String>),
                TimedOut,
                Cancelled,
            }
            let completion = tokio::select! {
                result = tokio::time::timeout(timeout, execution) => match result {
                    Ok(result) => Completion::Finished(result),
                    Err(_) => Completion::TimedOut,
                },
                () = cancellation.cancelled() => Completion::Cancelled,
            };
            match completion {
                Completion::Finished(Ok(exit_code)) => {
                    let _ = sender.send(Ok(CommandEvent::Exited(exit_code))).await;
                }
                Completion::Finished(Err(error)) => {
                    let termination =
                        terminate_exec(&docker, &container_id, &exec.id, &pid_file).await;
                    let message = termination
                        .err()
                        .map_or(error.clone(), |cleanup| format!("{error}; {cleanup}"));
                    let _ = sender.send(Err(message)).await;
                }
                Completion::TimedOut => {
                    let event = terminate_exec(&docker, &container_id, &exec.id, &pid_file)
                        .await
                        .map(|()| CommandEvent::TimedOut);
                    let _ = sender.send(event).await;
                }
                Completion::Cancelled => {
                    let event = terminate_exec(&docker, &container_id, &exec.id, &pid_file)
                        .await
                        .map(|()| CommandEvent::Cancelled);
                    let _ = sender.send(event).await;
                }
            }
        });
        Ok(CommandExecution { events })
    }

    async fn remove(&self, id: &str) -> Result<(), bollard::errors::Error> {
        for attempt in 1..=3 {
            match self
                .docker
                .remove_container(
                    id,
                    Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                )
                .await
            {
                Ok(())
                | Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {
                    self.deployments.lock().await.remove(id);
                    self.command_limits.lock().await.remove(id);
                    return Ok(());
                }
                Err(error) if attempt == 3 => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        unreachable!("bounded cleanup loop returns on its final attempt")
    }
}

#[async_trait]
impl SessionBackend for DockerSessionBackend {
    fn volume_name(&self, key: &SessionKey) -> String {
        session_volume_name(&self.runtime_owner, key)
    }

    async fn start(
        &self,
        key: &SessionKey,
        runtime: Arc<ResolvedRuntime>,
        cancellation: CancellationToken,
    ) -> Result<SessionContainer, String> {
        if let Some(container) = self.adoptable.lock().await.remove(key) {
            return Ok(container);
        }
        self.remove_failed_start(key).await?;
        self.create(key, runtime, cancellation).await
    }

    async fn ping(&self, container: &SessionContainer) -> SessionHealth {
        let deployment = self.deployments.lock().await.get(&container.id).cloned();
        let Some(deployment) = deployment else {
            return SessionHealth::Unhealthy;
        };
        self.ping_endpoint(container, &deployment).await
    }

    async fn stop(&self, container: &SessionContainer) -> Result<(), String> {
        self.remove(&container.id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn execute_command(
        &self,
        container: &SessionContainer,
        command: String,
        timeout_seconds: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<CommandExecution, String> {
        DockerSessionBackend::execute_command(
            self,
            container,
            command,
            timeout_seconds,
            cancellation,
        )
        .await
    }
}

async fn terminate_exec(
    docker: &Docker,
    container_id: &str,
    exec_id: &str,
    pid_file: &str,
) -> Result<(), String> {
    for signal in ["TERM", "KILL"] {
        let inspection = docker
            .inspect_exec(exec_id)
            .await
            .map_err(|error| format!("inspect cancelled runtime command: {error}"))?;
        if !inspection.running.unwrap_or(false) {
            remove_command_pid_file(docker, container_id, pid_file).await;
            return Ok(());
        }
        let script = format!("pid=$(cat \"$1\") && kill -{signal} -- \"-$pid\"",);
        run_detached_exec(
            docker,
            container_id,
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script,
                "agentcore-command-signal".to_owned(),
                pid_file.to_owned(),
            ],
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let inspection = docker
        .inspect_exec(exec_id)
        .await
        .map_err(|error| format!("verify cancelled runtime command: {error}"))?;
    remove_command_pid_file(docker, container_id, pid_file).await;
    if inspection.running.unwrap_or(false) {
        Err("runtime command process group could not be terminated".to_owned())
    } else {
        Ok(())
    }
}

async fn remove_command_pid_file(docker: &Docker, container_id: &str, pid_file: &str) {
    let _ = run_detached_exec(
        docker,
        container_id,
        vec!["rm".to_owned(), "-f".to_owned(), pid_file.to_owned()],
    )
    .await;
}

async fn run_detached_exec(
    docker: &Docker,
    container_id: &str,
    command: Vec<String>,
) -> Result<(), String> {
    let exec = docker
        .create_exec(
            container_id,
            CreateExecOptions {
                cmd: Some(command),
                user: Some("10001:10001".to_owned()),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| format!("create runtime command control process: {error}"))?;
    docker
        .start_exec(
            &exec.id,
            Some(StartExecOptions {
                detach: true,
                ..Default::default()
            }),
        )
        .await
        .map_err(|error| format!("start runtime command control process: {error}"))?;
    Ok(())
}

async fn http_endpoint_health(client: &reqwest::Client, endpoint: &str) -> SessionHealth {
    #[derive(Deserialize)]
    struct PingResponse {
        status: String,
    }

    let Ok(response) = client.get(endpoint).send().await else {
        return SessionHealth::Unhealthy;
    };
    if !response.status().is_success() {
        return SessionHealth::Unhealthy;
    }
    match response.json::<PingResponse>().await {
        Ok(response) if response.status == "Healthy" => SessionHealth::Healthy,
        Ok(response) if response.status == "HealthyBusy" => SessionHealth::HealthyBusy,
        _ => SessionHealth::Unhealthy,
    }
}

async fn tcp_endpoint_health(endpoint: &str) -> SessionHealth {
    let Ok(endpoint) = reqwest::Url::parse(endpoint) else {
        return SessionHealth::Unhealthy;
    };
    let Some(host) = endpoint.host_str() else {
        return SessionHealth::Unhealthy;
    };
    let Some(port) = endpoint.port_or_known_default() else {
        return SessionHealth::Unhealthy;
    };
    match tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    {
        Ok(Ok(_)) => SessionHealth::Healthy,
        Ok(Err(_)) | Err(_) => SessionHealth::Unhealthy,
    }
}

fn container_endpoint(
    inspection: &bollard::models::ContainerInspectResponse,
    deployment: &ResolvedRuntime,
) -> Result<String, String> {
    match deployment.connectivity.mode {
        ConnectivityMode::Native => {
            let port = format!("{}/tcp", deployment.protocol.port());
            let host_port = inspection
                .network_settings
                .as_ref()
                .and_then(|settings| settings.ports.as_ref())
                .and_then(|ports| ports.get(&port))
                .and_then(Option::as_ref)
                .and_then(|bindings| bindings.first())
                .and_then(|binding| binding.host_port.as_deref())
                .filter(|port| !port.is_empty())
                .ok_or_else(|| format!("runtime container has no published {port}"))?;
            Ok(format!("http://127.0.0.1:{host_port}"))
        }
        ConnectivityMode::Container => {
            let name = inspection
                .name
                .as_deref()
                .map(|name| name.trim_start_matches('/'))
                .filter(|name| !name.is_empty())
                .ok_or_else(|| "runtime container has no network name".to_owned())?;
            Ok(format!("http://{name}:{}", deployment.protocol.port()))
        }
    }
}

fn is_owned_reconciliation_candidate(
    labels: &HashMap<String, String>,
    runtime_owner: &str,
) -> bool {
    labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
        && labels.get(OWNER_LABEL).map(String::as_str) == Some(runtime_owner)
        && labels
            .get(SESSION_LABEL)
            .is_some_and(|session_id| valid_runtime_session_label(session_id))
}

fn valid_runtime_session_label(value: &str) -> bool {
    (33..=256).contains(&value.len())
        && value.bytes().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, b'.' | b':' | b'_' | b'-')
        })
}

fn session_key_digest(runtime_owner: &str, key: &SessionKey) -> String {
    let digest = Sha256::digest(format!(
        "{runtime_owner}:{}:{}:{}",
        key.runtime_arn, key.qualifier, key.runtime_session_id
    ));
    hex::encode(digest)[..24].to_owned()
}

fn session_container_name(runtime_owner: &str, key: &SessionKey) -> String {
    format!(
        "agentcore-session-{}",
        session_key_digest(runtime_owner, key)
    )
}

fn session_volume_name(runtime_owner: &str, key: &SessionKey) -> String {
    format!(
        "agentcore-session-data-{}",
        session_key_digest(runtime_owner, key)
    )
}

fn session_host_config(
    deployment: &ResolvedRuntime,
    volume_name: &str,
    mount_path: &str,
) -> HostConfig {
    let mut host_config = agent_host_config(deployment);
    if let Some(tmpfs) = host_config.tmpfs.as_mut() {
        tmpfs.remove(mount_path);
        if tmpfs.is_empty() {
            host_config.tmpfs = None;
        }
    }
    host_config.mounts = Some(vec![Mount {
        target: Some(mount_path.to_owned()),
        source: Some(volume_name.to_owned()),
        typ: Some(MountTypeEnum::VOLUME),
        read_only: Some(false),
        volume_options: Some(MountVolumeOptions {
            no_copy: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    }]);
    if deployment.connectivity.mode == ConnectivityMode::Native {
        host_config.port_bindings = Some(HashMap::from([(
            format!("{}/tcp", deployment.protocol.port()),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_owned()),
                host_port: Some(String::new()),
            }]),
        )]));
    }
    host_config
}

fn agent_host_config(deployment: &ResolvedRuntime) -> HostConfig {
    let workspace_no_exec = if deployment.resources.workspace_no_exec {
        ",noexec"
    } else {
        ""
    };
    let workspace_mount = format!(
        "rw,nosuid,nodev{workspace_no_exec},size={},uid=10001,gid=10001,mode=0700",
        deployment.resources.workspace_size_bytes
    );
    HostConfig {
        cap_drop: Some(vec!["ALL".to_owned()]),
        extra_hosts: deployment
            .connectivity
            .add_host_gateway
            .then(|| vec!["host.docker.internal:host-gateway".to_owned()]),
        init: Some(true),
        memory: Some(deployment.resources.memory_bytes),
        memory_swap: Some(deployment.resources.memory_bytes),
        nano_cpus: Some(deployment.resources.nano_cpus),
        network_mode: deployment.connectivity.docker_network.clone(),
        pids_limit: Some(deployment.resources.pids_limit),
        readonly_rootfs: Some(deployment.resources.read_only_root_filesystem),
        security_opt: Some(vec!["no-new-privileges:true".to_owned()]),
        tmpfs: Some(HashMap::from([("/workspace".to_owned(), workspace_mount)])),
        ..Default::default()
    }
}

#[cfg(test)]
fn deterministic_agent_failure(stderr: &[u8]) -> Option<AgentFailure> {
    let failure = std::str::from_utf8(stderr)
        .ok()?
        .lines()
        .rev()
        .find_map(|line| {
            line.char_indices()
                .filter(|(_, character)| *character == '{')
                .find_map(|(start, _)| serde_json::from_str::<AgentFailure>(&line[start..]).ok())
        })?;
    matches!(
        failure.code.as_str(),
        "invalid_invocation"
            | "invalid_model_policy"
            | "model_not_found"
            | "effort_not_supported"
            | "python_limit_exceeded"
    )
    .then_some(failure)
}

#[cfg(test)]
fn container_name(invocation: &ContainerInvocation) -> String {
    format!("flint-runtime-fixture-{}", invocation.attempt_id)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use axum::{Json, Router, http::StatusCode, routing::get};
    use bollard::{
        Docker,
        models::ContainerCreateBody,
        query_parameters::{
            CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder,
            RemoveVolumeOptionsBuilder,
        },
    };

    use crate::{
        catalog::{ConnectivityMode, LocalIdentity, RuntimeCatalog, RuntimeRegistry},
        config::{DockerDiscoveryConfig, RuntimeConfig},
        session::{CommandEvent, SessionHealth, SessionKey, SessionManager},
    };

    use super::{
        CATALOG_GENERATION_LABEL, CREATED_AT_LABEL, DockerSessionBackend, IMAGE_ID_LABEL,
        IMAGE_LABEL, MANAGED_LABEL, OWNER_LABEL, QUALIFIER_LABEL, RUNTIME_LABEL, SESSION_LABEL,
        agent_host_config, container_endpoint, discover_runtime_catalog, http_endpoint_health,
        is_owned_reconciliation_candidate, refresh_discovered_catalog, resolve_catalog_image_ids,
        session_container_name, session_host_config, session_volume_name, tcp_endpoint_health,
    };

    const VALID_SESSION_LABEL: &str = "20000000-0000-0000-0000-000000000099";

    #[tokio::test]
    async fn mcp_tcp_health_checks_endpoint_reachability() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("TCP listener");
        let endpoint = format!(
            "http://{}",
            listener.local_addr().expect("listener address")
        );
        assert_eq!(tcp_endpoint_health(&endpoint).await, SessionHealth::Healthy);
        drop(listener);
        assert_eq!(
            tcp_endpoint_health(&endpoint).await,
            SessionHealth::Unhealthy
        );
    }

    #[tokio::test]
    async fn http_health_requires_a_successful_supported_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("health listener");
        let endpoint = format!("http://{}", listener.local_addr().expect("health address"));
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route(
                        "/healthy",
                        get(|| async { Json(serde_json::json!({"status":"Healthy"})) }),
                    )
                    .route(
                        "/busy",
                        get(|| async { Json(serde_json::json!({"status":"HealthyBusy"})) }),
                    )
                    .route("/missing", get(|| async { Json(serde_json::json!({})) }))
                    .route("/invalid", get(|| async { "not-json" }))
                    .route(
                        "/error",
                        get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
                    ),
            )
            .await
            .expect("serve health fixture");
        });
        let client = reqwest::Client::new();

        assert_eq!(
            http_endpoint_health(&client, &format!("{endpoint}/healthy")).await,
            SessionHealth::Healthy
        );
        assert_eq!(
            http_endpoint_health(&client, &format!("{endpoint}/busy")).await,
            SessionHealth::HealthyBusy
        );
        for path in ["missing", "invalid", "error", "absent"] {
            assert_eq!(
                http_endpoint_health(&client, &format!("{endpoint}/{path}")).await,
                SessionHealth::Unhealthy,
                "{path} must be unhealthy"
            );
        }
        server.abort();
    }

    #[test]
    fn reconciliation_ownership_requires_exact_managed_owner_and_session_labels() {
        let owner = "owner-a";
        let complete = HashMap::from([
            (MANAGED_LABEL.to_owned(), "true".to_owned()),
            (OWNER_LABEL.to_owned(), owner.to_owned()),
            (SESSION_LABEL.to_owned(), VALID_SESSION_LABEL.to_owned()),
        ]);
        assert!(is_owned_reconciliation_candidate(&complete, owner));

        for unrelated in [
            HashMap::from([(MANAGED_LABEL.to_owned(), "true".to_owned())]),
            HashMap::from([(OWNER_LABEL.to_owned(), owner.to_owned())]),
            HashMap::from([
                (MANAGED_LABEL.to_owned(), "true".to_owned()),
                (OWNER_LABEL.to_owned(), "owner-b".to_owned()),
                (SESSION_LABEL.to_owned(), VALID_SESSION_LABEL.to_owned()),
            ]),
            HashMap::from([
                (MANAGED_LABEL.to_owned(), "true".to_owned()),
                (OWNER_LABEL.to_owned(), owner.to_owned()),
                (SESSION_LABEL.to_owned(), "malformed".to_owned()),
            ]),
        ] {
            assert!(!is_owned_reconciliation_candidate(&unrelated, owner));
        }
    }

    #[test]
    fn native_agent_containers_disable_host_gateway_by_default() {
        let deployment = RuntimeConfig::test_defaults()
            .test_catalog()
            .default_snapshot();
        let host_config = agent_host_config(&deployment);

        assert_eq!(host_config.network_mode, None);
        assert_eq!(host_config.extra_hosts, None);
    }

    #[test]
    fn native_session_containers_publish_only_an_ephemeral_loopback_port() {
        let snapshot = RuntimeConfig::test_defaults()
            .test_catalog()
            .default_snapshot();
        let mut deployment = (*snapshot).clone();
        deployment.connectivity.mode = ConnectivityMode::Native;
        deployment.connectivity.docker_network = None;
        let host_config = session_host_config(&deployment, "fixture-volume", "/workspace");
        assert_eq!(host_config.tmpfs, None);
        let mount = host_config
            .mounts
            .as_ref()
            .and_then(|mounts| mounts.first())
            .expect("persistent session mount");
        assert_eq!(mount.source.as_deref(), Some("fixture-volume"));
        assert_eq!(mount.target.as_deref(), Some("/workspace"));
        assert_eq!(mount.typ, Some(bollard::models::MountTypeEnum::VOLUME));
        assert_eq!(mount.read_only, Some(false));
        assert_eq!(
            mount
                .volume_options
                .as_ref()
                .and_then(|options| options.no_copy),
            Some(false)
        );
        let bindings = host_config
            .port_bindings
            .expect("native session publishes its protocol port");
        assert_eq!(bindings.len(), 1);
        let binding = bindings
            .get("8080/tcp")
            .and_then(Option::as_ref)
            .and_then(|bindings| bindings.first())
            .expect("loopback binding");
        assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(binding.host_port.as_deref(), Some(""));
    }

    #[test]
    fn container_session_containers_use_the_configured_network_without_publishing_ports() {
        let snapshot = RuntimeConfig::test_defaults()
            .test_catalog()
            .default_snapshot();
        let mut deployment = (*snapshot).clone();
        deployment.connectivity.mode = ConnectivityMode::Container;
        deployment.connectivity.docker_network = Some("flint-agentcore".to_owned());
        deployment.connectivity.add_host_gateway = false;

        let host_config = session_host_config(&deployment, "fixture-volume", "/workspace");
        assert_eq!(host_config.network_mode.as_deref(), Some("flint-agentcore"));
        assert_eq!(host_config.port_bindings, None);
        assert_eq!(host_config.extra_hosts, None);

        let inspection = bollard::models::ContainerInspectResponse {
            name: Some("/agentcore-session-owner-session".to_owned()),
            ..Default::default()
        };
        assert_eq!(
            container_endpoint(&inspection, &deployment).expect("container network endpoint"),
            "http://agentcore-session-owner-session:8080"
        );
    }

    #[test]
    fn session_container_names_are_stable_per_owner_and_session() {
        let key = SessionKey {
            runtime_arn: "arn:aws:bedrock-agentcore:us-west-2:1:runtime/demo".to_owned(),
            qualifier: "DEFAULT".to_owned(),
            runtime_session_id: "session-a".to_owned(),
        };
        let first = session_container_name("owner-a", &key);
        assert_eq!(first, session_container_name("owner-a", &key));
        let volume = session_volume_name("owner-a", &key);
        assert_eq!(volume, session_volume_name("owner-a", &key));
        let mut other = key;
        other.runtime_session_id = "session-b".to_owned();
        assert_ne!(first, session_container_name("owner-a", &other));
        assert_ne!(volume, session_volume_name("owner-a", &other));
        assert!(first.len() <= 63);
        assert!(volume.len() <= 63);
    }

    fn native_discovery_config(image: &str) -> DockerDiscoveryConfig {
        DockerDiscoveryConfig {
            image_allowlist: vec![image.to_owned()],
            connectivity_mode: ConnectivityMode::Native,
            docker_network: None,
            refresh_interval: Duration::from_secs(30),
            environment_allowlist: vec![
                "FLINT_FIXTURE_ALLOWED".to_owned(),
                "FLINT_FIXTURE_UNSET".to_owned(),
            ],
            header_allowlist: vec!["x-flint-invocation-id".to_owned()],
        }
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the labeled flint-runtime-fixture image"]
    async fn real_docker_discovery_resolves_the_labeled_fixture_immutably() {
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        let catalog = discover_runtime_catalog(
            &docker,
            &native_discovery_config("flint-runtime-fixture:local"),
        )
        .await
        .expect("discover labeled fixture");
        assert_eq!(catalog.len(), 1);
        let identity = LocalIdentity {
            region: "us-west-2".to_owned(),
            account_id: "000000000000".to_owned(),
        };
        let deployment = catalog
            .resolve(
                "arn:aws:bedrock-agentcore:us-west-2:000000000000:runtime/flint_local",
                None,
                Some("DEFAULT"),
                &identity,
            )
            .expect("resolve discovered fixture");
        assert_eq!(deployment.image, "flint-runtime-fixture:local");
        assert!(
            deployment.image_id.contains("@sha256:") || deployment.image_id.starts_with("sha256:")
        );
        assert!(!deployment.image_platform.is_empty());
        assert_eq!(
            deployment.environment_value("FLINT_FIXTURE_ALLOWED"),
            Some("fixture-allowed")
        );
        assert_eq!(
            deployment.environment_value("FLINT_FIXTURE_UNAPPROVED"),
            None
        );
        assert_eq!(deployment.environment_value("FLINT_FIXTURE_UNSET"), None);
        assert_eq!(deployment.environment_warnings.len(), 2);
        DockerSessionBackend::preflight_catalog(&docker, &catalog)
            .await
            .expect("preflight immutable discovery snapshot");
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the labeled flint-runtime-fixture image"]
    async fn real_docker_invalid_refresh_preserves_the_last_known_good_snapshot() {
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        let catalog = discover_runtime_catalog(
            &docker,
            &native_discovery_config("flint-runtime-fixture:local"),
        )
        .await
        .expect("discover labeled fixture");
        let registry = RuntimeRegistry::new(catalog);
        let last_known_good = registry.snapshot();

        refresh_discovered_catalog(
            &docker,
            &registry,
            &native_discovery_config("python:3.13-alpine"),
        )
        .await;

        assert!(Arc::ptr_eq(&last_known_good, &registry.snapshot()));
        assert_eq!(registry.health().discovery_status, "degraded");
        assert!(registry.health().last_error.is_some());
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the labeled flint-runtime-fixture image"]
    async fn real_docker_initial_discovery_failure_preserves_owned_sessions() {
        let owner = format!("agentcore-initial-discovery-test-{}", uuid::Uuid::new_v4());
        let catalog = RuntimeCatalog::test_catalog();
        let backend = DockerSessionBackend::connect(owner.clone(), catalog)
            .await
            .expect("connect session backend");
        let deployment = backend.catalog.snapshot().default_snapshot();
        let manager = SessionManager::new(Arc::new(backend.clone()));
        let lease = manager
            .acquire(Arc::clone(&deployment), VALID_SESSION_LABEL.to_owned())
            .await
            .expect("start owned session");
        let container_id = lease.container.id.clone();
        drop(lease);
        drop(manager);
        drop(backend);

        let missing_image = format!("flint-missing-{}:local", uuid::Uuid::new_v4());
        let error = match DockerSessionBackend::connect_with_registry(
            owner.clone(),
            RuntimeRegistry::new(RuntimeCatalog::empty_discovery()),
            Some(native_discovery_config(&missing_image)),
            "/workspace".to_owned(),
        )
        .await
        {
            Ok(_) => panic!("missing initial discovery image unexpectedly started Flint"),
            Err(error) => error,
        };
        assert!(error.to_string().contains(&missing_image));
        assert!(
            Docker::connect_with_local_defaults()
                .expect("connect to Docker")
                .inspect_container(&container_id, None)
                .await
                .is_ok(),
            "initial discovery failure must leave owned sessions untouched"
        );
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        docker
            .remove_container(
                &container_id,
                Some(RemoveContainerOptionsBuilder::default().force(true).build()),
            )
            .await
            .expect("remove owned session container");
        let key = SessionKey {
            runtime_arn: deployment.runtime_arn.clone(),
            qualifier: deployment.qualifier.clone(),
            runtime_session_id: VALID_SESSION_LABEL.to_owned(),
        };
        docker
            .remove_volume(
                &session_volume_name(&owner, &key),
                Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
            )
            .await
            .expect("remove owned session volume");
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the flint-runtime-fixture image"]
    async fn real_docker_preflight_rejects_a_missing_image() {
        let image = format!("flint-missing-{}:local", uuid::Uuid::new_v4());
        let error = match DockerSessionBackend::connect(
            format!("agentcore-image-test-{}", uuid::Uuid::new_v4()),
            RuntimeCatalog::test_catalog_with_image(&image),
        )
        .await
        {
            Ok(_) => panic!("missing image passed startup preflight"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains(&image));
        assert!(message.contains("build or load it before starting Flint"));
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the flint-runtime-fixture image"]
    async fn real_docker_rejects_mismatched_session_volume_ownership() {
        let owner = format!("agentcore-volume-test-{}", uuid::Uuid::new_v4());
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        let backend = DockerSessionBackend::connect(owner.clone(), RuntimeCatalog::test_catalog())
            .await
            .expect("connect session backend");
        let deployment = backend.catalog.snapshot().default_snapshot();
        let key = SessionKey {
            runtime_arn: deployment.runtime_arn.clone(),
            qualifier: deployment.qualifier.clone(),
            runtime_session_id: VALID_SESSION_LABEL.to_owned(),
        };
        let volume_name = session_volume_name(&owner, &key);
        docker
            .create_volume(bollard::models::VolumeCreateRequest {
                name: Some(volume_name.clone()),
                labels: Some(HashMap::from([
                    (MANAGED_LABEL.to_owned(), "true".to_owned()),
                    (OWNER_LABEL.to_owned(), "another-owner".to_owned()),
                ])),
                ..Default::default()
            })
            .await
            .expect("create mismatched volume");
        let manager = SessionManager::new(Arc::new(backend));

        let error = match manager
            .acquire(deployment, VALID_SESSION_LABEL.to_owned())
            .await
        {
            Ok(_) => panic!("mismatched volume unexpectedly provisioned compute"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("another owner"));
        assert!(docker.inspect_volume(&volume_name).await.is_ok());
        docker
            .remove_volume(
                &volume_name,
                Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
            )
            .await
            .expect("remove mismatched test volume");
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the flint-runtime-fixture image"]
    async fn real_docker_preflight_rejects_a_missing_network() {
        let network = format!("flint-missing-{}", uuid::Uuid::new_v4());
        let error = match DockerSessionBackend::connect(
            format!("agentcore-network-test-{}", uuid::Uuid::new_v4()),
            RuntimeCatalog::test_compose_catalog_with_network(&network),
        )
        .await
        {
            Ok(_) => panic!("missing network passed startup preflight"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains(&network));
        assert!(message.contains("Docker network"));
        assert!(message.contains("unavailable"));
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the flint-runtime-fixture image"]
    async fn real_docker_reconciliation_removes_owned_unhealthy_containers() {
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        docker.ping().await.expect("ping Docker");
        let owner = format!("agentcore-transient-test-{}", uuid::Uuid::new_v4());
        let catalog = resolve_catalog_image_ids(&docker, &RuntimeCatalog::test_catalog())
            .await
            .expect("resolve fixture image");
        let deployment = catalog.default_snapshot();
        let image_id = deployment.image_id.clone();
        let labels = HashMap::from([
            (MANAGED_LABEL.to_owned(), "true".to_owned()),
            (OWNER_LABEL.to_owned(), owner.clone()),
            (RUNTIME_LABEL.to_owned(), deployment.runtime_arn.clone()),
            (QUALIFIER_LABEL.to_owned(), deployment.qualifier.clone()),
            (SESSION_LABEL.to_owned(), VALID_SESSION_LABEL.to_owned()),
            (
                CATALOG_GENERATION_LABEL.to_owned(),
                catalog.generation().to_owned(),
            ),
            (IMAGE_LABEL.to_owned(), deployment.image.clone()),
            (IMAGE_ID_LABEL.to_owned(), image_id),
            (
                CREATED_AT_LABEL.to_owned(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("current time")
                    .as_secs()
                    .to_string(),
            ),
        ]);
        let port = format!("{}/tcp", deployment.protocol.port());
        let name = format!("flint-transient-{}", uuid::Uuid::new_v4());
        let created = docker
            .create_container(
                Some(CreateContainerOptionsBuilder::default().name(&name).build()),
                ContainerCreateBody {
                    image: Some(deployment.image.clone()),
                    entrypoint: Some(vec!["/bin/sh".to_owned(), "-lc".to_owned()]),
                    cmd: Some(vec!["sleep 60".to_owned()]),
                    labels: Some(labels),
                    exposed_ports: Some(vec![port]),
                    host_config: Some(session_host_config(
                        &deployment,
                        "fixture-volume",
                        "/workspace",
                    )),
                    ..Default::default()
                },
            )
            .await
            .expect("create unhealthy owned container");
        docker
            .start_container(
                &created.id,
                None::<bollard::query_parameters::StartContainerOptions>,
            )
            .await
            .expect("start unhealthy owned container");

        DockerSessionBackend::connect(owner, catalog)
            .await
            .expect("reconcile unhealthy owned container");
        assert!(docker.inspect_container(&created.id, None).await.is_err());
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the flint-runtime-fixture image"]
    async fn real_docker_reconciliation_leaves_unowned_containers_untouched() {
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        docker.ping().await.expect("ping Docker");
        let owner = format!("agentcore-reconcile-test-{}", uuid::Uuid::new_v4());
        let cases = [
            (
                "managed-only",
                HashMap::from([(MANAGED_LABEL.to_owned(), "true".to_owned())]),
                true,
            ),
            (
                "owner-only",
                HashMap::from([(OWNER_LABEL.to_owned(), owner.clone())]),
                true,
            ),
            (
                "different-owner",
                HashMap::from([
                    (MANAGED_LABEL.to_owned(), "true".to_owned()),
                    (OWNER_LABEL.to_owned(), "another-owner".to_owned()),
                    (SESSION_LABEL.to_owned(), VALID_SESSION_LABEL.to_owned()),
                ]),
                true,
            ),
            (
                "malformed-session",
                HashMap::from([
                    (MANAGED_LABEL.to_owned(), "true".to_owned()),
                    (OWNER_LABEL.to_owned(), owner.clone()),
                    (SESSION_LABEL.to_owned(), "malformed".to_owned()),
                ]),
                true,
            ),
            (
                "owned-stale",
                HashMap::from([
                    (MANAGED_LABEL.to_owned(), "true".to_owned()),
                    (OWNER_LABEL.to_owned(), owner.clone()),
                    (SESSION_LABEL.to_owned(), VALID_SESSION_LABEL.to_owned()),
                ]),
                false,
            ),
        ];
        let mut created = Vec::new();
        for (case, labels, should_survive) in cases {
            let name = format!("flint-{case}-{}", uuid::Uuid::new_v4());
            let container = docker
                .create_container(
                    Some(CreateContainerOptionsBuilder::default().name(&name).build()),
                    ContainerCreateBody {
                        image: Some("flint-runtime-fixture:local".to_owned()),
                        labels: Some(labels),
                        ..Default::default()
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("create {case} container: {error}"));
            created.push((case, container.id, should_survive));
        }

        DockerSessionBackend::connect(owner, RuntimeCatalog::test_catalog())
            .await
            .expect("reconcile session backend");

        for (case, id, should_survive) in created {
            let inspection = docker.inspect_container(&id, None).await;
            assert_eq!(
                inspection.is_ok(),
                should_survive,
                "unexpected reconciliation result for {case}"
            );
            if should_survive {
                docker
                    .remove_container(
                        &id,
                        Some(RemoveContainerOptionsBuilder::default().force(true).build()),
                    )
                    .await
                    .unwrap_or_else(|error| panic!("remove {case} container: {error}"));
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires local Docker and the flint-runtime-fixture image"]
    async fn real_docker_session_is_ready_reused_adopted_executable_and_removed() {
        let owner = format!("agentcore-session-test-{}", uuid::Uuid::new_v4());
        let docker = Docker::connect_with_local_defaults().expect("connect to Docker");
        let catalog = discover_runtime_catalog(
            &docker,
            &native_discovery_config("flint-runtime-fixture:local"),
        )
        .await
        .expect("discover labeled fixture");
        let backend = DockerSessionBackend::connect(owner.clone(), catalog.clone())
            .await
            .expect("connect session backend");
        let deployment = backend.catalog.snapshot().default_snapshot();
        let manager = SessionManager::new(Arc::new(backend.clone()));
        let first = manager
            .acquire(Arc::clone(&deployment), VALID_SESSION_LABEL.to_owned())
            .await
            .expect("start ready session");
        let container_id = first.container.id.clone();
        drop(first);
        let reused = manager
            .acquire(Arc::clone(&deployment), VALID_SESSION_LABEL.to_owned())
            .await
            .expect("reuse session");
        assert_eq!(reused.container.id, container_id);
        drop(reused);
        drop(manager);

        let adopted_backend = DockerSessionBackend::connect(owner.clone(), catalog)
            .await
            .expect("reconcile session backend");
        let adopted_manager = SessionManager::new(Arc::new(adopted_backend.clone()));
        let adopted = adopted_manager
            .acquire(Arc::clone(&deployment), VALID_SESSION_LABEL.to_owned())
            .await
            .expect("adopt session");
        assert_eq!(adopted.container.id, container_id);

        let mut write = adopted_backend
            .execute_command(
                &adopted.container,
                "printf persistent > /workspace/session-marker".to_owned(),
                None,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("write command");
        while let Some(event) = write.recv().await {
            if let CommandEvent::Exited(code) = event.expect("write event") {
                assert_eq!(code, 0);
            }
        }
        let mut read = adopted_backend
            .execute_command(
                &adopted.container,
                "cat /workspace/session-marker".to_owned(),
                None,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("read command");
        let mut stdout = Vec::new();
        while let Some(event) = read.recv().await {
            match event.expect("read event") {
                CommandEvent::Stdout(chunk) => stdout.extend(chunk),
                CommandEvent::Exited(code) => assert_eq!(code, 0),
                _ => {}
            }
        }
        assert_eq!(stdout, b"persistent");

        let mut environment = adopted_backend
            .execute_command(
                &adopted.container,
                "printf '%s|%s|%s' \"${FLINT_FIXTURE_ALLOWED-unset}\" \"${FLINT_FIXTURE_UNAPPROVED-unset}\" \"${FLINT_FIXTURE_UNSET-unset}\"".to_owned(),
                None,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("inspect forwarded environment");
        let mut environment_stdout = Vec::new();
        while let Some(event) = environment.recv().await {
            match event.expect("environment event") {
                CommandEvent::Stdout(chunk) => environment_stdout.extend(chunk),
                CommandEvent::Exited(code) => assert_eq!(code, 0),
                _ => {}
            }
        }
        assert_eq!(environment_stdout, b"fixture-allowed|unset|unset");
        drop(adopted);
        adopted_backend
            .docker
            .kill_container(&container_id, None)
            .await
            .expect("kill adopted session container");
        let replacement = adopted_manager
            .acquire(Arc::clone(&deployment), VALID_SESSION_LABEL.to_owned())
            .await
            .expect("reprovision killed session");
        let replacement_id = replacement.container.id.clone();
        assert_ne!(replacement_id, container_id);
        let mut persistent_read = adopted_backend
            .execute_command(
                &replacement.container,
                "cat /workspace/session-marker".to_owned(),
                None,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("read persistent marker from replacement");
        let mut persistent_stdout = Vec::new();
        while let Some(event) = persistent_read.recv().await {
            match event.expect("persistent read event") {
                CommandEvent::Stdout(chunk) => persistent_stdout.extend(chunk),
                CommandEvent::Exited(code) => assert_eq!(code, 0),
                _ => {}
            }
        }
        assert_eq!(persistent_stdout, b"persistent");
        drop(replacement);
        adopted_manager
            .stop(
                &deployment,
                VALID_SESSION_LABEL.to_owned(),
                Some("cleanup".to_owned()),
            )
            .await
            .expect("remove replacement compute");
        assert!(
            adopted_backend
                .docker
                .inspect_container(&replacement_id, None)
                .await
                .is_err()
        );
        let key = SessionKey {
            runtime_arn: deployment.runtime_arn.clone(),
            qualifier: deployment.qualifier.clone(),
            runtime_session_id: VALID_SESSION_LABEL.to_owned(),
        };
        let volume_name = session_volume_name(&owner, &key);
        adopted_backend
            .docker
            .inspect_volume(&volume_name)
            .await
            .expect("stopped logical session retains its volume");
        adopted_backend
            .docker
            .remove_volume(
                &volume_name,
                Some(RemoveVolumeOptionsBuilder::default().force(true).build()),
            )
            .await
            .expect("remove test session volume");
    }
}
