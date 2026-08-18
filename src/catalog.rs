use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const CATALOG_SCHEMA_VERSION: u32 = 1;
const DEFAULT_QUALIFIER: &str = "DEFAULT";

#[derive(Clone, Debug)]
pub(crate) struct RuntimeCatalog {
    source: PathBuf,
    generation: String,
    runtimes: Vec<ResolvedRuntimeDefinition>,
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
    pub(crate) runtime_arn: String,
    pub(crate) runtime_id: String,
    pub(crate) account_id: String,
    pub(crate) qualifier: String,
    pub(crate) image: String,
    pub(crate) protocol: Protocol,
    pub(crate) protocol_port: u16,
    pub(crate) invocation_path: String,
    pub(crate) ping_path: String,
    pub(crate) agent_card_path: Option<String>,
    pub(crate) environment: HashMap<String, ResolvedEnvironmentVariable>,
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
        self.environment
            .iter()
            .map(|(name, variable)| format!("{name}={}", variable.value))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedEnvironmentVariable {
    value: String,
    #[allow(dead_code)]
    pub(crate) secret: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Protocol {
    Http,
    Mcp,
    A2a,
    AgUi,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthenticationDocument {
    mode: AuthenticationMode,
    #[serde(default)]
    allowed_clock_skew_seconds: u64,
    #[serde(default)]
    credentials: Vec<CredentialReference>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialReference {
    access_key_id_environment: String,
    secret_access_key_environment: String,
    session_token_environment: Option<String>,
    #[serde(default)]
    principal_arn: Option<String>,
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
#[serde(rename_all = "camelCase")]
struct CatalogDocument {
    schema_version: u32,
    runtimes: Vec<RuntimeDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeDocument {
    runtime_arn: String,
    runtime_id: String,
    account_id: String,
    #[serde(default = "default_qualifier")]
    default_qualifier: String,
    deployments: Vec<DeploymentDocument>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeploymentDocument {
    qualifier: String,
    image: String,
    protocol: Protocol,
    protocol_port: u16,
    invocation_path: String,
    ping_path: String,
    agent_card_path: Option<String>,
    #[serde(default)]
    environment: Vec<EnvironmentReference>,
    resources: ResourcePolicy,
    command: CommandPolicy,
    lifecycle: LifecyclePolicy,
    #[serde(default)]
    allowed_custom_headers: Vec<String>,
    authentication: AuthenticationDocument,
    #[serde(default = "empty_authorization_policy")]
    policy: AuthorizationPolicy,
    connectivity: Connectivity,
    limits: ProxyLimits,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentReference {
    name: String,
    source_environment: String,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    secret: bool,
}

impl RuntimeCatalog {
    pub(crate) fn load(path: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| CatalogError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_bytes_with_environment(path, &bytes, |name| std::env::var(name).ok())
    }

    fn from_bytes_with_environment<F>(
        path: &Path,
        bytes: &[u8],
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
        if document.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::Validation(format!(
                "unsupported schemaVersion {}; expected {CATALOG_SCHEMA_VERSION}",
                document.schema_version
            )));
        }
        if document.runtimes.is_empty() {
            return Err(CatalogError::Validation(
                "catalog must contain at least one runtime".to_owned(),
            ));
        }

        let mut runtime_arns = HashSet::new();
        let mut runtime_ids = HashSet::new();
        let mut runtimes = Vec::with_capacity(document.runtimes.len());
        for runtime in document.runtimes {
            validate_runtime_identity(&runtime)?;
            if !runtime_arns.insert(runtime.runtime_arn.clone()) {
                return Err(CatalogError::Validation(format!(
                    "duplicate runtimeArn {}",
                    runtime.runtime_arn
                )));
            }
            if !runtime_ids.insert((runtime.account_id.clone(), runtime.runtime_id.clone())) {
                return Err(CatalogError::Validation(format!(
                    "duplicate runtimeId {} for account {}",
                    runtime.runtime_id, runtime.account_id
                )));
            }
            if runtime.deployments.is_empty() {
                return Err(CatalogError::Validation(format!(
                    "runtime {} has no deployments",
                    runtime.runtime_id
                )));
            }

            let mut deployments = HashMap::new();
            for deployment in runtime.deployments {
                validate_deployment(&runtime.runtime_id, &deployment)?;
                let qualifier = deployment.qualifier.clone();
                let resolved_environment = resolve_environment(
                    &runtime.runtime_id,
                    &qualifier,
                    deployment.environment,
                    &environment,
                )?;
                let resolved_authentication = resolve_authentication(
                    &runtime.runtime_id,
                    &qualifier,
                    deployment.authentication,
                    &environment,
                )?;
                let resolved = Arc::new(ResolvedRuntime {
                    runtime_arn: runtime.runtime_arn.clone(),
                    runtime_id: runtime.runtime_id.clone(),
                    account_id: runtime.account_id.clone(),
                    qualifier: qualifier.clone(),
                    image: deployment.image,
                    protocol: deployment.protocol,
                    protocol_port: deployment.protocol_port,
                    invocation_path: deployment.invocation_path,
                    ping_path: deployment.ping_path,
                    agent_card_path: deployment.agent_card_path,
                    environment: resolved_environment,
                    resources: deployment.resources,
                    command: deployment.command,
                    lifecycle: deployment.lifecycle,
                    allowed_custom_headers: deployment.allowed_custom_headers,
                    authentication: resolved_authentication,
                    policy: deployment.policy,
                    connectivity: deployment.connectivity,
                    limits: deployment.limits,
                });
                if deployments.insert(qualifier.clone(), resolved).is_some() {
                    return Err(CatalogError::Validation(format!(
                        "runtime {} has duplicate qualifier {qualifier}",
                        runtime.runtime_id
                    )));
                }
            }
            if !deployments.contains_key(&runtime.default_qualifier) {
                return Err(CatalogError::Validation(format!(
                    "runtime {} defaultQualifier {} has no deployment",
                    runtime.runtime_id, runtime.default_qualifier
                )));
            }
            runtimes.push(ResolvedRuntimeDefinition {
                runtime_arn: runtime.runtime_arn,
                runtime_id: runtime.runtime_id,
                account_id: runtime.account_id,
                default_qualifier: runtime.default_qualifier,
                deployments,
            });
        }

        Ok(Self {
            source: path.to_owned(),
            generation: catalog_generation(&canonical_source, &runtimes),
            runtimes,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn resolve(
        &self,
        runtime_identifier: &str,
        account_id: Option<&str>,
        qualifier: Option<&str>,
    ) -> Result<Arc<ResolvedRuntime>, CatalogError> {
        let runtime = if runtime_identifier.starts_with("arn:") {
            self.runtimes
                .iter()
                .find(|runtime| runtime.runtime_arn == runtime_identifier)
        } else {
            let account_id = account_id.ok_or_else(|| {
                CatalogError::Resolution(
                    "accountId is required when resolving a runtime ID".to_owned(),
                )
            })?;
            self.runtimes.iter().find(|runtime| {
                runtime.runtime_id == runtime_identifier && runtime.account_id == account_id
            })
        }
        .ok_or_else(|| {
            CatalogError::Resolution(format!("runtime {runtime_identifier} was not found"))
        })?;
        let qualifier = qualifier.unwrap_or(&runtime.default_qualifier);
        runtime.deployments.get(qualifier).cloned().ok_or_else(|| {
            CatalogError::Resolution(format!(
                "runtime {} has no qualifier {qualifier}",
                runtime.runtime_id
            ))
        })
    }

    pub(crate) fn default_snapshot(&self) -> Arc<ResolvedRuntime> {
        let runtime = &self.runtimes[0];
        runtime.deployments[&runtime.default_qualifier].clone()
    }

    pub(crate) fn snapshots(&self) -> impl Iterator<Item = Arc<ResolvedRuntime>> + '_ {
        self.runtimes
            .iter()
            .flat_map(|runtime| runtime.deployments.values().cloned())
    }

    pub(crate) fn source(&self) -> &Path {
        &self.source
    }

    pub(crate) fn generation(&self) -> &str {
        &self.generation
    }

    #[cfg(test)]
    pub(crate) fn test_catalog() -> Self {
        Self::from_bytes_with_environment(
            Path::new("tests/fixtures/runtime-catalog.json"),
            include_bytes!("../tests/fixtures/runtime-catalog.json"),
            |name| (name == "OPENAI_API_KEY").then(|| "test-openai-key".to_owned()),
        )
        .expect("checked-in runtime catalog is valid")
    }

    #[cfg(test)]
    pub(crate) fn test_catalog_with_image(image: &str) -> Self {
        let document = include_str!("../tests/fixtures/runtime-catalog.json")
            .replace("flint-runtime-fixture:local", image);
        Self::from_bytes_with_environment(
            Path::new("tests/fixtures/runtime-catalog.json"),
            document.as_bytes(),
            |_| None,
        )
        .expect("test image override is valid")
    }

    #[cfg(test)]
    pub(crate) fn test_compose_catalog_with_network(network: &str) -> Self {
        let document = include_str!("../config/runtime-catalog.compose.example.json")
            .replace("flint-agentcore", network);
        Self::from_bytes_with_environment(
            Path::new("config/runtime-catalog.compose.example.json"),
            document.as_bytes(),
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

fn catalog_generation(canonical_source: &[u8], runtimes: &[ResolvedRuntimeDefinition]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_source);
    let mut resolved = Vec::new();
    for runtime in runtimes {
        for deployment in runtime.deployments.values() {
            for (name, variable) in &deployment.environment {
                resolved.push(format!(
                    "environment\0{}\0{}\0{name}\0{}",
                    runtime.runtime_arn, deployment.qualifier, variable.value
                ));
            }
            for credential in &deployment.authentication.credentials {
                resolved.push(format!(
                    "credential\0{}\0{}\0{}\0{}\0{}\0{}",
                    runtime.runtime_arn,
                    deployment.qualifier,
                    credential.access_key_id,
                    credential.secret_access_key,
                    credential.session_token.as_deref().unwrap_or_default(),
                    credential.principal_arn.as_deref().unwrap_or_default(),
                ));
            }
        }
    }
    resolved.sort();
    for value in resolved {
        hasher.update(value.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

fn resolve_environment<F>(
    runtime_id: &str,
    qualifier: &str,
    references: Vec<EnvironmentReference>,
    environment: &F,
) -> Result<HashMap<String, ResolvedEnvironmentVariable>, CatalogError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut resolved = HashMap::new();
    for reference in references {
        validate_environment_name("container environment name", &reference.name)?;
        validate_environment_name("source environment name", &reference.source_environment)?;
        if reference.secret && reference.default.is_some() {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {qualifier} secret {} cannot have a catalog default",
                reference.name
            )));
        }
        let value = environment(&reference.source_environment)
            .filter(|value| !value.trim().is_empty())
            .or(reference.default)
            .ok_or_else(|| CatalogError::MissingEnvironment {
                runtime_id: runtime_id.to_owned(),
                qualifier: qualifier.to_owned(),
                name: reference.source_environment.clone(),
            })?;
        if resolved
            .insert(
                reference.name.clone(),
                ResolvedEnvironmentVariable {
                    value,
                    secret: reference.secret,
                },
            )
            .is_some()
        {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {qualifier} has duplicate environment target {}",
                reference.name
            )));
        }
    }
    Ok(resolved)
}

fn resolve_authentication<F>(
    runtime_id: &str,
    qualifier: &str,
    authentication: AuthenticationDocument,
    environment: &F,
) -> Result<ResolvedAuthenticationPolicy, CatalogError>
where
    F: Fn(&str) -> Option<String>,
{
    let mut credentials = Vec::with_capacity(authentication.credentials.len());
    let mut access_key_ids = HashSet::new();
    for reference in authentication.credentials {
        let required = |name: &str| {
            environment(name)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| CatalogError::MissingEnvironment {
                    runtime_id: runtime_id.to_owned(),
                    qualifier: qualifier.to_owned(),
                    name: name.to_owned(),
                })
        };
        let access_key_id = required(&reference.access_key_id_environment)?;
        if !access_key_ids.insert(access_key_id.clone()) {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {qualifier} has duplicate resolved access key ID"
            )));
        }
        credentials.push(ResolvedCredential {
            access_key_id,
            secret_access_key: required(&reference.secret_access_key_environment)?,
            session_token: reference
                .session_token_environment
                .as_deref()
                .map(required)
                .transpose()?,
            principal_arn: reference.principal_arn,
        });
    }
    Ok(ResolvedAuthenticationPolicy {
        mode: authentication.mode,
        allowed_clock_skew_seconds: authentication.allowed_clock_skew_seconds,
        credentials,
    })
}

fn validate_runtime_identity(runtime: &RuntimeDocument) -> Result<(), CatalogError> {
    if runtime.account_id.len() != 12
        || !runtime
            .account_id
            .bytes()
            .all(|character| character.is_ascii_digit())
    {
        return Err(CatalogError::Validation(format!(
            "runtime {} has an invalid accountId",
            runtime.runtime_id
        )));
    }
    if !valid_runtime_id(&runtime.runtime_id) {
        return Err(CatalogError::Validation(format!(
            "runtimeId {} is invalid",
            runtime.runtime_id
        )));
    }
    let arn_parts = runtime.runtime_arn.splitn(6, ':').collect::<Vec<_>>();
    if arn_parts.len() != 6
        || arn_parts[0] != "arn"
        || arn_parts[2] != "bedrock-agentcore"
        || arn_parts[3].is_empty()
        || arn_parts[4] != runtime.account_id
        || arn_parts[5] != format!("runtime/{}", runtime.runtime_id)
    {
        return Err(CatalogError::Validation(format!(
            "runtimeArn {} does not match its runtimeId and accountId",
            runtime.runtime_arn
        )));
    }
    if !valid_qualifier(&runtime.default_qualifier) {
        return Err(CatalogError::Validation(format!(
            "runtime {} has invalid defaultQualifier {}",
            runtime.runtime_id, runtime.default_qualifier
        )));
    }
    Ok(())
}

fn validate_deployment(
    runtime_id: &str,
    deployment: &DeploymentDocument,
) -> Result<(), CatalogError> {
    if !valid_qualifier(&deployment.qualifier) {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} has invalid qualifier {}",
            deployment.qualifier
        )));
    }
    if deployment.image.trim().is_empty() {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has an empty image",
            deployment.qualifier
        )));
    }
    if deployment.protocol_port == 0 {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has invalid protocolPort 0",
            deployment.qualifier
        )));
    }
    for (name, path) in [
        ("invocationPath", Some(deployment.invocation_path.as_str())),
        ("pingPath", Some(deployment.ping_path.as_str())),
        ("agentCardPath", deployment.agent_card_path.as_deref()),
    ] {
        if path.is_some_and(|path| !valid_http_path(path)) {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {} has invalid {name}",
                deployment.qualifier
            )));
        }
    }
    let resources = &deployment.resources;
    if resources.memory_bytes <= 0
        || resources.nano_cpus <= 0
        || resources.pids_limit <= 0
        || resources.workspace_size_bytes == 0
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has invalid resource limits",
            deployment.qualifier
        )));
    }
    let command = &deployment.command;
    if command.enabled
        && (command.shell.is_empty()
            || command.shell.iter().any(|part| part.is_empty())
            || command.max_concurrency == 0
            || command.timeout_seconds == 0
            || command.max_output_bytes == 0)
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has invalid command policy",
            deployment.qualifier
        )));
    }
    if deployment.lifecycle.startup_timeout_seconds == 0
        || deployment.lifecycle.idle_timeout_seconds == 0
        || deployment.lifecycle.maximum_lifetime_seconds < deployment.lifecycle.idle_timeout_seconds
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has invalid lifecycle policy",
            deployment.qualifier
        )));
    }
    if deployment.limits.max_request_bytes == 0
        || deployment.limits.max_response_bytes == 0
        || deployment.limits.max_chunk_bytes == 0
        || deployment.limits.max_duration_seconds == 0
        || deployment.limits.max_chunk_bytes > deployment.limits.max_response_bytes
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has invalid proxy limits",
            deployment.qualifier
        )));
    }
    let mut headers = HashSet::new();
    for header in &deployment.allowed_custom_headers {
        if header.is_empty()
            || !header.bytes().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == b'-'
            })
            || forbidden_proxy_header(header)
            || !headers.insert(header)
        {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {} has invalid or duplicate custom header {header}",
                deployment.qualifier
            )));
        }
    }
    if deployment.authentication.mode == AuthenticationMode::Permissive
        && !deployment.authentication.credentials.is_empty()
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} permissive authentication cannot configure credentials",
            deployment.qualifier
        )));
    }
    if deployment.authentication.mode != AuthenticationMode::Permissive
        && deployment.authentication.credentials.is_empty()
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} strict authentication requires credentials",
            deployment.qualifier
        )));
    }
    if deployment.authentication.allowed_clock_skew_seconds > 900 {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} has excessive authentication clock skew",
            deployment.qualifier
        )));
    }
    for credential in &deployment.authentication.credentials {
        validate_environment_name(
            "access key ID environment name",
            &credential.access_key_id_environment,
        )?;
        validate_environment_name(
            "secret access key environment name",
            &credential.secret_access_key_environment,
        )?;
        if let Some(session_token) = &credential.session_token_environment {
            validate_environment_name("session token environment name", session_token)?;
        }
    }
    if deployment.authentication.mode == AuthenticationMode::Policy
        && deployment.policy.identity_statements.is_empty()
    {
        return Err(CatalogError::Validation(format!(
            "runtime {runtime_id} qualifier {} policy authentication requires identityStatements",
            deployment.qualifier
        )));
    }
    for (resource_statement, statement) in deployment
        .policy
        .identity_statements
        .iter()
        .map(|statement| (false, statement))
        .chain(
            deployment
                .policy
                .resource_statements
                .iter()
                .map(|statement| (true, statement)),
        )
    {
        if statement.actions.is_empty()
            || statement.resources.is_empty()
            || (resource_statement && statement.principals.is_empty())
        {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {} has an empty policy statement",
                deployment.qualifier
            )));
        }
        for operator in statement.conditions.keys() {
            if !matches!(
                operator.as_str(),
                "StringEquals" | "StringLike" | "ArnEquals" | "ArnLike"
            ) {
                return Err(CatalogError::Validation(format!(
                    "runtime {runtime_id} qualifier {} uses unsupported policy condition operator {operator}",
                    deployment.qualifier
                )));
            }
        }
    }
    match deployment.connectivity.mode {
        ConnectivityMode::Native if deployment.connectivity.docker_network.is_some() => {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {} native connectivity cannot set dockerNetwork",
                deployment.qualifier
            )));
        }
        ConnectivityMode::Container
            if deployment
                .connectivity
                .docker_network
                .as_deref()
                .is_none_or(str::is_empty) =>
        {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {} container connectivity requires dockerNetwork",
                deployment.qualifier
            )));
        }
        ConnectivityMode::Container
            if deployment
                .connectivity
                .docker_network
                .as_deref()
                .is_some_and(|network| !valid_docker_network_name(network)) =>
        {
            return Err(CatalogError::Validation(format!(
                "runtime {runtime_id} qualifier {} requires a named private Docker network",
                deployment.qualifier
            )));
        }
        _ => {}
    }
    Ok(())
}

