use std::{
    collections::HashSet,
    env,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use thiserror::Error;

use crate::catalog::{Connectivity, ConnectivityMode, DiscoveryPolicy, RuntimeCatalog};

pub(crate) const DEFAULT_CATALOG_PATH: &str = "config/runtime-catalog.example.json";
const DEFAULT_DISCOVERY_REFRESH_SECONDS: u64 = 30;
const DEFAULT_HEALTH_CHECK_INTERVAL_SECONDS: u64 = 5;
const DEFAULT_RUNTIME_OWNER: &str = "flint";
const DEFAULT_SESSION_STORAGE_MOUNT_PATH: &str = "/workspace";
const MAX_DISCOVERY_REFRESH_SECONDS: u64 = 3_600;
const MAX_HEALTH_CHECK_INTERVAL_SECONDS: u64 = 3_600;

#[derive(Clone, Debug)]
pub(crate) struct RuntimeConfig {
    pub(crate) runtime_owner: String,
    pub(crate) runtime_source: RuntimeSourceConfig,
    pub(crate) state_path: PathBuf,
    pub(crate) session_storage_mount_path: String,
    pub(crate) health_check_interval: Duration,
}

#[derive(Clone, Debug)]
pub(crate) enum RuntimeSourceConfig {
    Catalog { catalog: RuntimeCatalog },
    Docker(DockerDiscoveryConfig),
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct DockerDiscoveryConfig {
    pub(crate) image_allowlist: Vec<String>,
    pub(crate) connectivity_mode: ConnectivityMode,
    pub(crate) docker_network: Option<String>,
    pub(crate) refresh_interval: Duration,
    pub(crate) environment_allowlist: Vec<String>,
    pub(crate) header_allowlist: Vec<String>,
}

impl RuntimeConfig {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(optional_environment)
    }

    fn from_lookup<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&'static str) -> Result<Option<String>, ConfigError>,
    {
        let runtime_owner =
            lookup("AGENTCORE_RUNTIME_OWNER")?.unwrap_or_else(|| DEFAULT_RUNTIME_OWNER.to_owned());
        let source = lookup("AGENTCORE_RUNTIME_SOURCE")?.unwrap_or_else(|| "docker".to_owned());
        let configured_catalog = lookup("AGENTCORE_RUNTIME_CATALOG")?;
        let policy = runtime_policy_from_lookup(&lookup)?;
        let runtime_source = match source.as_str() {
            "catalog" => {
                reject_discovery_only_configuration(&lookup)?;
                let path = configured_catalog
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_CATALOG_PATH));
                let catalog = RuntimeCatalog::load(&path, &policy)?;
                RuntimeSourceConfig::Catalog { catalog }
            }
            "docker" => {
                if configured_catalog.is_some() {
                    return Err(ConfigError::Conflict(
                        "AGENTCORE_RUNTIME_CATALOG requires AGENTCORE_RUNTIME_SOURCE=catalog",
                    ));
                }
                RuntimeSourceConfig::Docker(DockerDiscoveryConfig::from_lookup(&lookup, policy)?)
            }
            _ => return Err(ConfigError::Invalid("AGENTCORE_RUNTIME_SOURCE")),
        };
        let state_path = state_path_from_lookup(&lookup)?;
        let session_storage_mount_path = lookup("FLINT_SESSION_STORAGE_MOUNT_PATH")?
            .unwrap_or_else(|| DEFAULT_SESSION_STORAGE_MOUNT_PATH.to_owned());
        let health_check_interval = lookup("FLINT_HEALTH_CHECK_INTERVAL_SECONDS")?
            .map(|value| {
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (1..=MAX_HEALTH_CHECK_INTERVAL_SECONDS).contains(seconds))
                    .map(Duration::from_secs)
                    .ok_or(ConfigError::Invalid("FLINT_HEALTH_CHECK_INTERVAL_SECONDS"))
            })
            .transpose()?
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_HEALTH_CHECK_INTERVAL_SECONDS));
        let config = Self {
            runtime_owner,
            runtime_source,
            state_path,
            session_storage_mount_path,
            health_check_interval,
        };
        config.validate()?;
        Ok(config)
    }

    #[cfg(test)]
    pub(crate) fn test_defaults() -> Self {
        Self {
            runtime_owner: format!("agentcore-test-{}", std::process::id()),
            runtime_source: RuntimeSourceConfig::Catalog {
                catalog: RuntimeCatalog::test_catalog(),
            },
            state_path: env::temp_dir().join(format!(
                "flint-test-{}-{}.sqlite3",
                std::process::id(),
                uuid::Uuid::new_v4()
            )),
            session_storage_mount_path: DEFAULT_SESSION_STORAGE_MOUNT_PATH.to_owned(),
            health_check_interval: Duration::from_secs(DEFAULT_HEALTH_CHECK_INTERVAL_SECONDS),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_catalog(&self) -> RuntimeCatalog {
        match &self.runtime_source {
            RuntimeSourceConfig::Catalog { catalog, .. } => catalog.clone(),
            RuntimeSourceConfig::Docker(_) => panic!("test config uses catalog source"),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.runtime_owner.trim().is_empty() {
            return Err(ConfigError::Invalid("AGENTCORE_RUNTIME_OWNER"));
        }
        if !self.state_path.is_absolute() {
            return Err(ConfigError::Invalid("FLINT_STATE_PATH"));
        }
        if !valid_container_mount_path(&self.session_storage_mount_path) {
            return Err(ConfigError::Invalid("FLINT_SESSION_STORAGE_MOUNT_PATH"));
        }
        Ok(())
    }
}

fn state_path_from_lookup<F>(lookup: &F) -> Result<PathBuf, ConfigError>
where
    F: Fn(&'static str) -> Result<Option<String>, ConfigError>,
{
    if let Some(path) = lookup("FLINT_STATE_PATH")? {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = lookup("XDG_STATE_HOME")? {
        return Ok(PathBuf::from(path).join("flint/flint.sqlite3"));
    }
    if let Some(home) = lookup("HOME")? {
        return Ok(PathBuf::from(home).join(".local/state/flint/flint.sqlite3"));
    }
    Err(ConfigError::Missing("FLINT_STATE_PATH"))
}

fn valid_container_mount_path(value: &str) -> bool {
    let path = Path::new(value);
    path.is_absolute()
        && path != Path::new("/")
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

impl DockerDiscoveryConfig {
    fn from_lookup<F>(lookup: &F, policy: DiscoveryPolicy) -> Result<Self, ConfigError>
    where
        F: Fn(&'static str) -> Result<Option<String>, ConfigError>,
    {
        let refresh_seconds = lookup("FLINT_DISCOVERY_REFRESH_SECONDS")?
            .map(|value| {
                value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| (1..=MAX_DISCOVERY_REFRESH_SECONDS).contains(seconds))
                    .ok_or(ConfigError::Invalid("FLINT_DISCOVERY_REFRESH_SECONDS"))
            })
            .transpose()?
            .unwrap_or(DEFAULT_DISCOVERY_REFRESH_SECONDS);
        Ok(Self {
            image_allowlist: parse_list(
                "AGENTCORE_RUNTIME_IMAGES",
                lookup("AGENTCORE_RUNTIME_IMAGES")?,
                |value| !value.is_empty(),
            )?,
            connectivity_mode: policy.connectivity.mode,
            docker_network: policy.connectivity.docker_network,
            refresh_interval: Duration::from_secs(refresh_seconds),
            environment_allowlist: policy.environment_allowlist,
            header_allowlist: policy.header_allowlist,
        })
    }
}

fn runtime_policy_from_lookup<F>(lookup: &F) -> Result<DiscoveryPolicy, ConfigError>
where
    F: Fn(&'static str) -> Result<Option<String>, ConfigError>,
{
    let connectivity_mode = match lookup("FLINT_CONNECTIVITY_MODE")?.as_deref() {
        None | Some("native") => ConnectivityMode::Native,
        Some("container") => ConnectivityMode::Container,
        Some(_) => return Err(ConfigError::Invalid("FLINT_CONNECTIVITY_MODE")),
    };
    let docker_network = lookup("FLINT_DOCKER_NETWORK")?;
    match (connectivity_mode, docker_network.as_deref()) {
        (ConnectivityMode::Native, Some(_)) => {
            return Err(ConfigError::Conflict(
                "FLINT_DOCKER_NETWORK requires FLINT_CONNECTIVITY_MODE=container",
            ));
        }
        (ConnectivityMode::Container, None) => {
            return Err(ConfigError::Missing("FLINT_DOCKER_NETWORK"));
        }
        (ConnectivityMode::Container, Some(network)) if !valid_docker_network(network) => {
            return Err(ConfigError::Invalid("FLINT_DOCKER_NETWORK"));
        }
        _ => {}
    }
    Ok(DiscoveryPolicy {
        connectivity: Connectivity {
            mode: connectivity_mode,
            docker_network,
            add_host_gateway: false,
        },
        environment_allowlist: parse_list(
            "FLINT_RUNTIME_ENV_ALLOWLIST",
            lookup("FLINT_RUNTIME_ENV_ALLOWLIST")?,
            valid_environment_name,
        )?,
        header_allowlist: parse_list(
            "FLINT_RUNTIME_HEADER_ALLOWLIST",
            lookup("FLINT_RUNTIME_HEADER_ALLOWLIST")?,
            valid_custom_header,
        )?,
    })
}

fn reject_discovery_only_configuration<F>(lookup: &F) -> Result<(), ConfigError>
where
    F: Fn(&'static str) -> Result<Option<String>, ConfigError>,
{
    for name in [
        "AGENTCORE_RUNTIME_IMAGES",
        "FLINT_DISCOVERY_REFRESH_SECONDS",
    ] {
        if lookup(name)?.is_some() {
            return Err(ConfigError::Conflict(
                "Docker discovery configuration requires AGENTCORE_RUNTIME_SOURCE=docker",
            ));
        }
    }
    Ok(())
}

fn parse_list<F>(
    name: &'static str,
    value: Option<String>,
    valid: F,
) -> Result<Vec<String>, ConfigError>
where
    F: Fn(&str) -> bool,
{
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.trim().is_empty() {
        return Err(ConfigError::Invalid(name));
    }
    let mut seen = HashSet::new();
    let mut values = Vec::new();
    for entry in value.split(',').map(str::trim) {
        if !valid(entry) || !seen.insert(entry.to_owned()) {
            return Err(ConfigError::Invalid(name));
        }
        values.push(entry.to_owned());
    }
    values.sort_unstable();
    Ok(values)
}

fn optional_environment(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Err(ConfigError::Invalid(name)),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::Invalid(name)),
    }
}

fn valid_docker_network(value: &str) -> bool {
    !matches!(value, "bridge" | "default" | "host" | "none")
        && !value.starts_with("container:")
        && value.len() <= 255
        && value
            .bytes()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && value.bytes().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, b'.' | b'_' | b'-')
        })
}

fn valid_environment_name(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == b'_')
        && value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || character == b'_')
}

