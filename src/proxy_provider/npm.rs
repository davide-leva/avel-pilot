use async_trait::async_trait;
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::RwLock;

use crate::cert_provider::{
    CertProvider, CertProviderError, CertProviderResult, Certificate, CertificateChallenge,
    CertificateChange, CertificateId, CertificateMeta, MANAGED_CERTIFICATE_NAME_MARKER,
};
use crate::logging;
use crate::proxy_provider::{
    CertificateRef, ProxyChange, ProxyError, ProxyHost, ProxyHostId, ProxyProvider, ProxyResult,
    Upstream, UpstreamScheme,
};

const MANAGED_META_KEY: &str = "avel_pilot";
#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

#[derive(Debug, Serialize)]
struct TokenRequest<'a> {
    identity: &'a str,
    secret: &'a str,
}

#[derive(Debug, Deserialize)]
struct NpmErrorResponse {
    #[serde(default)]
    message: Option<String>,

    #[serde(default)]
    error: Option<NpmErrorBody>,
}

#[derive(Debug, Deserialize)]
struct NpmErrorBody {
    message: String,
}

#[derive(Debug, Deserialize)]
struct NpmProxyHost {
    id: u32,
    domain_names: Vec<String>,
    forward_scheme: String,
    forward_host: String,
    forward_port: u16,
    allow_websocket_upgrade: bool,
    certificate_id: u32,
    ssl_forced: bool,
    #[serde(default)]
    meta: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct NpmProxyHostRequest<'a> {
    domain_names: &'a [String],
    forward_scheme: &'a str,
    forward_host: &'a str,
    forward_port: u16,
    access_list_id: u32,
    certificate_id: u32,
    ssl_forced: bool,
    caching_enabled: bool,
    block_exploits: bool,
    allow_websocket_upgrade: bool,
    http2_support: bool,
    hsts_enabled: bool,
    hsts_subdomains: bool,
    enabled: bool,
    advanced_config: &'a str,
    locations: Vec<serde_json::Value>,
    meta: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct NpmCertificate {
    id: u32,
    provider: String,
    nice_name: String,
    domain_names: Vec<String>,
    #[serde(rename = "expires_on")]
    _expires_on: Option<String>,
    #[serde(default)]
    meta: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct NpmCertificateRequest<'a> {
    provider: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    nice_name: Option<&'a str>,
    domain_names: &'a [String],
    meta: NpmCertificateRequestMeta<'a>,
}

#[derive(Debug, Serialize)]
struct NpmCertificateRequestMeta<'a> {
    dns_challenge: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    dns_provider: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dns_provider_credentials: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    propagation_seconds: Option<u32>,
}

impl TryFrom<NpmProxyHost> for ProxyHost {
    type Error = ProxyError;

    fn try_from(host: NpmProxyHost) -> Result<Self, Self::Error> {
        let scheme = match host.forward_scheme.as_str() {
            "http" => UpstreamScheme::Http,
            "https" => UpstreamScheme::Https,
            other => {
                return Err(ProxyError::UnsupportedUpstreamScheme(other.to_owned()));
            }
        };

        Ok(Self {
            id: Some(ProxyHostId(host.id.to_string())),
            domains: host.domain_names,
            upstream: Upstream {
                scheme,
                host: host.forward_host,
                port: host.forward_port,
            },
            websocket: host.allow_websocket_upgrade,
            certificate: (host.certificate_id != 0).then_some(CertificateRef {
                id: host.certificate_id,
            }),
            force_https: host.ssl_forced,
        })
    }
}

impl From<NpmCertificate> for Certificate {
    fn from(certificate: NpmCertificate) -> Self {
        let challenge = if certificate
            .meta
            .get("dns_challenge")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let provider = certificate
                .meta
                .get("dns_provider")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let propagation_seconds = certificate
                .meta
                .get("propagation_seconds")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());

            CertificateChallenge::Dns01(crate::cert_provider::Dns01Challenge {
                provider,
                credentials: String::new(),
                propagation_seconds,
            })
        } else {
            CertificateChallenge::Http01
        };

        Self {
            id: Some(CertificateId(certificate.id.to_string())),
            meta: CertificateMeta {
                managed: certificate
                    .nice_name
                    .contains(MANAGED_CERTIFICATE_NAME_MARKER),
            },
            name: Some(certificate.nice_name),
            domains: certificate.domain_names,
            challenge,
            expires_at: None,
        }
    }
}

