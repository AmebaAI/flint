use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub(crate) const RUNTIME_NAME_LABEL: &str = "ai.ameba.flint.runtime.name";
pub(crate) const RUNTIME_PROTOCOL_LABEL: &str = "ai.ameba.flint.runtime.protocol";
pub(crate) const RUNTIME_ENVIRONMENT_VARIABLES_LABEL: &str =
    "ai.ameba.flint.runtime.environment-variables";
pub(crate) const RUNTIME_IDLE_TIMEOUT_LABEL: &str =
    "ai.ameba.flint.runtime.lifecycle.idle-runtime-session-timeout";
pub(crate) const RUNTIME_MAX_LIFETIME_LABEL: &str = "ai.ameba.flint.runtime.lifecycle.max-lifetime";
pub(crate) const DEFAULT_REGION: &str = "us-east-1";
pub(crate) const DEFAULT_ACCOUNT_ID: &str = "000000000000";
pub(crate) const DEFAULT_QUALIFIER: &str = "DEFAULT";
const DEFAULT_IDLE_RUNTIME_SESSION_TIMEOUT: u64 = 900;
const DEFAULT_MAX_LIFETIME: u64 = 28_800;
const MIN_LIFECYCLE_SECONDS: u64 = 60;
const MAX_MICROVM_LIFECYCLE_SECONDS: u64 = 28_800;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeCatalog {
    source: RuntimeCatalogSource,
    generation: String,
    runtimes: Vec<ResolvedRuntimeDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalIdentity {
    pub(crate) region: String,
    pub(crate) account_id: String,
}

impl Default for LocalIdentity {
    fn default() -> Self {
        Self {
            region: DEFAULT_REGION.to_owned(),
            account_id: DEFAULT_ACCOUNT_ID.to_owned(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeCatalogSource {
    File(PathBuf),
    Docker,
}

impl RuntimeCatalogSource {
    fn kind(&self) -> &'static str {
        match self {
            Self::File(_) => "catalog",
            Self::Docker => "docker",
        }
    }
}

impl std::fmt::Display for RuntimeCatalogSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(path) => path.display().fmt(formatter),
            Self::Docker => formatter.write_str("Docker image discovery"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeRegistryHealth {
    pub(crate) runtime_source: &'static str,
    pub(crate) runtime_count: usize,
    pub(crate) discovery_status: &'static str,
    pub(crate) last_successful_refresh_unix_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

struct RuntimeRegistryState {
    catalog: Arc<RuntimeCatalog>,
    health: RuntimeRegistryHealth,
}

#[derive(Clone)]
pub(crate) struct RuntimeRegistry {
    current: Arc<RwLock<RuntimeRegistryState>>,
}

impl RuntimeRegistry {
    pub(crate) fn new(catalog: RuntimeCatalog) -> Self {
        let health = healthy_registry_state(&catalog);
        Self {
            current: Arc::new(RwLock::new(RuntimeRegistryState {
                catalog: Arc::new(catalog),
                health,
            })),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<RuntimeCatalog> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog
            .clone()
    }

    pub(crate) fn replace(&self, catalog: RuntimeCatalog) {
        let health = healthy_registry_state(&catalog);
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RuntimeRegistryState {
            catalog: Arc::new(catalog),
            health,
        };
    }

    pub(crate) fn mark_refresh_failure(&self, error: impl Into<String>) {
        let mut state = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.health.discovery_status = "degraded";
        state.health.last_error = Some(error.into());
    }

    pub(crate) fn health(&self) -> RuntimeRegistryHealth {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .health
            .clone()
    }

    pub(crate) fn resolve(
        &self,
        runtime_identifier: &str,
        account_id: Option<&str>,
        qualifier: Option<&str>,
        identity: &LocalIdentity,
    ) -> Result<Arc<ResolvedRuntime>, CatalogError> {
        self.snapshot()
            .resolve(runtime_identifier, account_id, qualifier, identity)
    }

    pub(crate) fn resolve_stored(
        &self,
        runtime_arn: &str,
        qualifier: Option<&str>,
    ) -> Result<Arc<ResolvedRuntime>, CatalogError> {
        self.snapshot().resolve_stored(runtime_arn, qualifier)
    }
}

fn healthy_registry_state(catalog: &RuntimeCatalog) -> RuntimeRegistryHealth {
    RuntimeRegistryHealth {
        runtime_source: catalog.source.kind(),
        runtime_count: catalog.len(),
        discovery_status: "ready",
        last_successful_refresh_unix_seconds: Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        ),
        last_error: None,
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct ResolvedRuntimeDefinition {
    runtime_arn: String,
    runtime_id: String,
    account_id: String,
    default_qualifier: String,
    deployments: HashMap<String, Arc<ResolvedRuntime>>,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct ResolvedRuntime {
    pub(crate) catalog_generation: String,
    pub(crate) runtime_arn: String,
    pub(crate) runtime_id: String,
    pub(crate) account_id: String,
    pub(crate) qualifier: String,
    pub(crate) image: String,
    pub(crate) image_id: String,
    pub(crate) image_platform: String,
    pub(crate) image_entrypoint: Option<Vec<String>>,
    pub(crate) image_command: Option<Vec<String>>,
    pub(crate) image_environment: Vec<String>,
    pub(crate) image_working_directory: Option<String>,
    pub(crate) protocol: Protocol,
    pub(crate) environment: HashMap<String, ResolvedEnvironmentVariable>,
    pub(crate) environment_warnings: Vec<String>,
    pub(crate) resources: ResourcePolicy,
    pub(crate) command: CommandPolicy,
    pub(crate) lifecycle: LifecyclePolicy,
    pub(crate) allowed_custom_headers: Vec<String>,
    pub(crate) authentication: ResolvedAuthenticationPolicy,
    pub(crate) policy: AuthorizationPolicy,
    pub(crate) connectivity: Connectivity,
    pub(crate) limits: ProxyLimits,
}

impl ResolvedRuntime {
    #[cfg(test)]
    pub(crate) fn environment_value(&self, name: &str) -> Option<&str> {
        self.environment
            .get(name)
            .map(|variable| variable.value.as_str())
    }

    pub(crate) fn container_environment(&self) -> Vec<String> {
        let mut environment = self
            .image_environment
            .iter()
            .map(|entry| {
                let name = entry
                    .split_once('=')
                    .map_or(entry.as_str(), |(name, _)| name);
                (name.to_owned(), entry.clone())
            })
            .collect::<HashMap<_, _>>();
        environment.extend(
            self.environment
                .iter()
                .map(|(name, variable)| (name.clone(), format!("{name}={}", variable.value))),
        );
        let mut environment = environment.into_values().collect::<Vec<_>>();
        environment.sort_unstable();
        environment
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedEnvironmentVariable {
    value: String,
    #[allow(dead_code)]
    pub(crate) secret: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum Protocol {
    #[serde(rename = "HTTP")]
    Http,
    #[serde(rename = "MCP")]
    Mcp,
    #[serde(rename = "A2A")]
    A2a,
    #[serde(rename = "AGUI")]
    AgUi,
}

impl Protocol {
    pub(crate) const fn port(self) -> u16 {
        match self {
            Self::Http | Self::AgUi => 8080,
            Self::Mcp => 8000,
            Self::A2a => 9000,
        }
    }

    pub(crate) const fn invocation_path(self) -> &'static str {
        match self {
            Self::Http | Self::AgUi => "/invocations",
            Self::Mcp => "/mcp",
            Self::A2a => "/",
        }
    }

    pub(crate) const fn ping_path(self) -> Option<&'static str> {
        match self {
            Self::Http | Self::A2a | Self::AgUi => Some("/ping"),
            Self::Mcp => None,
        }
    }

    pub(crate) const fn agent_card_path(self) -> Option<&'static str> {
        match self {
            Self::A2a => Some("/.well-known/agent-card.json"),
            Self::Http | Self::Mcp | Self::AgUi => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResourcePolicy {
    pub(crate) memory_bytes: i64,
    pub(crate) nano_cpus: i64,
    pub(crate) pids_limit: i64,
    pub(crate) read_only_root_filesystem: bool,
    pub(crate) workspace_size_bytes: u64,
    pub(crate) workspace_no_exec: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandPolicy {
    pub(crate) enabled: bool,
    #[serde(default = "default_command_shell")]
    pub(crate) shell: Vec<String>,
    pub(crate) max_concurrency: usize,
    pub(crate) timeout_seconds: u64,
    pub(crate) max_output_bytes: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LifecyclePolicy {
    #[serde(default = "default_startup_timeout_seconds")]
    pub(crate) startup_timeout_seconds: u64,
    pub(crate) idle_timeout_seconds: u64,
    pub(crate) maximum_lifetime_seconds: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AuthenticationMode {
    Permissive,
    Signature,
    Policy,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedAuthenticationPolicy {
    pub(crate) mode: AuthenticationMode,
    pub(crate) allowed_clock_skew_seconds: u64,
    pub(crate) credentials: Vec<ResolvedCredential>,
}

#[derive(Clone)]
pub(crate) struct ResolvedCredential {
    pub(crate) access_key_id: String,
    pub(crate) secret_access_key: String,
    pub(crate) session_token: Option<String>,
    pub(crate) principal_arn: Option<String>,
}

impl std::fmt::Debug for ResolvedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedCredential")
            .field("access_key_id", &"[REDACTED]")
            .field("secret_access_key", &"[REDACTED]")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("principal_arn", &self.principal_arn)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthorizationPolicy {
    #[serde(default, alias = "statements")]
    pub(crate) identity_statements: Vec<PolicyStatement>,
    #[serde(default)]
    pub(crate) resource_statements: Vec<PolicyStatement>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PolicyStatement {
    pub(crate) effect: PolicyEffect,
    pub(crate) actions: Vec<String>,
    pub(crate) resources: Vec<String>,
    #[serde(default)]
    pub(crate) principals: Vec<String>,
    #[serde(default)]
    pub(crate) conditions: HashMap<String, HashMap<String, Vec<String>>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PolicyEffect {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Connectivity {
    pub(crate) mode: ConnectivityMode,
    pub(crate) docker_network: Option<String>,
    #[serde(default)]
    pub(crate) add_host_gateway: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConnectivityMode {
    Native,
    Container,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProxyLimits {
    pub(crate) max_request_bytes: usize,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_chunk_bytes: usize,
    pub(crate) max_duration_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CatalogDocument {
    runtimes: Vec<RuntimeDocument>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RuntimeDocument {
    #[serde(default)]
    name: Option<String>,
    image: String,
    protocol: Protocol,
    #[serde(default)]
    environment_variables: Vec<String>,
    #[serde(default)]
    lifecycle_configuration: LifecycleConfigurationDocument,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct LifecycleConfigurationDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) idle_runtime_session_timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_lifetime: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct RuntimeDescriptor {
    pub(crate) name: String,
    pub(crate) protocol: Protocol,
    #[serde(default)]
    pub(crate) environment_variables: Vec<String>,
    #[serde(default)]
    pub(crate) lifecycle_configuration: LifecycleConfigurationDocument,
}

#[derive(Clone, Debug)]
pub(crate) struct DiscoveredRuntimeImage {
    pub(crate) image_id: String,
    pub(crate) image_platform: String,
    pub(crate) image_entrypoint: Option<Vec<String>>,
    pub(crate) image_command: Option<Vec<String>>,
    pub(crate) image_environment: Vec<String>,
    pub(crate) image_working_directory: Option<String>,
    pub(crate) image_reference: String,
    pub(crate) descriptor: RuntimeDescriptor,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedImage {
    pub(crate) immutable_reference: String,
    pub(crate) platform: String,
    pub(crate) entrypoint: Option<Vec<String>>,
    pub(crate) command: Option<Vec<String>>,
    pub(crate) environment: Vec<String>,
    pub(crate) working_directory: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct DiscoveryPolicy {
    pub(crate) connectivity: Connectivity,
    pub(crate) environment_allowlist: Vec<String>,
    pub(crate) header_allowlist: Vec<String>,
}

pub(crate) fn parse_runtime_descriptor(
    labels: &HashMap<String, String>,
    default_name: &str,
) -> Result<RuntimeDescriptor, RuntimeDescriptorError> {
    let name = labels
        .get(RUNTIME_NAME_LABEL)
        .map_or_else(|| default_name.to_owned(), Clone::clone);
    let protocol_value = required_runtime_label(labels, RUNTIME_PROTOCOL_LABEL)?;
    let protocol = match protocol_value {
        "HTTP" => Protocol::Http,
        "MCP" => Protocol::Mcp,
        "A2A" => Protocol::A2a,
        "AGUI" => Protocol::AgUi,
        value => {
            return Err(RuntimeDescriptorError::InvalidLabel {
                label: RUNTIME_PROTOCOL_LABEL,
                value: value.to_owned(),
                reason: "must be one of HTTP, MCP, A2A, or AGUI".to_owned(),
            });
        }
    };
    let environment_variables = parse_environment_variables(labels)?;
    let lifecycle_configuration = LifecycleConfigurationDocument {
        idle_runtime_session_timeout: parse_optional_lifecycle_value(
            labels,
            RUNTIME_IDLE_TIMEOUT_LABEL,
        )?,
        max_lifetime: parse_optional_lifecycle_value(labels, RUNTIME_MAX_LIFETIME_LABEL)?,
    };
    let descriptor = RuntimeDescriptor {
        name,
        protocol,
        environment_variables,
        lifecycle_configuration,
    };
    validate_runtime_descriptor(&descriptor)?;
    Ok(descriptor)
}

fn required_runtime_label<'a>(
    labels: &'a HashMap<String, String>,
    label: &'static str,
) -> Result<&'a str, RuntimeDescriptorError> {
    labels
        .get(label)
        .map(String::as_str)
        .ok_or(RuntimeDescriptorError::MissingLabel { label })
}

pub(crate) fn runtime_name_from_image(image: &str) -> String {
    let image = image.trim().split('@').next().unwrap_or_default();
    let image = image.rsplit('/').next().unwrap_or(image);
    image.split(':').next().unwrap_or(image).to_owned()
}

fn parse_environment_variables(
    labels: &HashMap<String, String>,
) -> Result<Vec<String>, RuntimeDescriptorError> {
    let Some(value) = labels.get(RUNTIME_ENVIRONMENT_VARIABLES_LABEL) else {
        return Ok(Vec::new());
    };
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .enumerate()
        .map(|(index, name)| {
            if name.is_empty() {
                return Err(RuntimeDescriptorError::InvalidLabel {
                    label: RUNTIME_ENVIRONMENT_VARIABLES_LABEL,
                    value: value.clone(),
                    reason: format!("entry {} is empty", index + 1),
                });
            }
            Ok(name)
        })
        .collect()
}

fn parse_optional_lifecycle_value(
    labels: &HashMap<String, String>,
    label: &'static str,
) -> Result<Option<u64>, RuntimeDescriptorError> {
    let Some(value) = labels.get(label) else {
        return Ok(None);
    };
    value
        .parse()
        .map(Some)
        .map_err(|error| RuntimeDescriptorError::InvalidLabel {
            label,
            value: value.clone(),
            reason: format!("must be an unsigned integer: {error}"),
        })
}

fn validate_runtime_descriptor(
    descriptor: &RuntimeDescriptor,
) -> Result<(), RuntimeDescriptorError> {
    validate_runtime_declaration(
        &descriptor.name,
        &descriptor.environment_variables,
        &descriptor.lifecycle_configuration,
    )
    .map(|_| ())
    .map_err(RuntimeDescriptorError::Validation)
}

fn validate_runtime_declaration(
    name: &str,
    environment_variables: &[String],
    lifecycle: &LifecycleConfigurationDocument,
) -> Result<LifecyclePolicy, String> {
    if !valid_runtime_id(name) {
        return Err(format!("runtime name {name} is invalid"));
    }
    let mut environment_names = HashSet::new();
    for environment_name in environment_variables {
        validate_environment_name("runtime environment name", environment_name)
            .map_err(|error| error.to_string())?;
        if !environment_names.insert(environment_name) {
            return Err(format!(
                "runtime {name} has duplicate environment variable {environment_name}"
            ));
        }
    }
    resolve_lifecycle(lifecycle).map_err(|error| format!("runtime {name} {error}"))
}

fn resolve_lifecycle(
    lifecycle: &LifecycleConfigurationDocument,
) -> Result<LifecyclePolicy, String> {
    let maximum_lifetime_seconds = lifecycle.max_lifetime.unwrap_or(DEFAULT_MAX_LIFETIME);
    let idle_timeout_seconds = lifecycle
        .idle_runtime_session_timeout
        .unwrap_or_else(|| DEFAULT_IDLE_RUNTIME_SESSION_TIMEOUT.min(maximum_lifetime_seconds));
    if !(MIN_LIFECYCLE_SECONDS..=MAX_MICROVM_LIFECYCLE_SECONDS).contains(&idle_timeout_seconds) {
        return Err(format!(
            "idleRuntimeSessionTimeout must be between {MIN_LIFECYCLE_SECONDS} and {MAX_MICROVM_LIFECYCLE_SECONDS} seconds"
        ));
    }
    if !(MIN_LIFECYCLE_SECONDS..=MAX_MICROVM_LIFECYCLE_SECONDS).contains(&maximum_lifetime_seconds)
    {
        return Err(format!(
            "maxLifetime must be between {MIN_LIFECYCLE_SECONDS} and {MAX_MICROVM_LIFECYCLE_SECONDS} seconds"
        ));
    }
    if idle_timeout_seconds > maximum_lifetime_seconds {
        return Err("idleRuntimeSessionTimeout must not exceed maxLifetime".to_owned());
    }
    Ok(LifecyclePolicy {
        startup_timeout_seconds: default_startup_timeout_seconds(),
        idle_timeout_seconds,
        maximum_lifetime_seconds,
    })
}

fn local_runtime_arn(region: &str, account_id: &str, runtime_id: &str) -> String {
    format!("arn:aws:bedrock-agentcore:{region}:{account_id}:runtime/{runtime_id}")
}

fn parse_runtime_arn(value: &str) -> Result<(&str, &str, &str), CatalogError> {
    let parts = value.splitn(6, ':').collect::<Vec<_>>();
    let Some(runtime_id) = parts
        .get(5)
        .and_then(|resource| resource.strip_prefix("runtime/"))
    else {
        return Err(CatalogError::Resolution(format!(
            "runtime {value} was not found"
        )));
    };
    if parts.len() != 6
        || parts[0] != "arn"
        || parts[1] != "aws"
        || parts[2] != "bedrock-agentcore"
        || parts[3].is_empty()
        || parts[4].len() != 12
        || !parts[4].bytes().all(|character| character.is_ascii_digit())
        || !valid_runtime_id(runtime_id)
    {
        return Err(CatalogError::Resolution(format!(
            "runtime {value} was not found"
        )));
    }
    Ok((parts[3], parts[4], runtime_id))
}

fn resolve_requested_environment<F>(
    runtime_name: &str,
    requested: &[String],
    allowlist: &HashSet<&str>,
    environment: &F,
    generation_values: &mut Vec<String>,
) -> (HashMap<String, ResolvedEnvironmentVariable>, Vec<String>)
where
    F: Fn(&str) -> Option<String>,
{
    let mut resolved = HashMap::new();
    let mut warnings = Vec::new();
    for name in requested {
        if !allowlist.contains(name.as_str()) {
            warnings.push(format!(
                "runtime {runtime_name} requested environment variable {name}, but it is not approved by FLINT_RUNTIME_ENV_ALLOWLIST"
            ));
            continue;
        }
        match environment(name).filter(|value| !value.trim().is_empty()) {
            Some(value) => {
                generation_values.push(format!("environment\0{runtime_name}\0{name}\0{value}"));
                resolved.insert(
                    name.clone(),
                    ResolvedEnvironmentVariable {
                        value,
                        secret: true,
                    },
                );
            }
            None => warnings.push(format!(
                "runtime {runtime_name} requested environment variable {name}, but it is not set"
            )),
        }
    }
    (resolved, warnings)
}

fn policy_generation_values(policy: &DiscoveryPolicy) -> Vec<String> {
    vec![
        format!("connectivity\0{:?}", policy.connectivity.mode),
        format!(
            "network\0{}",
            policy
                .connectivity
                .docker_network
                .as_deref()
                .unwrap_or_default()
        ),
        format!(
            "environment-allowlist\0{}",
            policy.environment_allowlist.join("\0")
        ),
        format!("header-allowlist\0{}", policy.header_allowlist.join("\0")),
    ]
}

fn default_authentication() -> ResolvedAuthenticationPolicy {
    ResolvedAuthenticationPolicy {
        mode: AuthenticationMode::Permissive,
        allowed_clock_skew_seconds: 300,
        credentials: Vec::new(),
    }
}

impl RuntimeCatalog {
    pub(crate) fn from_discovered_images<F>(
        mut images: Vec<DiscoveredRuntimeImage>,
        policy: &DiscoveryPolicy,
        environment: F,
    ) -> Result<Self, CatalogError>
    where
        F: Fn(&str) -> Option<String>,
    {
        if images.is_empty() {
            return Ok(Self::empty_discovery());
        }
        images.sort_by(|left, right| {
            (&left.image_id, &left.image_reference).cmp(&(&right.image_id, &right.image_reference))
        });
        let environment_allowlist = policy
            .environment_allowlist
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut runtime_names = HashSet::new();
        let mut generation_values = Vec::new();
        let mut runtimes = Vec::with_capacity(images.len());
        for image in images {
            validate_runtime_descriptor(&image.descriptor)
                .map_err(|error| CatalogError::Validation(error.to_string()))?;
            if !runtime_names.insert(image.descriptor.name.clone()) {
                return Err(CatalogError::Validation(format!(
                    "runtime name {} is claimed by multiple images",
                    image.descriptor.name
                )));
            }
            let descriptor_json = serde_json::to_value(&image.descriptor)
                .map_err(|error| CatalogError::Validation(error.to_string()))?;
            let canonical_descriptor = serde_json_canonicalizer::to_vec(&descriptor_json)
                .map_err(|error| CatalogError::Validation(error.to_string()))?;
            generation_values.push(format!(
                "descriptor\0{}\0{}\0{}",
                image.image_id,
                image.image_reference,
                hex::encode(canonical_descriptor)
            ));
            let lifecycle = validate_runtime_declaration(
                &image.descriptor.name,
                &image.descriptor.environment_variables,
                &image.descriptor.lifecycle_configuration,
            )
            .map_err(CatalogError::Validation)?;
            let (resolved_environment, environment_warnings) = resolve_requested_environment(
                &image.descriptor.name,
                &image.descriptor.environment_variables,
                &environment_allowlist,
                &environment,
                &mut generation_values,
            );
            let runtime_id = image.descriptor.name.clone();
            let runtime_arn = local_runtime_arn(DEFAULT_REGION, DEFAULT_ACCOUNT_ID, &runtime_id);
            let resolved = Arc::new(ResolvedRuntime {
                catalog_generation: String::new(),
                runtime_arn: runtime_arn.clone(),
                runtime_id: runtime_id.clone(),
                account_id: DEFAULT_ACCOUNT_ID.to_owned(),
                qualifier: DEFAULT_QUALIFIER.to_owned(),
                image: image.image_reference,
                image_id: image.image_id,
                image_platform: image.image_platform,
                image_entrypoint: image.image_entrypoint,
                image_command: image.image_command,
                image_environment: image.image_environment,
                image_working_directory: image.image_working_directory,
                protocol: image.descriptor.protocol,
                environment: resolved_environment,
                environment_warnings,
                resources: default_discovery_resources(),
                command: default_discovery_command(),
                lifecycle,
                allowed_custom_headers: policy.header_allowlist.clone(),
                authentication: default_authentication(),
                policy: empty_authorization_policy(),
                connectivity: policy.connectivity.clone(),
                limits: default_discovery_limits(),
            });
            runtimes.push(ResolvedRuntimeDefinition {
                runtime_arn,
                runtime_id,
                account_id: DEFAULT_ACCOUNT_ID.to_owned(),
                default_qualifier: DEFAULT_QUALIFIER.to_owned(),
                deployments: HashMap::from([(DEFAULT_QUALIFIER.to_owned(), resolved)]),
            });
        }
        runtimes.sort_by(|left, right| left.runtime_id.cmp(&right.runtime_id));
        generation_values.extend(policy_generation_values(policy));
        generation_values.sort_unstable();
        let mut hasher = Sha256::new();
        hasher.update(b"flint-docker-discovery-v2\0");
        for value in generation_values {
            hasher.update(value.as_bytes());
            hasher.update([0]);
        }
        let generation = hex::encode(hasher.finalize());
        set_catalog_generation(&mut runtimes, &generation);
        Ok(Self {
            source: RuntimeCatalogSource::Docker,
            generation,
            runtimes,
        })
    }

    pub(crate) fn with_resolved_image_ids(
        &self,
        image_ids: &HashMap<String, ResolvedImage>,
    ) -> Result<Self, CatalogError> {
        let mut resolved = self.clone();
        let mut generation_values = Vec::new();
        for runtime in &mut resolved.runtimes {
            for deployment in runtime.deployments.values_mut() {
                let deployment = Arc::make_mut(deployment);
                let image = image_ids.get(&deployment.image).ok_or_else(|| {
                    CatalogError::Validation(format!(
                        "runtime {} qualifier {} image {} has no immutable image ID",
                        deployment.runtime_id, deployment.qualifier, deployment.image
                    ))
                })?;
                deployment.image_id.clone_from(&image.immutable_reference);
                deployment.image_platform.clone_from(&image.platform);
                deployment.image_entrypoint.clone_from(&image.entrypoint);
                deployment.image_command.clone_from(&image.command);
                deployment.image_environment.clone_from(&image.environment);
                deployment
                    .image_working_directory
                    .clone_from(&image.working_directory);
                generation_values.push(format!(
                    "image\0{}\0{}\0{}\0{}",
                    deployment.runtime_arn,
                    deployment.qualifier,
                    image.immutable_reference,
                    image.platform
                ));
            }
        }
        generation_values.sort_unstable();
        let mut hasher = Sha256::new();
        hasher.update(resolved.generation.as_bytes());
        for value in generation_values {
            hasher.update(value.as_bytes());
            hasher.update([0]);
        }
        resolved.generation = hex::encode(hasher.finalize());
        set_catalog_generation(&mut resolved.runtimes, &resolved.generation);
        Ok(resolved)
    }

    pub(crate) fn load(
        path: impl AsRef<Path>,
        policy: &DiscoveryPolicy,
    ) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| CatalogError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_bytes_with_environment(path, &bytes, policy, |name| std::env::var(name).ok())
    }

    fn from_bytes_with_environment<F>(
        path: &Path,
        bytes: &[u8],
        policy: &DiscoveryPolicy,
        environment: F,
    ) -> Result<Self, CatalogError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let document: CatalogDocument =
            serde_json::from_slice(bytes).map_err(|source| CatalogError::Parse {
                path: path.to_owned(),
                source,
            })?;
        let source_value: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|source| CatalogError::Parse {
                path: path.to_owned(),
                source,
            })?;
        let canonical_source =
            serde_json_canonicalizer::to_vec(&source_value).map_err(|source| {
                CatalogError::Parse {
                    path: path.to_owned(),
                    source,
                }
            })?;
        if document.runtimes.is_empty() {
            return Err(CatalogError::Validation(
                "catalog must contain at least one runtime".to_owned(),
            ));
        }

        let environment_allowlist = policy
            .environment_allowlist
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let mut runtime_names = HashSet::new();
        let mut generation_values = policy_generation_values(policy);
        let mut runtimes = Vec::with_capacity(document.runtimes.len());
        for runtime in document.runtimes {
            let image = runtime.image;
            if image.trim().is_empty() {
                return Err(CatalogError::Validation(
                    "runtime has an empty image".to_owned(),
                ));
            }
            let name = runtime
                .name
                .unwrap_or_else(|| runtime_name_from_image(&image));
            if !runtime_names.insert(name.clone()) {
                return Err(CatalogError::Validation(format!(
                    "duplicate runtime name {name}"
                )));
            }
            let lifecycle = validate_runtime_declaration(
                &name,
                &runtime.environment_variables,
                &runtime.lifecycle_configuration,
            )
            .map_err(CatalogError::Validation)?;
            let (resolved_environment, environment_warnings) = resolve_requested_environment(
                &name,
                &runtime.environment_variables,
                &environment_allowlist,
                &environment,
                &mut generation_values,
            );
            let runtime_id = name;
            let runtime_arn = local_runtime_arn(DEFAULT_REGION, DEFAULT_ACCOUNT_ID, &runtime_id);
            let resolved = Arc::new(ResolvedRuntime {
                catalog_generation: String::new(),
                runtime_arn: runtime_arn.clone(),
                runtime_id: runtime_id.clone(),
                account_id: DEFAULT_ACCOUNT_ID.to_owned(),
                qualifier: DEFAULT_QUALIFIER.to_owned(),
                image,
                image_id: String::new(),
                image_platform: String::new(),
                image_entrypoint: None,
                image_command: None,
                image_environment: Vec::new(),
                image_working_directory: None,
                protocol: runtime.protocol,
                environment: resolved_environment,
                environment_warnings,
                resources: default_discovery_resources(),
                command: default_discovery_command(),
                lifecycle,
                allowed_custom_headers: policy.header_allowlist.clone(),
                authentication: default_authentication(),
                policy: empty_authorization_policy(),
                connectivity: policy.connectivity.clone(),
                limits: default_discovery_limits(),
            });
            runtimes.push(ResolvedRuntimeDefinition {
                runtime_arn,
                runtime_id,
                account_id: DEFAULT_ACCOUNT_ID.to_owned(),
                default_qualifier: DEFAULT_QUALIFIER.to_owned(),
                deployments: HashMap::from([(DEFAULT_QUALIFIER.to_owned(), resolved)]),
            });
        }

        generation_values.sort_unstable();
        let generation = catalog_generation(&canonical_source, &generation_values);
        set_catalog_generation(&mut runtimes, &generation);
        Ok(Self {
            source: RuntimeCatalogSource::File(path.to_owned()),
            generation,
            runtimes,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn resolve(
        &self,
        runtime_identifier: &str,
        account_id: Option<&str>,
        qualifier: Option<&str>,
        identity: &LocalIdentity,
    ) -> Result<Arc<ResolvedRuntime>, CatalogError> {
        if account_id.is_some_and(|account_id| account_id != identity.account_id) {
            return Err(CatalogError::IdentityMismatch(
                "accountId does not match the request credentials".to_owned(),
            ));
        }
        let runtime_id = if runtime_identifier.starts_with("arn:") {
            let (region, arn_account_id, runtime_id) = parse_runtime_arn(runtime_identifier)?;
            if region != identity.region || arn_account_id != identity.account_id {
                return Err(CatalogError::IdentityMismatch(
                    "runtime ARN does not match the request credential scope".to_owned(),
                ));
            }
            runtime_id
        } else {
            let account_id = account_id.ok_or_else(|| {
                CatalogError::Resolution(
                    "accountId is required when resolving a runtime ID".to_owned(),
                )
            })?;
            if account_id != identity.account_id {
                return Err(CatalogError::IdentityMismatch(
                    "accountId does not match the request credentials".to_owned(),
                ));
            }
            runtime_identifier
        };
        let runtime = self
            .runtimes
            .iter()
            .find(|runtime| runtime.runtime_id == runtime_id)
            .ok_or_else(|| {
                CatalogError::Resolution(format!("runtime {runtime_identifier} was not found"))
            })?;
        let qualifier = qualifier.unwrap_or(DEFAULT_QUALIFIER);
        if qualifier != DEFAULT_QUALIFIER {
            return Err(CatalogError::Resolution(format!(
                "runtime {} has no qualifier {qualifier}",
                runtime.runtime_id
            )));
        }
        let template = runtime
            .deployments
            .get(DEFAULT_QUALIFIER)
            .expect("minimal runtime has a DEFAULT deployment");
        let mut resolved = (**template).clone();
        resolved.runtime_arn =
            local_runtime_arn(&identity.region, &identity.account_id, runtime_id);
        resolved.account_id.clone_from(&identity.account_id);
        resolved.qualifier = DEFAULT_QUALIFIER.to_owned();
        Ok(Arc::new(resolved))
    }

    fn resolve_stored(
        &self,
        runtime_arn: &str,
        qualifier: Option<&str>,
    ) -> Result<Arc<ResolvedRuntime>, CatalogError> {
        let (region, account_id, _) = parse_runtime_arn(runtime_arn)?;
        self.resolve(
            runtime_arn,
            None,
            qualifier,
            &LocalIdentity {
                region: region.to_owned(),
                account_id: account_id.to_owned(),
            },
        )
    }

    pub(crate) fn empty_discovery() -> Self {
        Self {
            source: RuntimeCatalogSource::Docker,
            generation: hex::encode(Sha256::digest(b"docker-discovery-empty-v1")),
            runtimes: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn default_snapshot(&self) -> Arc<ResolvedRuntime> {
        self.default_snapshot_opt()
            .expect("catalog contains a default runtime")
    }

    pub(crate) fn default_snapshot_opt(&self) -> Option<Arc<ResolvedRuntime>> {
        let runtime = self.runtimes.first()?;
        runtime.deployments.get(&runtime.default_qualifier).cloned()
    }

    pub(crate) fn snapshots(&self) -> impl Iterator<Item = Arc<ResolvedRuntime>> + '_ {
        self.runtimes
            .iter()
            .flat_map(|runtime| runtime.deployments.values().cloned())
    }

    pub(crate) fn source(&self) -> &RuntimeCatalogSource {
        &self.source
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> &str {
        &self.generation
    }

    pub(crate) fn len(&self) -> usize {
        self.runtimes
            .iter()
            .map(|runtime| runtime.deployments.len())
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn test_catalog() -> Self {
        let policy = DiscoveryPolicy {
            connectivity: Connectivity {
                mode: ConnectivityMode::Native,
                docker_network: None,
                add_host_gateway: false,
            },
            environment_allowlist: vec!["OPENAI_API_KEY".to_owned()],
            header_allowlist: vec!["x-flint-invocation-id".to_owned()],
        };
        Self::from_bytes_with_environment(
            Path::new("tests/fixtures/runtime-catalog.json"),
            include_bytes!("../tests/fixtures/runtime-catalog.json"),
            &policy,
            |name| (name == "OPENAI_API_KEY").then(|| "test-openai-key".to_owned()),
        )
        .expect("checked-in runtime catalog is valid")
    }

    #[cfg(test)]
    pub(crate) fn test_catalog_with_image(image: &str) -> Self {
        let document = include_str!("../tests/fixtures/runtime-catalog.json")
            .replace("flint-runtime-fixture:local", image);
        let policy = DiscoveryPolicy {
            connectivity: Connectivity {
                mode: ConnectivityMode::Native,
                docker_network: None,
                add_host_gateway: false,
            },
            environment_allowlist: Vec::new(),
            header_allowlist: vec!["x-flint-invocation-id".to_owned()],
        };
        Self::from_bytes_with_environment(
            Path::new("tests/fixtures/runtime-catalog.json"),
            document.as_bytes(),
            &policy,
            |_| None,
        )
        .expect("test image override is valid")
    }

    #[cfg(test)]
    pub(crate) fn test_compose_catalog_with_network(network: &str) -> Self {
        let policy = DiscoveryPolicy {
            connectivity: Connectivity {
                mode: ConnectivityMode::Container,
                docker_network: Some(network.to_owned()),
                add_host_gateway: false,
            },
            environment_allowlist: Vec::new(),
            header_allowlist: vec!["x-flint-invocation-id".to_owned()],
        };
        Self::from_bytes_with_environment(
            Path::new("config/runtime-catalog.compose.example.json"),
            include_bytes!("../config/runtime-catalog.compose.example.json"),
            &policy,
            |_| None,
        )
        .expect("test network override is valid")
    }

    #[cfg(test)]
    pub(crate) fn test_catalog_with_security(
        mode: AuthenticationMode,
        policy: AuthorizationPolicy,
    ) -> Self {
        let mut catalog = Self::test_catalog();
        let deployment = Arc::make_mut(
            catalog.runtimes[0]
                .deployments
                .get_mut(DEFAULT_QUALIFIER)
                .expect("default test deployment"),
        );
        deployment.authentication = ResolvedAuthenticationPolicy {
            mode,
            allowed_clock_skew_seconds: 300,
            credentials: vec![ResolvedCredential {
                access_key_id: "local-access-key".to_owned(),
                secret_access_key: "local-secret-key".to_owned(),
                session_token: None,
                principal_arn: Some("arn:aws:iam::000000000000:role/local-runtime".to_owned()),
            }],
        };
        deployment.policy = policy;
        catalog
    }
}

fn set_catalog_generation(runtimes: &mut [ResolvedRuntimeDefinition], generation: &str) {
    for runtime in runtimes {
        for deployment in runtime.deployments.values_mut() {
            Arc::make_mut(deployment)
                .catalog_generation
                .clone_from(&generation.to_owned());
        }
    }
}

fn catalog_generation(canonical_source: &[u8], generation_values: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"flint-file-catalog-v2\0");
    hasher.update(canonical_source);
    for value in generation_values {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn valid_runtime_id(value: &str) -> bool {
    (1..=48).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
        && value.bytes().all(|character| {
            character.is_ascii_alphanumeric() || character == b'_' || character == b'-'
        })
}

fn validate_environment_name(label: &str, value: &str) -> Result<(), CatalogError> {
    if value.is_empty()
        || !value
            .bytes()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == b'_')
        || !value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || character == b'_')
    {
        return Err(CatalogError::Validation(format!(
            "{label} {value} is invalid"
        )));
    }
    Ok(())
}

fn default_discovery_resources() -> ResourcePolicy {
    ResourcePolicy {
        memory_bytes: 268_435_456,
        nano_cpus: 500_000_000,
        pids_limit: 64,
        read_only_root_filesystem: true,
        workspace_size_bytes: 67_108_864,
        workspace_no_exec: true,
    }
}

fn default_discovery_command() -> CommandPolicy {
    CommandPolicy {
        enabled: true,
        shell: default_command_shell(),
        max_concurrency: 1,
        timeout_seconds: 30,
        max_output_bytes: 1_048_576,
    }
}

fn default_discovery_limits() -> ProxyLimits {
    ProxyLimits {
        max_request_bytes: 2_097_152,
        max_response_bytes: 2_097_152,
        max_chunk_bytes: 262_144,
        max_duration_seconds: 60,
    }
}

fn default_command_shell() -> Vec<String> {
    vec!["/bin/sh".to_owned(), "-lc".to_owned()]
}

fn default_startup_timeout_seconds() -> u64 {
    60
}

fn empty_authorization_policy() -> AuthorizationPolicy {
    AuthorizationPolicy {
        identity_statements: Vec::new(),
        resource_statements: Vec::new(),
    }
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeDescriptorError {
    #[error("missing runtime label {label}")]
    MissingLabel { label: &'static str },
    #[error("invalid runtime label {label} value {value:?}: {reason}")]
    InvalidLabel {
        label: &'static str,
        value: String,
        reason: String,
    },
    #[error("invalid runtime descriptor: {0}")]
    Validation(String),
}

#[allow(dead_code)]
#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("failed to read runtime catalog {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse runtime catalog {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("invalid runtime catalog: {0}")]
    Validation(String),
    #[error("runtime identity mismatch: {0}")]
    IdentityMismatch(String),
    #[error("runtime catalog resolution failed: {0}")]
    Resolution(String),
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path, sync::Arc};

    use super::*;

    const CATALOG: &str = r#"{
      "runtimes": [{
        "name": "flint_local",
        "image": "flint-runtime-fixture:local",
        "protocol": "HTTP",
        "environmentVariables": ["MODEL", "MISSING"],
        "lifecycleConfiguration": {
          "idleRuntimeSessionTimeout": 600,
          "maxLifetime": 3600
        }
      }]
    }"#;

    fn runtime_labels() -> HashMap<String, String> {
        HashMap::from([
            (RUNTIME_NAME_LABEL.to_owned(), "flint_local".to_owned()),
            (RUNTIME_PROTOCOL_LABEL.to_owned(), "HTTP".to_owned()),
            (
                RUNTIME_ENVIRONMENT_VARIABLES_LABEL.to_owned(),
                "MODEL,MISSING".to_owned(),
            ),
            (RUNTIME_IDLE_TIMEOUT_LABEL.to_owned(), "600".to_owned()),
            (RUNTIME_MAX_LIFETIME_LABEL.to_owned(), "3600".to_owned()),
        ])
    }

    fn policy() -> DiscoveryPolicy {
        DiscoveryPolicy {
            connectivity: Connectivity {
                mode: ConnectivityMode::Native,
                docker_network: None,
                add_host_gateway: false,
            },
            environment_allowlist: vec!["MODEL".to_owned(), "MISSING".to_owned()],
            header_allowlist: vec!["x-flint-invocation-id".to_owned()],
        }
    }

    fn catalog_with_environment(environment: &HashMap<&str, &str>) -> RuntimeCatalog {
        RuntimeCatalog::from_bytes_with_environment(
            Path::new("fixture.json"),
            CATALOG.as_bytes(),
            &policy(),
            |name| environment.get(name).map(ToString::to_string),
        )
        .expect("valid minimal catalog")
    }

    fn discovered_image(
        image_id: &str,
        image_reference: &str,
        descriptor: RuntimeDescriptor,
    ) -> DiscoveredRuntimeImage {
        DiscoveredRuntimeImage {
            image_id: image_id.to_owned(),
            image_platform: "linux/amd64".to_owned(),
            image_entrypoint: Some(vec!["runtime".to_owned()]),
            image_command: None,
            image_environment: vec!["BASE=value".to_owned()],
            image_working_directory: Some("/workspace".to_owned()),
            image_reference: image_reference.to_owned(),
            descriptor,
        }
    }

    #[test]
    fn protocol_contracts_are_fixed() {
        assert_eq!(Protocol::Http.port(), 8080);
        assert_eq!(Protocol::Http.invocation_path(), "/invocations");
        assert_eq!(Protocol::Http.ping_path(), Some("/ping"));
        assert_eq!(Protocol::Http.agent_card_path(), None);
        assert_eq!(Protocol::Mcp.port(), 8000);
        assert_eq!(Protocol::Mcp.invocation_path(), "/mcp");
        assert_eq!(Protocol::Mcp.ping_path(), None);
        assert_eq!(Protocol::Mcp.agent_card_path(), None);
        assert_eq!(Protocol::A2a.port(), 9000);
        assert_eq!(Protocol::A2a.invocation_path(), "/");
        assert_eq!(Protocol::A2a.ping_path(), Some("/ping"));
        assert_eq!(
            Protocol::A2a.agent_card_path(),
            Some("/.well-known/agent-card.json")
        );
        assert_eq!(Protocol::AgUi.port(), 8080);
        assert_eq!(Protocol::AgUi.invocation_path(), "/invocations");
        assert_eq!(Protocol::AgUi.ping_path(), Some("/ping"));
        assert_eq!(Protocol::AgUi.agent_card_path(), None);
    }

    #[test]
    fn image_names_provide_runtime_name_defaults() {
        assert_eq!(
            runtime_name_from_image("ghcr.io/acme/my-runtime:latest"),
            "my-runtime"
        );
        assert_eq!(
            runtime_name_from_image("my-runtime@sha256:abc"),
            "my-runtime"
        );

        let catalog = RuntimeCatalog::from_bytes_with_environment(
            Path::new("catalog.json"),
            br#"{"runtimes":[{"image":"ghcr.io/acme/my-runtime:latest","protocol":"HTTP"}]}"#,
            &policy(),
            |_| None,
        )
        .expect("catalog with default runtime name");
        assert_eq!(catalog.default_snapshot().runtime_id, "my-runtime");
    }

    #[test]
    fn runtime_labels_parse_into_a_descriptor() {
        let mut labels = runtime_labels();
        let descriptor = parse_runtime_descriptor(&labels, "fallback").expect("runtime labels");
        assert_eq!(descriptor.name, "flint_local");
        assert_eq!(descriptor.protocol, Protocol::Http);
        assert_eq!(descriptor.environment_variables, ["MODEL", "MISSING"]);
        assert_eq!(
            descriptor
                .lifecycle_configuration
                .idle_runtime_session_timeout,
            Some(600)
        );
        assert_eq!(descriptor.lifecycle_configuration.max_lifetime, Some(3600));

        labels.remove(RUNTIME_ENVIRONMENT_VARIABLES_LABEL);
        labels.remove(RUNTIME_IDLE_TIMEOUT_LABEL);
        labels.remove(RUNTIME_MAX_LIFETIME_LABEL);
        let minimal =
            parse_runtime_descriptor(&labels, "fallback").expect("minimal runtime labels");
        assert!(minimal.environment_variables.is_empty());
        assert!(
            minimal
                .lifecycle_configuration
                .idle_runtime_session_timeout
                .is_none()
        );
        assert!(minimal.lifecycle_configuration.max_lifetime.is_none());
    }

    #[test]
    fn runtime_labels_default_the_name_and_reject_invalid_values() {
        let mut labels = runtime_labels();
        labels.remove(RUNTIME_NAME_LABEL);
        let descriptor = parse_runtime_descriptor(&labels, "my-runtime").expect("default name");
        assert_eq!(descriptor.name, "my-runtime");

        let mut labels = runtime_labels();
        labels.remove(RUNTIME_PROTOCOL_LABEL);
        assert!(matches!(
            parse_runtime_descriptor(&labels, "fallback"),
            Err(RuntimeDescriptorError::MissingLabel {
                label: RUNTIME_PROTOCOL_LABEL
            })
        ));

        let mut labels = runtime_labels();
        labels.insert(RUNTIME_PROTOCOL_LABEL.to_owned(), "GRPC".to_owned());
        assert!(matches!(
            parse_runtime_descriptor(&labels, "fallback"),
            Err(RuntimeDescriptorError::InvalidLabel {
                label: RUNTIME_PROTOCOL_LABEL,
                ..
            })
        ));

        let mut labels = runtime_labels();
        labels.insert(
            RUNTIME_ENVIRONMENT_VARIABLES_LABEL.to_owned(),
            "MODEL,,MISSING".to_owned(),
        );
        assert!(matches!(
            parse_runtime_descriptor(&labels, "fallback"),
            Err(RuntimeDescriptorError::InvalidLabel {
                label: RUNTIME_ENVIRONMENT_VARIABLES_LABEL,
                ..
            })
        ));

        let mut labels = runtime_labels();
        labels.insert(
            RUNTIME_IDLE_TIMEOUT_LABEL.to_owned(),
            "not-a-number".to_owned(),
        );
        assert!(matches!(
            parse_runtime_descriptor(&labels, "fallback"),
            Err(RuntimeDescriptorError::InvalidLabel {
                label: RUNTIME_IDLE_TIMEOUT_LABEL,
                ..
            })
        ));

        let mut labels = runtime_labels();
        labels.insert(RUNTIME_IDLE_TIMEOUT_LABEL.to_owned(), "59".to_owned());
        assert!(matches!(
            parse_runtime_descriptor(&labels, "fallback"),
            Err(RuntimeDescriptorError::Validation(_))
        ));

        let mut labels = runtime_labels();
        labels.insert(
            RUNTIME_ENVIRONMENT_VARIABLES_LABEL.to_owned(),
            "MODEL,MODEL".to_owned(),
        );
        assert!(matches!(
            parse_runtime_descriptor(&labels, "fallback"),
            Err(RuntimeDescriptorError::Validation(_))
        ));
    }

    #[test]
    fn catalog_resolves_environment_and_records_nonblocking_warnings() {
        let catalog = catalog_with_environment(&HashMap::from([("MODEL", "fixture-model")]));
        let deployment = catalog.default_snapshot();
        assert_eq!(deployment.environment_value("MODEL"), Some("fixture-model"));
        assert_eq!(deployment.environment_value("MISSING"), None);
        assert_eq!(deployment.environment_warnings.len(), 1);
        assert!(deployment.environment_warnings[0].contains("MISSING"));
        assert_eq!(deployment.lifecycle.idle_timeout_seconds, 600);
        assert_eq!(deployment.lifecycle.maximum_lifetime_seconds, 3600);
        assert_eq!(deployment.allowed_custom_headers, ["x-flint-invocation-id"]);
    }

    #[test]
    fn lifecycle_defaults_match_agentcore_microvm_behavior() {
        let parse = |lifecycle: &str| {
            let document = format!(
                r#"{{"runtimes":[{{"name":"agent","image":"agent:local","protocol":"HTTP"{lifecycle}}}]}}"#
            );
            RuntimeCatalog::from_bytes_with_environment(
                Path::new("lifecycle.json"),
                document.as_bytes(),
                &policy(),
                |_| None,
            )
            .map(|catalog| catalog.default_snapshot().lifecycle.clone())
        };
        let defaults = parse("").expect("default lifecycle");
        assert_eq!(defaults.idle_timeout_seconds, 900);
        assert_eq!(defaults.maximum_lifetime_seconds, 28_800);
        let max_only =
            parse(r#", "lifecycleConfiguration":{"maxLifetime":600}"#).expect("max-only lifecycle");
        assert_eq!(max_only.idle_timeout_seconds, 600);
        assert_eq!(max_only.maximum_lifetime_seconds, 600);
        let idle_only = parse(r#", "lifecycleConfiguration":{"idleRuntimeSessionTimeout":1200}"#)
            .expect("idle-only lifecycle");
        assert_eq!(idle_only.idle_timeout_seconds, 1200);
        assert_eq!(idle_only.maximum_lifetime_seconds, 28_800);
        for lifecycle in [
            r#", "lifecycleConfiguration":{"idleRuntimeSessionTimeout":59}"#,
            r#", "lifecycleConfiguration":{"maxLifetime":28801}"#,
            r#", "lifecycleConfiguration":{"idleRuntimeSessionTimeout":601,"maxLifetime":600}"#,
        ] {
            assert!(parse(lifecycle).is_err());
        }
    }

    #[test]
    fn discovered_runtime_uses_immutable_image_and_rejects_duplicate_names() {
        let descriptor =
            parse_runtime_descriptor(&runtime_labels(), "fallback").expect("descriptor");
        let catalog = RuntimeCatalog::from_discovered_images(
            vec![discovered_image(
                "sha256:immutable",
                "runtime:local",
                descriptor.clone(),
            )],
            &policy(),
            |name| (name == "MODEL").then(|| "fixture-model".to_owned()),
        )
        .expect("discovered catalog");
        let deployment = catalog.default_snapshot();
        assert_eq!(deployment.image_id, "sha256:immutable");
        assert_eq!(deployment.image_platform, "linux/amd64");
        assert_eq!(deployment.environment_value("MODEL"), Some("fixture-model"));
        let error = RuntimeCatalog::from_discovered_images(
            vec![
                discovered_image("sha256:first", "runtime:first", descriptor.clone()),
                discovered_image("sha256:second", "runtime:second", descriptor),
            ],
            &policy(),
            |_| None,
        )
        .expect_err("duplicate runtime name");
        assert!(error.to_string().contains("claimed by multiple images"));
    }

    #[test]
    fn resolves_floci_style_identity_and_rejects_mismatches() {
        let catalog = catalog_with_environment(&HashMap::new());
        let identity = LocalIdentity {
            region: "us-west-2".to_owned(),
            account_id: "123456789012".to_owned(),
        };
        let by_arn = catalog
            .resolve(
                "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/flint_local",
                None,
                None,
                &identity,
            )
            .expect("resolve ARN");
        let by_id = catalog
            .resolve(
                "flint_local",
                Some("123456789012"),
                Some(DEFAULT_QUALIFIER),
                &identity,
            )
            .expect("resolve ID");
        assert_eq!(by_arn.runtime_arn, by_id.runtime_arn);
        assert_eq!(by_arn.account_id, "123456789012");
        assert!(matches!(
            catalog.resolve("flint_local", Some(DEFAULT_ACCOUNT_ID), None, &identity),
            Err(CatalogError::IdentityMismatch(_))
        ));
        assert!(matches!(
            catalog.resolve(
                "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/flint_local",
                Some(DEFAULT_ACCOUNT_ID),
                None,
                &identity,
            ),
            Err(CatalogError::IdentityMismatch(_))
        ));
        assert!(
            catalog
                .resolve("flint_local", Some("123456789012"), Some("BLUE"), &identity,)
                .is_err()
        );
    }

    #[test]
    fn registry_preserves_last_known_good_snapshot() {
        let registry = RuntimeRegistry::new(RuntimeCatalog::empty_discovery());
        let initial = registry.snapshot();
        registry.mark_refresh_failure("invalid marked image");
        assert!(Arc::ptr_eq(&initial, &registry.snapshot()));
        assert_eq!(registry.health().discovery_status, "degraded");
        registry.replace(catalog_with_environment(&HashMap::new()));
        assert_eq!(registry.health().discovery_status, "ready");
        assert_eq!(registry.health().runtime_count, 1);
    }

    #[test]
    fn checked_in_catalogs_use_the_minimal_shape_and_global_topology() {
        let native = RuntimeCatalog::from_bytes_with_environment(
            Path::new("config/runtime-catalog.example.json"),
            include_bytes!("../config/runtime-catalog.example.json"),
            &policy(),
            |_| None,
        )
        .expect("native catalog");
        assert_eq!(
            native.default_snapshot().connectivity.mode,
            ConnectivityMode::Native
        );
        let mut container_policy = policy();
        container_policy.connectivity = Connectivity {
            mode: ConnectivityMode::Container,
            docker_network: Some("flint-agentcore".to_owned()),
            add_host_gateway: false,
        };
        let compose = RuntimeCatalog::from_bytes_with_environment(
            Path::new("config/runtime-catalog.compose.example.json"),
            include_bytes!("../config/runtime-catalog.compose.example.json"),
            &container_policy,
            |_| None,
        )
        .expect("Compose catalog");
        assert_eq!(compose.default_snapshot().qualifier, DEFAULT_QUALIFIER);
        assert_eq!(
            compose.default_snapshot().connectivity.mode,
            ConnectivityMode::Container
        );
    }

    #[test]
    fn catalog_generation_tracks_environment_but_not_json_whitespace() {
        let first = catalog_with_environment(&HashMap::from([("MODEL", "one")]));
        let second = catalog_with_environment(&HashMap::from([("MODEL", "two")]));
        assert_ne!(first.generation(), second.generation());
        let whitespace = format!("\n{CATALOG}\n");
        let same = RuntimeCatalog::from_bytes_with_environment(
            Path::new("fixture.json"),
            whitespace.as_bytes(),
            &policy(),
            |name| (name == "MODEL").then(|| "one".to_owned()),
        )
        .expect("whitespace-only catalog");
        assert_eq!(first.generation(), same.generation());
    }
}
