use async_trait::async_trait;
use thiserror::Error;

pub mod npm;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("NPM API error {status}: {message}")]
    Api { status: u16, message: String },

    #[error("unsupported upstream scheme: {0}")]
    UnsupportedUpstreamScheme(String),

    #[error("invalid proxy host id: {0}")]
    InvalidHostId(String),

    #[error("unknown error")]
    UnknownError,
}

pub type ProxyResult<T = ()> = Result<T, ProxyError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHost {
    pub id: Option<ProxyHostId>,
    pub domains: Vec<String>,
    pub upstream: Upstream,
    pub websocket: bool,
    pub certificate: Option<CertificateRef>,
    pub force_https: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHostId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub scheme: UpstreamScheme,
    pub host: String,
    pub port: u16,
}

// TODO: To be deleted
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateRef {
    pub id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamScheme {
    Http,
    Https,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyChange {
    Create(ProxyHost),
    Update { id: ProxyHostId, host: ProxyHost },
    Delete { id: ProxyHostId },
}

#[async_trait]
pub trait ProxyProvider: Send + Sync {
    async fn hosts(&self) -> ProxyResult<Vec<ProxyHost>>;

    async fn apply(&self, change: ProxyChange) -> ProxyResult;
}