impl<'a> TryFrom<&'a Certificate> for NpmCertificateRequest<'a> {
    type Error = CertProviderError;

    fn try_from(certificate: &'a Certificate) -> Result<Self, Self::Error> {
        let meta = match &certificate.challenge {
            CertificateChallenge::Http01 => NpmCertificateRequestMeta {
                dns_challenge: false,
                dns_provider: None,
                dns_provider_credentials: None,
                propagation_seconds: None,
            },
            CertificateChallenge::Dns01(challenge) => NpmCertificateRequestMeta {
                dns_challenge: true,
                dns_provider: Some(&challenge.provider),
                dns_provider_credentials: Some(&challenge.credentials),
                propagation_seconds: challenge.propagation_seconds,
            },
        };

        Ok(Self {
            provider: "letsencrypt",
            nice_name: certificate.name.as_deref(),
            domain_names: &certificate.domains,
            meta,
        })
    }
}

impl<'a> From<&'a ProxyHost> for NpmProxyHostRequest<'a> {
    fn from(host: &'a ProxyHost) -> Self {
        Self {
            domain_names: &host.domains,
            forward_scheme: match host.upstream.scheme {
                UpstreamScheme::Http => "http",
                UpstreamScheme::Https => "https",
            },
            forward_host: &host.upstream.host,
            forward_port: host.upstream.port,
            access_list_id: 0,
            certificate_id: host
                .certificate
                .as_ref()
                .map(|certificate| certificate.id)
                .unwrap_or(0),
            ssl_forced: host.force_https,
            caching_enabled: false,
            block_exploits: true,
            allow_websocket_upgrade: host.websocket,
            http2_support: host.certificate.is_some(),
            hsts_enabled: false,
            hsts_subdomains: false,
            enabled: true,
            advanced_config: "",
            locations: Vec::new(),
            meta: serde_json::json!({
                MANAGED_META_KEY: true,
            }),
        }
    }
}

