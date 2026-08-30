
#[derive(Debug, Error)]
pub enum DnsProviderError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Cloudflare API error {code}: {message}")]
    Api {
        code: u32,
        message: String
    },

    #[error("DNS zone not found: {0}")]
    ZoneNotFound(String),

    #[error("unsupported record type: {0}")]
    UnsupportedRecordType(String),

    #[error("unknown error")]
    UnknownErorr
}

pub type DnsProviderResult <T = ()> = Result<T, DnsProviderError>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DnsRecordId(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsRecord {
    pub id: Option<DnsRecordId>,
    pub name: String,
    pub record_type: DnsRecordType,
    pub value: String,
    pub ttl: Option<u32>
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsRecordType {
    A,
    Aaaa,
    Cname,
    Txt,
    Mx
}

impl DnsRecordType {
    fn as_name(&self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Cname => "CNAME",
            Self::Txt => "TXT",
            Self::Mx => "MX"
        }
    }
}


pub enum DnsChange {
    Create(DnsRecord),
    Update {
        id: DnsRecordId,
        record: DnsRecord
    },
    Delete {
        id: DnsRecordId
    }
}

#[async_trait]
pub trait DnsProvider: Send + Sync {
    async fn records(
        &self,
        zone: &str,
    ) -> DnsProviderResult<Vec<DnsRecord>>;

    async fn apply(
        &self,
        zone: &str,
        change: DnsChange
    ) -> DnsProviderResult;
}
