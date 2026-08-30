use std::time::SystemTime;

use async_trait::async_trait;
use thiserror::Error;

use crate::proxy_provider::ProxyError;

pub const MANAGED_CERTIFICATE_NAME_MARKER: &str = "managed-by:avel-pilot";

#[derive(Debug, Error)]
pub enum CertProviderError {
    #[error("certificate not found: {0}")]
    CertificateNotFound(String),

    #[error("unsupported challenge type: {0}")]
    UnsupportedChallengeType(String),

    #[error("proxy provider error: {0}")]
    Proxy(#[from] ProxyError),

    #[error("certificate operation failed: {0}")]
    OperationFailed(String),

    #[error("unknown error")]
    UnknownError,
}

pub type CertProviderResult<T = ()> = Result<T, CertProviderError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CertificateId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub id: Option<CertificateId>,
    pub name: Option<String>,
    pub meta: CertificateMeta,
    pub domains: Vec<String>,
    pub challenge: CertificateChallenge,
    pub expires_at: Option<SystemTime>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CertificateMeta {
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CertificateChallenge {
    Http01,
    Dns01(Dns01Challenge),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dns01Challenge {
    pub provider: String,
    pub credentials: String,
    pub propagation_seconds: Option<u32>,
}

pub enum CertificateChange {
    Create(Certificate),
    Renew { id: CertificateId },
    Delete { id: CertificateId },
}

#[async_trait]
pub trait CertProvider: Send + Sync {
    async fn certificates(&self) -> CertProviderResult<Vec<Certificate>>;

    async fn apply(&self, change: CertificateChange) -> CertProviderResult;
}