fn valid_custom_header(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == b'-'
        })
        && !matches!(
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

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{0} has an invalid value")]
    Invalid(&'static str),
    #[error("configuration conflict: {0}")]
    Conflict(&'static str),
    #[error(transparent)]
    Catalog(#[from] crate::catalog::CatalogError),
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{ConfigError, RuntimeConfig, RuntimeSourceConfig};
    use crate::catalog::ConnectivityMode;

    fn config_from(values: &[(&str, &str)]) -> Result<RuntimeConfig, ConfigError> {
        let values = values.iter().copied().collect::<HashMap<_, _>>();
        RuntimeConfig::from_lookup(|name| {
            Ok(values
                .get(name)
                .map(ToString::to_string)
                .or_else(|| (name == "HOME").then(|| "/tmp/flint-config-test".to_owned())))
        })
    }

    #[test]
    fn session_control_defaults_and_validates_overrides() {
        let defaults = config_from(&[]).expect("default config");
        assert_eq!(defaults.runtime_owner, "flint");
        assert_eq!(
            defaults.state_path,
            std::path::Path::new("/tmp/flint-config-test/.local/state/flint/flint.sqlite3")
        );
        assert_eq!(defaults.session_storage_mount_path, "/workspace");
        assert_eq!(defaults.health_check_interval.as_secs(), 5);

        let configured = config_from(&[
            ("AGENTCORE_RUNTIME_OWNER", "owner"),
            ("FLINT_STATE_PATH", "/var/lib/flint/custom.sqlite3"),
            ("FLINT_SESSION_STORAGE_MOUNT_PATH", "/mnt/session"),
            ("FLINT_HEALTH_CHECK_INTERVAL_SECONDS", "10"),
        ])
        .expect("configured session control");
        assert_eq!(
            configured.state_path,
            std::path::Path::new("/var/lib/flint/custom.sqlite3")
        );
        assert_eq!(configured.session_storage_mount_path, "/mnt/session");
        assert_eq!(configured.health_check_interval.as_secs(), 10);

        for (name, value) in [
            ("FLINT_STATE_PATH", "relative.sqlite3"),
            ("FLINT_SESSION_STORAGE_MOUNT_PATH", "workspace"),
            ("FLINT_SESSION_STORAGE_MOUNT_PATH", "/"),
            ("FLINT_SESSION_STORAGE_MOUNT_PATH", "/workspace/../other"),
            ("FLINT_HEALTH_CHECK_INTERVAL_SECONDS", "0"),
            ("FLINT_HEALTH_CHECK_INTERVAL_SECONDS", "3601"),
        ] {
            assert!(
                config_from(&[("AGENTCORE_RUNTIME_OWNER", "owner"), (name, value)]).is_err(),
                "{name}={value} must be rejected"
            );
        }
    }

    #[test]
    fn docker_discovery_is_the_default_source() {
        let config = config_from(&[("AGENTCORE_RUNTIME_OWNER", "owner")]).expect("config");
        let RuntimeSourceConfig::Docker(discovery) = config.runtime_source else {
            panic!("Docker discovery source");
        };
        assert!(discovery.image_allowlist.is_empty());
        assert_eq!(discovery.refresh_interval.as_secs(), 30);
    }

    #[test]
    fn catalog_source_uses_global_policy_and_rejects_discovery_only_options() {
        let config = config_from(&[
            ("AGENTCORE_RUNTIME_OWNER", "owner"),
            ("AGENTCORE_RUNTIME_SOURCE", "catalog"),
            ("FLINT_CONNECTIVITY_MODE", "container"),
            ("FLINT_DOCKER_NETWORK", "flint-agentcore"),
            ("FLINT_RUNTIME_ENV_ALLOWLIST", "MODEL"),
            ("FLINT_RUNTIME_HEADER_ALLOWLIST", "x-one"),
        ])
        .expect("catalog config");
        let RuntimeSourceConfig::Catalog { catalog } = config.runtime_source else {
            panic!("catalog source");
        };
        let deployment = catalog.default_snapshot();
        assert_eq!(deployment.connectivity.mode, ConnectivityMode::Container);
        assert_eq!(deployment.allowed_custom_headers, ["x-one"]);
        let error = config_from(&[
            ("AGENTCORE_RUNTIME_OWNER", "owner"),
            ("AGENTCORE_RUNTIME_SOURCE", "catalog"),
            ("AGENTCORE_RUNTIME_IMAGES", "fixture:local"),
        ])
        .expect_err("conflicting discovery option");
        assert!(matches!(error, ConfigError::Conflict(_)));
    }

    #[test]
    fn container_discovery_requires_a_named_network() {
        for values in [
            vec![
                ("AGENTCORE_RUNTIME_OWNER", "owner"),
                ("FLINT_CONNECTIVITY_MODE", "container"),
            ],
            vec![
                ("AGENTCORE_RUNTIME_OWNER", "owner"),
                ("FLINT_CONNECTIVITY_MODE", "container"),
                ("FLINT_DOCKER_NETWORK", "host"),
            ],
        ] {
            assert!(config_from(&values).is_err());
        }
        assert!(
            config_from(&[
                ("AGENTCORE_RUNTIME_OWNER", "owner"),
                ("FLINT_CONNECTIVITY_MODE", "container"),
                ("FLINT_DOCKER_NETWORK", "flint-agentcore"),
            ])
            .is_ok()
        );
    }

    #[test]
    fn discovery_lists_are_sorted_unique_and_validated() {
        let config = config_from(&[
            ("AGENTCORE_RUNTIME_OWNER", "owner"),
            ("AGENTCORE_RUNTIME_IMAGES", "second:local,first:local"),
            ("FLINT_RUNTIME_ENV_ALLOWLIST", "MODEL,OPENAI_API_KEY"),
            ("FLINT_RUNTIME_HEADER_ALLOWLIST", "x-two,x-one"),
        ])
        .expect("discovery config");
        let RuntimeSourceConfig::Docker(discovery) = config.runtime_source else {
            panic!("Docker discovery source");
        };
        assert_eq!(discovery.image_allowlist, ["first:local", "second:local"]);
        assert_eq!(discovery.header_allowlist, ["x-one", "x-two"]);
        assert!(
            config_from(&[
                ("AGENTCORE_RUNTIME_OWNER", "owner"),
                (
                    "FLINT_RUNTIME_ENV_ALLOWLIST",
                    "OPENAI_API_KEY,OPENAI_API_KEY"
                ),
            ])
            .is_err()
        );
    }
}
