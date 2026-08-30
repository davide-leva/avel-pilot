use std::{
    collections::{BTreeMap, HashSet},
    net::IpAddr,
};

use thiserror::Error;

use crate::{
    cert_provider::{
        CertProvider, CertProviderError, Certificate, CertificateChallenge, CertificateChange,
        CertificateId, CertificateMeta, Dns01Challenge, MANAGED_CERTIFICATE_NAME_MARKER,
    },
    config::{Config, DnsConfig, ProxyConfig, ServiceConfig, ServicesFile, UpstreamSchemeConfig},
    dns_provider::{
        Dns01ProviderConfig, DnsChange, DnsProvider, DnsProviderError, DnsRecord, DnsRecordType,
    },
    proxy_provider::{
        CertificateRef, ProxyChange, ProxyError, ProxyHost, ProxyProvider, Upstream, UpstreamScheme,
    },
};

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("dns provider error: {0}")]
    Dns(#[from] DnsProviderError),

    #[error("proxy provider error: {0}")]
    Proxy(#[from] ProxyError),

    #[error("certificate provider error: {0}")]
    Certificate(#[from] CertProviderError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredState {
    pub dns_records: Vec<DnsRecord>,
    pub proxy_hosts: Vec<ProxyHost>,
    pub certificates: Vec<Certificate>,
}

pub async fn reconcile(
    config: &Config,
    services: &ServicesFile,
    dns_provider: &impl DnsProvider,
    proxy_provider: &impl ProxyProvider,
    cert_provider: &impl CertProvider,
) -> Result<(), ReconcileError> {
    validate(config, services)?;

    let zone = zone(config);
    let dns01 = dns01_provider_config(config, services, dns_provider)?;
    let certificate_id = ensure_wildcard_certificate(zone, services, dns01, cert_provider).await?;
    let desired = build_desired_state(config, services, certificate_id)?;

    let actual_dns = dns_provider.records(zone).await?;
    for change in diff_dns_records(&desired.dns_records, &actual_dns) {
        dns_provider.apply(zone, change).await?;
    }

    let actual_proxy = proxy_provider.hosts().await?;
    for change in diff_proxy_hosts(&desired.proxy_hosts, &actual_proxy) {
        proxy_provider.apply(change).await?;
    }

    Ok(())
}

pub fn build_desired_state(
    config: &Config,
    services: &ServicesFile,
    wildcard_certificate_id: Option<CertificateId>,
) -> Result<DesiredState, ReconcileError> {
    validate(config, services)?;

    let zone = zone(config);
    let proxy_host = proxy_host(config);
    let dns_record_type = dns_record_type(proxy_host);
    let certificate = wildcard_certificate_id
        .as_ref()
        .map(certificate_ref)
        .transpose()?;

    let dns_records = services
        .services
        .values()
        .map(|service| DnsRecord {
            id: None,
            name: service.domain.clone(),
            record_type: dns_record_type.clone(),
            value: proxy_host.to_owned(),
            ttl: None,
        })
        .collect();

    let proxy_hosts = services
        .services
        .values()
        .map(|service| ProxyHost {
            id: None,
            domains: vec![service.domain.clone()],
            upstream: Upstream {
                scheme: upstream_scheme(&service.upstream.scheme),
                host: service.upstream.host.clone(),
                port: service.upstream.port,
            },
            websocket: service.websocket,
            certificate: service.tls.then(|| certificate.clone()).flatten(),
            force_https: service.tls,
        })
        .collect();

    let certificates = services
        .services
        .values()
        .any(|service| service.tls)
        .then(|| wildcard_certificate(zone, None, None))
        .into_iter()
        .collect();

    Ok(DesiredState {
        dns_records,
        proxy_hosts,
        certificates,
    })
}

pub fn diff_dns_records(desired: &[DnsRecord], actual: &[DnsRecord]) -> Vec<DnsChange> {
    let desired_by_key = desired
        .iter()
        .map(|record| (dns_record_key(record), record))
        .collect::<BTreeMap<_, _>>();
    let actual_by_key = actual
        .iter()
        .map(|record| (dns_record_key(record), record))
        .collect::<BTreeMap<_, _>>();

    let mut changes = Vec::new();

    for (key, desired) in &desired_by_key {
        match actual_by_key.get(key) {
            Some(actual) if !same_dns_record(desired, actual) => {
                if let Some(id) = actual.id.clone() {
                    changes.push(DnsChange::Update {
                        id,
                        record: (*desired).clone(),
                    });
                }
            }
            Some(_) => {}
            None => changes.push(DnsChange::Create((*desired).clone())),
        }
    }

    for (key, actual) in &actual_by_key {
        if !desired_by_key.contains_key(key) {
            if let Some(id) = actual.id.clone() {
                changes.push(DnsChange::Delete { id });
            }
        }
    }

    changes
}

pub fn diff_proxy_hosts(desired: &[ProxyHost], actual: &[ProxyHost]) -> Vec<ProxyChange> {
    let desired_by_domain = desired
        .iter()
        .filter_map(|host| primary_domain(host).map(|domain| (domain.to_owned(), host)))
        .collect::<BTreeMap<_, _>>();
    let actual_by_domain = actual
        .iter()
        .filter_map(|host| primary_domain(host).map(|domain| (domain.to_owned(), host)))
        .collect::<BTreeMap<_, _>>();

    let mut changes = Vec::new();

    for (domain, desired) in &desired_by_domain {
        match actual_by_domain.get(domain) {
            Some(actual) if !same_proxy_host(desired, actual) => {
                if let Some(id) = actual.id.clone() {
                    changes.push(ProxyChange::Update {
                        id,
                        host: (*desired).clone(),
                    });
                }
            }
            Some(_) => {}
            None => changes.push(ProxyChange::Create((*desired).clone())),
        }
    }

    for (domain, actual) in &actual_by_domain {
        if !desired_by_domain.contains_key(domain) {
            if let Some(id) = actual.id.clone() {
                changes.push(ProxyChange::Delete { id });
            }
        }
    }

    changes
}

pub fn diff_certificates(
    desired: &[Certificate],
    actual: &[Certificate],
) -> Vec<CertificateChange> {
    let actual_domain_sets = actual
        .iter()
        .map(|certificate| normalized_domain_set(&certificate.domains))
        .collect::<Vec<_>>();

    desired
        .iter()
        .filter(|certificate| {
            let desired_domains = normalized_domain_set(&certificate.domains);
            !actual_domain_sets.contains(&desired_domains)
        })
        .cloned()
        .map(CertificateChange::Create)
        .collect()
}

fn validate(config: &Config, services: &ServicesFile) -> Result<(), ReconcileError> {
    if proxy_host(config).trim().is_empty() {
        return Err(ReconcileError::InvalidConfig(
            "proxy.host is empty".to_owned(),
        ));
    }

    let zone = zone(config);
    let mut domains = HashSet::new();

    for (name, service) in &services.services {
        validate_service(name, service, zone, &mut domains)?;
    }

    Ok(())
}

fn validate_service(
    name: &str,
    service: &ServiceConfig,
    zone: &str,
    domains: &mut HashSet<String>,
) -> Result<(), ReconcileError> {
    if service.domain.trim().is_empty() {
        return Err(ReconcileError::InvalidConfig(format!(
            "service {name} has empty domain"
        )));
    }

    if service.upstream.host.trim().is_empty() {
        return Err(ReconcileError::InvalidConfig(format!(
            "service {name} has empty upstream host"
        )));
    }

    if !domain_in_zone(&service.domain, zone) {
        return Err(ReconcileError::InvalidConfig(format!(
            "service {name} domain {} is outside zone {zone}",
            service.domain
        )));
    }

    if !domains.insert(service.domain.clone()) {
        return Err(ReconcileError::InvalidConfig(format!(
            "duplicate service domain {}",
            service.domain
        )));
    }

    Ok(())
}

async fn ensure_wildcard_certificate(
    zone: &str,
    services: &ServicesFile,
    dns01: Option<Dns01ProviderConfig>,
    cert_provider: &impl CertProvider,
) -> Result<Option<CertificateId>, ReconcileError> {
    if !services.services.values().any(|service| service.tls) {
        return Ok(None);
    }

    let dns01 = dns01.ok_or_else(|| {
        ReconcileError::InvalidConfig("DNS provider does not support DNS-01".to_owned())
    })?;

    let wildcard_domain = wildcard_domain(zone);
    let actual = cert_provider.certificates().await?;
    let existing = find_certificate_id(&actual, &[wildcard_domain.clone()]);
    if let Some(id) = existing {
        return Ok(Some(id));
    }

    let certificate = wildcard_certificate(zone, Some(dns01.clone()), None);
    for change in diff_certificates(&[certificate], &actual) {
        cert_provider.apply(change).await?;
    }

    let actual = cert_provider.certificates().await?;
    find_certificate_id(&actual, &[wildcard_domain.clone()])
        .ok_or_else(|| {
            ReconcileError::InvalidConfig(format!(
                "created wildcard certificate was not found for {wildcard_domain}"
            ))
        })
        .map(Some)
}

fn dns01_provider_config(
    config: &Config,
    services: &ServicesFile,
    dns_provider: &impl DnsProvider,
) -> Result<Option<Dns01ProviderConfig>, ReconcileError> {
    if !services.services.values().any(|service| service.tls) {
        return Ok(None);
    }

    let mut dns01 = dns_provider.dns01_provider_config().ok_or_else(|| {
        ReconcileError::InvalidConfig("DNS provider does not support DNS-01".to_owned())
    })?;

    match &config.dns {
        DnsConfig::Cloudflare {
            propagation_seconds,
            ..
        } => {
            dns01.propagation_seconds = *propagation_seconds;
        }
    }

    Ok(Some(dns01))
}

fn find_certificate_id(certificates: &[Certificate], domains: &[String]) -> Option<CertificateId> {
    let desired = normalized_domain_set(domains);

    certificates
        .iter()
        .find(|certificate| normalized_domain_set(&certificate.domains) == desired)
        .and_then(|certificate| certificate.id.clone())
}

fn wildcard_certificate(
    zone: &str,
    dns01: Option<Dns01ProviderConfig>,
    id: Option<CertificateId>,
) -> Certificate {
    Certificate {
        id,
        name: Some(format!(
            "{MANAGED_CERTIFICATE_NAME_MARKER}:{}",
            wildcard_domain(zone)
        )),
        meta: CertificateMeta { managed: true },
        domains: vec![wildcard_domain(zone)],
        challenge: CertificateChallenge::Dns01(Dns01Challenge {
            provider: dns01
                .as_ref()
                .map(|config| config.provider.clone())
                .unwrap_or_default(),
            credentials: dns01
                .as_ref()
                .map(|config| config.credentials.clone())
                .unwrap_or_default(),
            propagation_seconds: dns01.and_then(|config| config.propagation_seconds),
        }),
        expires_at: None,
    }
}

fn dns_record_key(record: &DnsRecord) -> (String, DnsRecordType) {
    (record.name.clone(), record.record_type.clone())
}

fn same_dns_record(left: &DnsRecord, right: &DnsRecord) -> bool {
    left.name == right.name
        && left.record_type == right.record_type
        && left.value == right.value
        && left.ttl == right.ttl
}

fn same_proxy_host(left: &ProxyHost, right: &ProxyHost) -> bool {
    left.domains == right.domains
        && left.upstream == right.upstream
        && left.websocket == right.websocket
        && left.certificate == right.certificate
        && left.force_https == right.force_https
}

fn primary_domain(host: &ProxyHost) -> Option<&str> {
    host.domains.first().map(String::as_str)
}

fn normalized_domain_set(domains: &[String]) -> HashSet<String> {
    domains.iter().map(|domain| domain.to_lowercase()).collect()
}

fn wildcard_domain(zone: &str) -> String {
    format!("*.{zone}")
}

fn domain_in_zone(domain: &str, zone: &str) -> bool {
    domain == zone || domain.ends_with(&format!(".{zone}"))
}

fn certificate_ref(id: &CertificateId) -> Result<CertificateRef, ReconcileError> {
    let id = id.0.parse::<u32>().map_err(|_| {
        ReconcileError::InvalidConfig(format!("invalid NPM certificate id {}", id.0))
    })?;

    Ok(CertificateRef { id })
}

fn dns_record_type(proxy_host: &str) -> DnsRecordType {
    match proxy_host.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => DnsRecordType::A,
        Ok(IpAddr::V6(_)) => DnsRecordType::Aaaa,
        Err(_) => DnsRecordType::Cname,
    }
}

fn upstream_scheme(scheme: &UpstreamSchemeConfig) -> UpstreamScheme {
    match scheme {
        UpstreamSchemeConfig::Http => UpstreamScheme::Http,
        UpstreamSchemeConfig::Https => UpstreamScheme::Https,
    }
}

fn zone(config: &Config) -> &str {
    match &config.dns {
        DnsConfig::Cloudflare { zone, .. } => zone,
    }
}

fn proxy_host(config: &Config) -> &str {
    match &config.proxy {
        ProxyConfig::Npm { host, .. } => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CertificateConfig, UpstreamConfig};

    #[test]
    fn desired_state_prefers_one_wildcard_certificate_for_tls_services() {
        let config = test_config("10.0.5.104");
        let services = test_services([
            ("jellyfin", "tv.avel.space", true),
            ("home", "home.avel.space", true),
        ]);

        let desired = build_desired_state(&config, &services, Some(CertificateId("7".to_owned())))
            .expect("desired state");

        assert_eq!(desired.certificates.len(), 1);
        assert_eq!(desired.certificates[0].domains, vec!["*.avel.space"]);
        assert!(desired.proxy_hosts.iter().all(|host| {
            host.certificate.as_ref().map(|certificate| certificate.id) == Some(7)
        }));
    }

    #[test]
    fn dns_diff_updates_creates_and_deletes_by_name_and_type() {
        let desired = vec![
            dns_record(None, "tv.avel.space", DnsRecordType::A, "10.0.5.104"),
            dns_record(None, "home.avel.space", DnsRecordType::A, "10.0.5.104"),
        ];
        let actual = vec![
            dns_record(Some("1"), "tv.avel.space", DnsRecordType::A, "10.0.5.105"),
            dns_record(Some("2"), "old.avel.space", DnsRecordType::A, "10.0.5.104"),
        ];

        let changes = diff_dns_records(&desired, &actual);

        assert_eq!(changes.len(), 3);
        assert!(matches!(changes[0], DnsChange::Create(_)));
        assert!(matches!(changes[1], DnsChange::Update { .. }));
        assert!(matches!(changes[2], DnsChange::Delete { .. }));
    }

    #[test]
    fn proxy_diff_is_idempotent_when_hosts_match() {
        let host = proxy_record(Some("4"), "tv.avel.space", Some(7));
        let desired = vec![proxy_record(None, "tv.avel.space", Some(7))];
        let actual = vec![host];

        assert!(diff_proxy_hosts(&desired, &actual).is_empty());
    }

    #[test]
    fn certificate_diff_creates_missing_wildcard_without_deleting_others() {
        let desired = vec![wildcard_certificate("avel.space", None, None)];
        let actual = vec![wildcard_certificate(
            "example.com",
            None,
            Some(CertificateId("9".to_owned())),
        )];

        let changes = diff_certificates(&desired, &actual);

        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], CertificateChange::Create(_)));
    }

    #[test]
    fn validation_rejects_domains_outside_zone() {
        let config = test_config("10.0.5.104");
        let services = test_services([("bad", "bad.example.com", false)]);

        assert!(build_desired_state(&config, &services, None).is_err());
    }

    #[test]
    fn validation_rejects_domain_that_only_shares_zone_suffix() {
        let config = test_config("10.0.5.104");
        let services = test_services([("bad", "badavel.space", false)]);

        assert!(build_desired_state(&config, &services, None).is_err());
    }

    fn test_config(proxy_host: &str) -> Config {
        Config {
            dns: DnsConfig::Cloudflare {
                zone: "avel.space".to_owned(),
                api_token: "token".to_owned(),
                propagation_seconds: Some(120),
            },
            proxy: ProxyConfig::Npm {
                host: proxy_host.to_owned(),
                url: "http://localhost:81".to_owned(),
                identity: "admin@example.com".to_owned(),
                secret: "secret".to_owned(),
            },
            certificates: CertificateConfig::default(),
        }
    }

    fn test_services<const N: usize>(items: [(&str, &str, bool); N]) -> ServicesFile {
        ServicesFile {
            services: items
                .into_iter()
                .map(|(name, domain, tls)| {
                    (
                        name.to_owned(),
                        ServiceConfig {
                            domain: domain.to_owned(),
                            upstream: UpstreamConfig {
                                host: "10.0.5.100".to_owned(),
                                port: 8000,
                                scheme: UpstreamSchemeConfig::Http,
                            },
                            tls,
                            websocket: true,
                        },
                    )
                })
                .collect(),
        }
    }

    fn dns_record(
        id: Option<&str>,
        name: &str,
        record_type: DnsRecordType,
        value: &str,
    ) -> DnsRecord {
        DnsRecord {
            id: id.map(|id| crate::dns_provider::DnsRecordId(id.to_owned())),
            name: name.to_owned(),
            record_type,
            value: value.to_owned(),
            ttl: None,
        }
    }

    fn proxy_record(id: Option<&str>, domain: &str, certificate_id: Option<u32>) -> ProxyHost {
        ProxyHost {
            id: id.map(|id| crate::proxy_provider::ProxyHostId(id.to_owned())),
            domains: vec![domain.to_owned()],
            upstream: Upstream {
                scheme: UpstreamScheme::Http,
                host: "10.0.5.100".to_owned(),
                port: 8000,
            },
            websocket: true,
            certificate: certificate_id.map(|id| CertificateRef { id }),
            force_https: certificate_id.is_some(),
        }
    }
}
