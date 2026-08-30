use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Read(#[from] std::io::Error),

    #[error("failed to parse yaml: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error("environment variable not found: {0}")]
    MissingEnvVar(String),

    #[error("unterminated environment variable reference")]
    UnterminatedEnvVar,
}

pub type ConfigResult<T> = Result<T, ConfigError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFiles {
    pub config: Config,
    pub services: ServicesFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Config {
    pub dns: DnsConfig,
    pub proxy: ProxyConfig,

    #[serde(default)]
    pub certificates: CertificateConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum DnsConfig {
    Cloudflare {
        zone: String,
        api_token: String,
        #[serde(default)]
        propagation_seconds: Option<u32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ProxyConfig {
    Npm {
        host: String,
        url: String,
        identity: String,
        secret: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CertificateConfig {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ServicesFile {
    pub services: BTreeMap<String, ServiceConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ServiceConfig {
    pub domain: String,
    pub upstream: UpstreamConfig,

    #[serde(default)]
    pub tls: bool,

    #[serde(default)]
    pub websocket: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub host: String,
    pub port: u16,

    #[serde(default = "default_upstream_scheme")]
    pub scheme: UpstreamSchemeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpstreamSchemeConfig {
    Http,
    Https,
}

pub fn load_app_files(
    config_path: impl AsRef<Path>,
    services_path: impl AsRef<Path>,
) -> ConfigResult<AppFiles> {
    Ok(AppFiles {
        config: load_yaml(config_path)?,
        services: load_yaml(services_path)?,
    })
}

pub fn load_yaml<T>(path: impl AsRef<Path>) -> ConfigResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    let content = fs::read_to_string(path)?;
    let content = expand_env_vars(&content)?;

    Ok(serde_yaml::from_str(&content)?)
}

fn expand_env_vars(input: &str) -> ConfigResult<String> {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let end = after_start
            .find('}')
            .ok_or(ConfigError::UnterminatedEnvVar)?;
        let name = &after_start[..end];
        let value = std::env::var(name).map_err(|_| ConfigError::MissingEnvVar(name.to_owned()))?;
        output.push_str(&value);
        rest = &after_start[end + 1..];
    }

    output.push_str(rest);

    Ok(output)
}

fn default_upstream_scheme() -> UpstreamSchemeConfig {
    UpstreamSchemeConfig::Http
}