fn valid_docker_network_name(value: &str) -> bool {
    !matches!(value, "bridge" | "default" | "host" | "none")
        && value.len() <= 255
        && value
            .bytes()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && value.bytes().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, b'.' | b'_' | b'-')
        })
}

fn valid_runtime_id(value: &str) -> bool {
    (1..=48).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
        && value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || character == b'_')
}

fn valid_qualifier(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.bytes().all(|character| {
            character.is_ascii_alphanumeric() || character == b'_' || character == b'-'
        })
}

fn forbidden_proxy_header(value: &str) -> bool {
    matches!(
        value,
        "authorization"
            | "proxy-authorization"
            | "x-amz-date"
            | "x-amz-content-sha256"
            | "x-amz-security-token"
            | "host"
            | "connection"
            | "proxy-connection"
            | "keep-alive"
            | "transfer-encoding"
            | "te"
            | "trailer"
            | "upgrade"
            | "content-length"
            | "x-amzn-bedrock-agentcore-runtime-session-id"
    )
}

fn valid_http_path(value: &str) -> bool {
    value.starts_with('/') && !value.contains('?') && !value.contains('#')
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

fn default_command_shell() -> Vec<String> {
    vec!["/bin/sh".to_owned(), "-lc".to_owned()]
}

fn default_startup_timeout_seconds() -> u64 {
    60
}

fn default_qualifier() -> String {
    DEFAULT_QUALIFIER.to_owned()
}

fn empty_authorization_policy() -> AuthorizationPolicy {
    AuthorizationPolicy {
        identity_statements: Vec::new(),
        resource_statements: Vec::new(),
    }
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
    #[error("runtime {runtime_id} qualifier {qualifier} requires environment variable {name}")]
    MissingEnvironment {
        runtime_id: String,
        qualifier: String,
        name: String,
    },
    #[error("runtime catalog resolution failed: {0}")]
    Resolution(String),
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::Path, sync::Arc};

    use super::{CatalogError, ConnectivityMode, Protocol, RuntimeCatalog};

    const CATALOG: &str = r#"
    {
      "schemaVersion": 1,
      "runtimes": [{
        "runtimeArn": "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/flint_local",
        "runtimeId": "flint_local",
        "accountId": "123456789012",
        "defaultQualifier": "DEFAULT",
        "deployments": [{
          "qualifier": "DEFAULT",
          "image": "flint-runtime-fixture:local",
          "protocol": "http",
          "protocolPort": 8080,
          "invocationPath": "/invocations",
          "pingPath": "/ping",
          "environment": [
            {"name": "OPENAI_API_KEY", "sourceEnvironment": "OPENAI_API_KEY", "secret": true},
            {"name": "MODEL", "sourceEnvironment": "AGENT_MODEL", "default": "fixture-model"}
          ],
          "resources": {
            "memoryBytes": 1073741824,
            "nanoCpus": 1000000000,
            "pidsLimit": 64,
            "readOnlyRootFilesystem": true,
            "workspaceSizeBytes": 67108864,
            "workspaceNoExec": true
          },
          "command": {
            "enabled": true,
            "maxConcurrency": 1,
            "timeoutSeconds": 300,
            "maxOutputBytes": 2097152
          },
          "lifecycle": {"idleTimeoutSeconds": 900, "maximumLifetimeSeconds": 28800},
          "allowedCustomHeaders": ["x-flint-invocation-id"],
          "authentication": {"mode": "permissive", "allowedClockSkewSeconds": 300},
          "connectivity": {"mode": "container", "dockerNetwork": "flint-test-network", "addHostGateway": false},
          "limits": {
            "maxRequestBytes": 2097152,
            "maxResponseBytes": 2097152,
            "maxChunkBytes": 262144,
            "maxDurationSeconds": 900
          }
        }]
      }]
    }
    "#;

    fn catalog(environment: &HashMap<&str, &str>) -> Result<RuntimeCatalog, CatalogError> {
        RuntimeCatalog::from_bytes_with_environment(
            Path::new("fixture.json"),
            CATALOG.as_bytes(),
            |name| environment.get(name).map(ToString::to_string),
        )
    }

    #[test]
    fn checked_in_catalogs_separate_native_and_compose_topologies() {
        let native = RuntimeCatalog::from_bytes_with_environment(
            Path::new("config/runtime-catalog.example.json"),
            include_bytes!("../config/runtime-catalog.example.json"),
            |_| None,
        )
        .expect("checked-in native catalog");
        assert_eq!(
            native.default_snapshot().connectivity.mode,
            ConnectivityMode::Native
        );
        assert_eq!(native.runtimes[0].deployments.len(), 1);

        let compose = RuntimeCatalog::from_bytes_with_environment(
            Path::new("config/runtime-catalog.compose.example.json"),
            include_bytes!("../config/runtime-catalog.compose.example.json"),
            |_| None,
        )
        .expect("checked-in Compose catalog");
        let deployment = compose.default_snapshot();
        assert_eq!(deployment.qualifier, "CONTAINER");
        assert_eq!(deployment.connectivity.mode, ConnectivityMode::Container);
        assert_eq!(compose.runtimes[0].deployments.len(), 1);
    }

    #[test]
    fn resolves_immutable_snapshot_by_arn_or_id() {
        let mut environment = HashMap::from([("OPENAI_API_KEY", "secret-one")]);
        let catalog = catalog(&environment).expect("valid catalog");
        let by_arn = catalog
            .resolve(
                "arn:aws:bedrock-agentcore:us-west-2:123456789012:runtime/flint_local",
                None,
                None,
            )
            .expect("resolve ARN");
        environment.insert("OPENAI_API_KEY", "secret-two");
        let by_id = catalog
            .resolve("flint_local", Some("123456789012"), Some("DEFAULT"))
            .expect("resolve ID");

        assert!(Arc::ptr_eq(&by_arn, &by_id));
        assert_eq!(
            by_arn.environment_value("OPENAI_API_KEY"),
            Some("secret-one")
        );
        assert_eq!(by_arn.environment_value("MODEL"), Some("fixture-model"));
        assert_eq!(by_arn.protocol, Protocol::Http);
        assert_eq!(by_arn.connectivity.mode, ConnectivityMode::Container);
        assert_eq!(catalog.generation().len(), 64);
        assert_ne!(
            catalog.generation(),
            super::RuntimeCatalog::from_bytes_with_environment(
                Path::new("fixture.json"),
                CATALOG.as_bytes(),
                |name| environment.get(name).map(ToString::to_string),
            )
            .expect("same catalog with changed secret")
            .generation(),
        );
        let whitespace_only = format!("\n{CATALOG}\n");
        assert_eq!(
            catalog.generation(),
            super::RuntimeCatalog::from_bytes_with_environment(
                Path::new("fixture.json"),
                whitespace_only.as_bytes(),
                |name| (name == "OPENAI_API_KEY").then(|| "secret-one".to_owned()),
            )
            .expect("whitespace-only catalog change")
            .generation(),
        );
    }

    #[test]
    fn requires_account_id_for_runtime_id_resolution() {
        let catalog =
            catalog(&HashMap::from([("OPENAI_API_KEY", "secret")])).expect("valid catalog");
        let error = catalog
            .resolve("flint_local", None, None)
            .expect_err("account ID is required");
        assert!(error.to_string().contains("accountId is required"));
    }

    #[test]
    fn rejects_special_docker_network_modes() {
        for network in ["bridge", "default", "host", "none", "container:peer"] {
            let document = CATALOG.replace("flint-test-network", network);
            let error = RuntimeCatalog::from_bytes_with_environment(
                Path::new("fixture.json"),
                document.as_bytes(),
                |name| (name == "OPENAI_API_KEY").then(|| "secret".to_owned()),
            )
            .expect_err("special Docker network mode");
            assert!(
                error
                    .to_string()
                    .contains("requires a named private Docker network")
            );
        }
    }

    #[test]
    fn rejects_zero_protocol_port() {
        let document = CATALOG.replace("\"protocolPort\": 8080", "\"protocolPort\": 0");
        let error = RuntimeCatalog::from_bytes_with_environment(
            Path::new("fixture.json"),
            document.as_bytes(),
            |name| (name == "OPENAI_API_KEY").then(|| "secret".to_owned()),
        )
        .expect_err("zero protocol port");
        assert!(error.to_string().contains("invalid protocolPort 0"));
    }

    #[test]
    fn rejects_missing_secret_environment() {
        let error = catalog(&HashMap::new()).expect_err("missing secret");
        assert!(matches!(error, CatalogError::MissingEnvironment { .. }));
        assert!(!error.to_string().contains("secret-one"));
    }

    #[test]
    fn rejects_credential_and_hop_by_hop_custom_headers() {
        for forbidden in ["authorization", "x-amz-security-token", "connection"] {
            let document = CATALOG.replace("x-flint-invocation-id", forbidden);
            let error = RuntimeCatalog::from_bytes_with_environment(
                Path::new("fixture.json"),
                document.as_bytes(),
                |name| (name == "OPENAI_API_KEY").then(|| "secret".to_owned()),
            )
            .expect_err("forbidden proxy header");
            assert!(
                error
                    .to_string()
                    .contains("invalid or duplicate custom header")
            );
        }
    }

    #[test]
    fn rejects_catalog_defaults_for_secrets() {
        let document = CATALOG.replace(
            "\"secret\": true}",
            "\"secret\": true, \"default\": \"embedded-secret\"}",
        );
        let error = RuntimeCatalog::from_bytes_with_environment(
            Path::new("fixture.json"),
            document.as_bytes(),
            |_| None,
        )
        .expect_err("embedded secret default");
        assert!(error.to_string().contains("cannot have a catalog default"));
        assert!(!error.to_string().contains("embedded-secret"));
    }
}
