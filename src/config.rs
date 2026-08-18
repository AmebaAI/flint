use std::{env, path::PathBuf};

use thiserror::Error;

use crate::catalog::RuntimeCatalog;

pub(crate) const DEFAULT_CATALOG_PATH: &str = "config/runtime-catalog.example.json";

#[derive(Clone, Debug)]
pub(crate) struct RuntimeConfig {
    pub(crate) catalog: RuntimeCatalog,
    pub(crate) runtime_owner: String,
}

impl RuntimeConfig {
    pub(crate) fn from_env() -> Result<Self, ConfigError> {
        let catalog_path = match env::var("AGENTCORE_RUNTIME_CATALOG") {
            Ok(path) if !path.trim().is_empty() => PathBuf::from(path),
            Ok(_) => return Err(ConfigError::Invalid("AGENTCORE_RUNTIME_CATALOG")),
            Err(env::VarError::NotPresent) => PathBuf::from(DEFAULT_CATALOG_PATH),
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::Invalid("AGENTCORE_RUNTIME_CATALOG"));
            }
        };
        let runtime_owner = match env::var("AGENTCORE_RUNTIME_OWNER") {
            Ok(owner) => owner,
            Err(env::VarError::NotPresent) => {
                return Err(ConfigError::Missing("AGENTCORE_RUNTIME_OWNER"));
            }
            Err(env::VarError::NotUnicode(_)) => {
                return Err(ConfigError::Invalid("AGENTCORE_RUNTIME_OWNER"));
            }
        };
        let config = Self {
            catalog: RuntimeCatalog::load(catalog_path)?,
            runtime_owner,
        };
        config.validate()?;
        Ok(config)
    }

    #[cfg(test)]
    pub(crate) fn test_defaults() -> Self {
        Self {
            catalog: RuntimeCatalog::test_catalog(),
            runtime_owner: format!("agentcore-test-{}", std::process::id()),
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.runtime_owner.trim().is_empty() {
            return Err(ConfigError::Invalid("AGENTCORE_RUNTIME_OWNER"));
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{0} has an invalid value")]
    Invalid(&'static str),
    #[error(transparent)]
    Catalog(#[from] crate::catalog::CatalogError),
}