impl NpmProxyHost {
    fn is_managed(&self) -> bool {
        self.meta
            .get(MANAGED_META_KEY)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

pub struct NPMProxyProvider {
    client: Client,
    base_url: String,
    identity: String,
    secret: String,
    token: RwLock<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NpmSummary {
    pub proxy_managed: usize,
    pub proxy_unmanaged: usize,
    pub ssl_managed: usize,
    pub ssl_unmanaged: usize,
}

impl NPMProxyProvider {
    pub fn new(
        base_url: impl Into<String>,
        identity: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            identity: identity.into(),
            secret: secret.into(),
            token: RwLock::new(None),
        }
    }

    async fn login(&self) -> ProxyResult<String> {
        let response = self
            .client
            .post(format!("{}/api/tokens", self.base_url))
            .json(&TokenRequest {
                identity: &self.identity,
                secret: &self.secret,
            })
            .send()
            .await?;
        let token = Self::decode_response::<TokenResponse>(response, "POST", "/api/tokens")
            .await?
            .token;

        *self.token.write().await = Some(token.clone());

        Ok(token)
    }

    async fn auth_token(&self) -> ProxyResult<String> {
        if let Some(token) = self.token.read().await.as_ref() {
            return Ok(token.clone());
        }

        self.login().await
    }

    async fn request<T>(&self, method: Method, path: &str) -> ProxyResult<T>
    where
        T: DeserializeOwned,
    {
        let token = self.auth_token().await?;
        let method_name = method.as_str().to_owned();
        let response = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(token)
            .send()
            .await?;

        Self::decode_response(response, method_name.as_str(), path).await
    }

    async fn request_with_body<B, T>(&self, method: Method, path: &str, body: &B) -> ProxyResult<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let token = self.auth_token().await?;
        let method_name = method.as_str().to_owned();
        let response = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(token)
            .json(body)
            .send()
            .await?;

        Self::decode_response(response, method_name.as_str(), path).await
    }

    async fn request_empty(&self, method: Method, path: &str) -> ProxyResult {
        let token = self.auth_token().await?;
        let method_name = method.as_str().to_owned();
        let response = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(token)
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        Err(Self::api_error(response, method_name.as_str(), path).await)
    }

    async fn decode_response<T>(
        response: reqwest::Response,
        method: &str,
        path: &str,
    ) -> ProxyResult<T>
    where
        T: DeserializeOwned,
    {
        if response.status().is_success() {
            return Ok(response.json::<T>().await?);
        }

        Err(Self::api_error(response, method, path).await)
    }

    async fn api_error(response: reqwest::Response, method: &str, path: &str) -> ProxyError {
        let status = response.status();
        let message = match response.text().await {
            Ok(text) => {
                logging::error(format_args!(
                    "NPM non-2xx response method={method} path={path} status={} body={text}",
                    status.as_u16()
                ));
                serde_json::from_str::<NpmErrorResponse>(&text)
                    .ok()
                    .and_then(|error| {
                        error
                            .message
                            .or_else(|| error.error.map(|body| body.message))
                    })
                    .unwrap_or(text)
            }
            Err(error) => error.to_string(),
        };

        ProxyError::Api {
            status: status.as_u16(),
            message,
        }
    }

    fn parse_id(id: &ProxyHostId) -> ProxyResult<u32> {
        id.0.parse()
            .map_err(|_| ProxyError::InvalidHostId(id.0.clone()))
    }

    async fn create_host(&self, host: &ProxyHost) -> ProxyResult<ProxyHost> {
        let host = self
            .request_with_body::<_, NpmProxyHost>(
                Method::POST,
                "/api/nginx/proxy-hosts",
                &NpmProxyHostRequest::from(host),
            )
            .await?;

        ProxyHost::try_from(host)
    }

    async fn update_host(&self, id: &ProxyHostId, host: &ProxyHost) -> ProxyResult<ProxyHost> {
        let id = Self::parse_id(id)?;
        let host = self
            .request_with_body::<_, NpmProxyHost>(
                Method::PUT,
                &format!("/api/nginx/proxy-hosts/{id}"),
                &NpmProxyHostRequest::from(host),
            )
            .await?;

        ProxyHost::try_from(host)
    }

    async fn delete_host(&self, id: &ProxyHostId) -> ProxyResult {
        let id = Self::parse_id(id)?;

        self.request_empty(Method::DELETE, &format!("/api/nginx/proxy-hosts/{id}"))
            .await
    }

    fn parse_certificate_id(id: &CertificateId) -> CertProviderResult<u32> {
        id.0.parse()
            .map_err(|_| CertProviderError::CertificateNotFound(id.0.clone()))
    }

    async fn create_certificate(
        &self,
        certificate: &Certificate,
    ) -> CertProviderResult<Certificate> {
        let request = NpmCertificateRequest::try_from(certificate)?;
        let certificate = self
            .request_with_body::<_, NpmCertificate>(
                Method::POST,
                "/api/nginx/certificates",
                &request,
            )
            .await?;

        Ok(certificate.into())
    }

    async fn renew_certificate(&self, id: &CertificateId) -> CertProviderResult {
        let id = Self::parse_certificate_id(id)?;
        let _: NpmCertificate = self
            .request(Method::POST, &format!("/api/nginx/certificates/{id}/renew"))
            .await?;

        Ok(())
    }

    async fn delete_certificate(&self, id: &CertificateId) -> CertProviderResult {
        let id = Self::parse_certificate_id(id)?;

        self.request_empty(Method::DELETE, &format!("/api/nginx/certificates/{id}"))
            .await?;

        Ok(())
    }

    pub async fn summary(&self) -> ProxyResult<NpmSummary> {
        let hosts = self
            .request::<Vec<NpmProxyHost>>(Method::GET, "/api/nginx/proxy-hosts")
            .await?;
        let certificates = self
            .request::<Vec<NpmCertificate>>(Method::GET, "/api/nginx/certificates")
            .await?;
        let proxy_managed = hosts.iter().filter(|host| host.is_managed()).count();
        let ssl_managed = certificates
            .iter()
            .filter(|certificate| {
                certificate.provider == "letsencrypt"
                    && certificate
                        .nice_name
                        .contains(MANAGED_CERTIFICATE_NAME_MARKER)
            })
            .count();
        Ok(NpmSummary {
            proxy_managed,
            proxy_unmanaged: hosts.len().saturating_sub(proxy_managed),
            ssl_managed,
            ssl_unmanaged: certificates.len().saturating_sub(ssl_managed),
        })
    }
}

#[async_trait]
impl ProxyProvider for NPMProxyProvider {
    async fn hosts(&self) -> ProxyResult<Vec<ProxyHost>> {
        let hosts = self
            .request::<Vec<NpmProxyHost>>(Method::GET, "/api/nginx/proxy-hosts")
            .await?;

        hosts
            .into_iter()
            .filter(NpmProxyHost::is_managed)
            .map(ProxyHost::try_from)
            .collect()
    }

    async fn apply(&self, change: ProxyChange) -> ProxyResult {
        match change {
            ProxyChange::Create(host) => self.create_host(&host).await.map(|_| ()),
            ProxyChange::Update { id, host } => self.update_host(&id, &host).await.map(|_| ()),
            ProxyChange::Delete { id } => self.delete_host(&id).await,
        }
    }
}

#[async_trait]
impl CertProvider for NPMProxyProvider {
    async fn certificates(&self) -> CertProviderResult<Vec<Certificate>> {
        let certificates = self
            .request::<Vec<NpmCertificate>>(Method::GET, "/api/nginx/certificates")
            .await?;

        Ok(certificates
            .into_iter()
            .filter(|certificate| certificate.provider == "letsencrypt")
            .map(Certificate::from)
            .collect())
    }

    async fn apply(&self, change: CertificateChange) -> CertProviderResult {
        match change {
            CertificateChange::Create(certificate) => {
                self.create_certificate(&certificate).await.map(|_| ())
            }
            CertificateChange::Renew { id } => self.renew_certificate(&id).await,
            CertificateChange::Delete { id } => self.delete_certificate(&id).await,
        }
    }
}
