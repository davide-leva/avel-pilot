use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use reqwest::{Client, Method, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::sync::RwLock;

use crate::{
    dns_provider::{
        Dns01ProviderConfig, DnsChange, DnsProvider, DnsProviderError, DnsProviderResult,
        DnsRecord, DnsRecordId, DnsRecordType,
    },
    logging,
};

const CLOUDFLARE_API_URL: &str = "https://api.cloudflare.com/client/v4";
const MANAGED_COMMENT: &str = "managed-by:avel-pilot";

#[derive(Debug, Deserialize)]
struct CloudflareResponse<T> {
    success: bool,
    result: T,

    #[serde(default)]
    errors: Vec<CloudflareApiError>,
}

#[derive(Debug, Deserialize)]
struct CloudflareApiError {
    code: u32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct CloudflareZone {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct CloudflareRecord {
    id: String,
    name: String,

    #[serde(rename = "type")]
    record_type: String,

    content: String,
    ttl: u32,

    #[serde(default)]
    comment: Option<String>,
}

#[derive(Debug, Serialize)]
struct CloudflareRecordRequest<'a> {
    #[serde(rename = "type")]
    record_type: &'a str,

    name: &'a str,
    content: &'a str,
    ttl: u32,
    comment: &'a str,
}

impl<'a> From<&'a DnsRecord> for CloudflareRecordRequest<'a> {
    fn from(record: &'a DnsRecord) -> Self {
        Self {
            record_type: record.record_type.as_name(),
            name: &record.name,
            content: &record.value,
            ttl: record.ttl.unwrap_or(1),
            comment: MANAGED_COMMENT,
        }
    }
}

impl CloudflareRecord {
    fn is_managed(&self) -> bool {
        self.comment
            .as_deref()
            .map(|comment| comment.contains(MANAGED_COMMENT))
            .unwrap_or(false)
    }
}

impl TryFrom<CloudflareRecord> for DnsRecord {
    type Error = DnsProviderError;

    fn try_from(record: CloudflareRecord) -> Result<Self, Self::Error> {
        let record_type = match record.record_type.as_str() {
            "A" => DnsRecordType::A,
            "AAAA" => DnsRecordType::Aaaa,
            "CNAME" => DnsRecordType::Cname,
            "TXT" => DnsRecordType::Txt,
            "MX" => DnsRecordType::Mx,

            other => return Err(DnsProviderError::UnsupportedRecordType(other.to_owned())),
        };

        Ok(Self {
            id: Some(super::DnsRecordId(record.id)),
            name: record.name,
            record_type,
            value: record.content,

            // Cloudflare: 1 == automatic
            ttl: (record.ttl != 1).then_some(record.ttl),
        })
    }
}

pub struct CloudflareDnsProvider {
    client: Client,
    api_token: String,

    zone_cache: Arc<RwLock<HashMap<String, String>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudflareDnsSummary {
    pub managed: usize,
    pub unmanaged: usize,
}

impl CloudflareDnsProvider {
    pub fn new(api_token: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            api_token: api_token.into(),
            zone_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl CloudflareDnsProvider {
    async fn request<T>(&self, method: Method, path: &str) -> DnsProviderResult<T>
    where
        T: DeserializeOwned,
    {
        let method_name = method.as_str().to_owned();
        let path = path.to_owned();
        let response = self
            .client
            .request(method, format!("{CLOUDFLARE_API_URL}{path}"))
            .bearer_auth(&self.api_token)
            .send()
            .await?;

        let response = Self::decode_response(response, &method_name, &path).await?;

        if !response.success {
            let error = response
                .errors
                .into_iter()
                .next()
                .unwrap_or(CloudflareApiError {
                    code: 0,
                    message: "Unknown Cloudflare API error".to_owned(),
                });

            return Err(DnsProviderError::Api {
                code: error.code,
                message: error.message,
            });
        }

        Ok(response.result)
    }

    async fn request_with_body<B, T>(
        &self,
        method: Method,
        path: &str,
        body: &B,
    ) -> DnsProviderResult<T>
    where
        B: Serialize + ?Sized,
        T: DeserializeOwned,
    {
        let method_name = method.as_str().to_owned();
        let path = path.to_owned();
        let response = self
            .client
            .request(method, format!("{CLOUDFLARE_API_URL}{path}"))
            .bearer_auth(&self.api_token)
            .json(body)
            .send()
            .await?;

        let response = Self::decode_response(response, &method_name, &path).await?;

        if !response.success {
            let error = response
                .errors
                .into_iter()
                .next()
                .unwrap_or(CloudflareApiError {
                    code: 0,
                    message: "Unknown Cloudflare API error".to_owned(),
                });

            return Err(DnsProviderError::Api {
                code: error.code,
                message: error.message,
            });
        }

        Ok(response.result)
    }

    async fn decode_response<T>(
        response: Response,
        method: &str,
        path: &str,
    ) -> DnsProviderResult<CloudflareResponse<T>>
    where
        T: DeserializeOwned,
    {
        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            logging::error(format_args!(
                "Cloudflare non-2xx response method={method} path={path} status={} body={body}",
                status.as_u16()
            ));
            return Err(DnsProviderError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }

        serde_json::from_str(&body).map_err(DnsProviderError::Json)
    }
}

impl CloudflareDnsProvider {
    async fn resolve_zone_id(&self, zone: &str) -> DnsProviderResult<String> {
        {
            let cache = self.zone_cache.read().await;

            if let Some(id) = cache.get(zone) {
                return Ok(id.clone());
            }
        }

        let zones = self
            .client
            .get(format!("{CLOUDFLARE_API_URL}/zones"))
            .bearer_auth(&self.api_token)
            .query(&[("name", &zone)])
            .send()
            .await?;

        let zones: CloudflareResponse<Vec<CloudflareZone>> =
            Self::decode_response(zones, "GET", "/zones?name=<zone>").await?;

        if !zones.success {
            let error = zones
                .errors
                .into_iter()
                .next()
                .unwrap_or(CloudflareApiError {
                    code: 0,
                    message: "Unknown Cloudflare API error".to_owned(),
                });

            return Err(DnsProviderError::Api {
                code: error.code,
                message: error.message,
            });
        }

        let zone_id = zones
            .result
            .into_iter()
            .find(|z| z.name == zone)
            .map(|z| z.id)
            .ok_or_else(|| DnsProviderError::ZoneNotFound(zone.to_string()))?;

        self.zone_cache
            .write()
            .await
            .insert(zone.to_string(), zone_id.clone());

        Ok(zone_id)
    }
}

impl CloudflareDnsProvider {
    // TODO: Use a better pagination strategy
    async fn list_raw_records(&self, zone_id: &str) -> DnsProviderResult<Vec<CloudflareRecord>> {
        self.request(
            Method::GET,
            &format!("/zones/{zone_id}/dns_records?per_page=5000"),
        )
        .await
    }

    async fn list_records(&self, zone_id: &str) -> DnsProviderResult<Vec<DnsRecord>> {
        let records: Vec<CloudflareRecord> = self.list_raw_records(zone_id).await?;

        records
            .into_iter()
            .filter(CloudflareRecord::is_managed)
            .map(DnsRecord::try_from)
            .collect()
    }

    async fn create_record(
        &self,
        zone_id: &str,
        record: &DnsRecord,
    ) -> Result<(), DnsProviderError> {
        let request = CloudflareRecordRequest::from(record);

        self.request_with_body::<_, CloudflareRecord>(
            Method::POST,
            &format!("/zones/{zone_id}/dns_records"),
            &request,
        )
        .await?;

        Ok(())
    }

    async fn update_record(
        &self,
        zone_id: &str,
        id: &DnsRecordId,
        record: &DnsRecord,
    ) -> Result<(), DnsProviderError> {
        let request = CloudflareRecordRequest::from(record);

        self.request_with_body::<_, CloudflareRecord>(
            Method::PATCH,
            &format!("/zones/{zone_id}/dns_records/{}", id.0),
            &request,
        )
        .await?;

        Ok(())
    }

    async fn delete_record(&self, zone_id: &str, id: &DnsRecordId) -> Result<(), DnsProviderError> {
        let _: serde_json::Value = self
            .request(
                Method::DELETE,
                &format!("/zones/{zone_id}/dns_records/{}", id.0),
            )
            .await?;

        Ok(())
    }

    pub async fn summary(&self, zone: &str) -> DnsProviderResult<CloudflareDnsSummary> {
        let zone_id = self.resolve_zone_id(zone).await?;
        let records = self.list_raw_records(&zone_id).await?;
        let managed = records.iter().filter(|record| record.is_managed()).count();

        Ok(CloudflareDnsSummary {
            managed,
            unmanaged: records.len().saturating_sub(managed),
        })
    }
}

#[async_trait]
impl DnsProvider for CloudflareDnsProvider {
    fn dns01_provider_config(&self) -> Option<Dns01ProviderConfig> {
        Some(Dns01ProviderConfig {
            provider: "cloudflare".to_owned(),
            credentials: format!("dns_cloudflare_api_token={}", self.api_token),
            propagation_seconds: None,
        })
    }

    async fn records(&self, zone: &str) -> Result<Vec<DnsRecord>, DnsProviderError> {
        let zone_id = self.resolve_zone_id(zone).await?;

        self.list_records(&zone_id).await
    }

    async fn apply(&self, zone: &str, change: DnsChange) -> Result<(), DnsProviderError> {
        let zone_id = self.resolve_zone_id(zone).await?;

        match change {
            DnsChange::Create(record) => {
                self.create_record(&zone_id, &record).await?;
            }

            DnsChange::Update { id, record } => {
                self.update_record(&zone_id, &id, &record).await?;
            }

            DnsChange::Delete { id } => {
                self.delete_record(&zone_id, &id).await?;
            }
        }

        Ok(())
    }
}
