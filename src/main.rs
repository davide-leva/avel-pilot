use std::{
    fs,
    io::{self, Write},
    path::Path,
    time::{Duration, SystemTime},
};

use avel_pilot::{
    config::{
        CertificateConfig, Config, DnsConfig, ProxyConfig, ServicesFile, load_app_files, load_yaml,
    },
    dns_provider::cloudflare::CloudflareDnsProvider,
    logging,
    proxy_provider::npm::NPMProxyProvider,
    reconciler::reconcile,
};
use tokio::time;

const DEFAULT_CONFIG_PATH: &str = "/etc/avel-pilot/config.yml";
const DEFAULT_SERVICES_PATH: &str = "/etc/avel-pilot/services.yml";
const WATCH_INTERVAL: Duration = Duration::from_secs(2);
const SYSTEM_CONFIG_PATH: &str = "/etc/avel-pilot/config.yml";
const SYSTEM_SERVICES_PATH: &str = "/etc/avel-pilot/services.yml";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("init") {
        init_config()?;
        return Ok(());
    }

    let config_path =
        std::env::var("AVEL_PILOT_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG_PATH.to_owned());
    let services_path =
        std::env::var("AVEL_PILOT_SERVICES").unwrap_or_else(|_| DEFAULT_SERVICES_PATH.to_owned());
    logging::info(format_args!(
        "Using config_path={config_path} services_path={services_path}"
    ));

    logging::timed(format!("check config file {config_path}"), || {
        ensure_runtime_file(&config_path, "config")
    })?;
    logging::timed(format!("check services file {services_path}"), || {
        ensure_runtime_file(&services_path, "services")
    })?;

    let files = logging::timed(
        format!("load app files from {config_path} and {services_path}"),
        || load_app_files(&config_path, &services_path),
    )?;
    let config = files.config;
    let mut services = files.services;
    let mut services_modified_at =
        logging::timed(format!("read modified time for {services_path}"), || {
            modified_at(&services_path)
        })?;

    run_reconcile(&config, &services).await;
    logging::info(format_args!("Watching {services_path} for changes"));

    let mut interval = time::interval(WATCH_INTERVAL);
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                logging::info("Shutting down");
                break;
            }
            _ = interval.tick() => {
                let modified_at = logging::timed(format!("read modified time for {services_path}"), || {
                    modified_at(&services_path)
                })?;
                if modified_at <= services_modified_at {
                    continue;
                }

                services_modified_at = modified_at;
                match logging::timed(format!("reload services from {services_path}"), || {
                    load_yaml::<ServicesFile>(&services_path)
                }) {
                    Ok(next_services) => {
                        services = next_services;
                        run_reconcile(&config, &services).await;
                    }
                    Err(error) => {
                        logging::error(format_args!("Failed to reload {services_path}: {error}"));
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_reconcile(config: &Config, services: &ServicesFile) {
    let (dns_provider, proxy_provider) =
        match logging::timed("initialize providers", || providers(config)) {
            Ok(providers) => providers,
            Err(error) => {
                logging::error(format_args!("Failed to initialize providers: {error}"));
                return;
            }
        };

    match logging::timed_async(
        format!("reconcile {} services", services.services.len()),
        reconcile(
            config,
            services,
            &dns_provider,
            &proxy_provider,
            &proxy_provider,
        ),
    )
    .await
    {
        Ok(()) => logging::info(format_args!(
            "Reconcile completed for {} services",
            services.services.len()
        )),
        Err(error) => logging::error(format_args!("Reconcile failed: {error}")),
    }
}

fn providers(
    config: &Config,
) -> Result<(CloudflareDnsProvider, NPMProxyProvider), Box<dyn std::error::Error>> {
    let dns_provider = match &config.dns {
        DnsConfig::Cloudflare { api_token, .. } => CloudflareDnsProvider::new(api_token.clone()),
    };

    let proxy_provider = match &config.proxy {
        ProxyConfig::Npm {
            url,
            identity,
            secret,
            ..
        } => NPMProxyProvider::new(url.clone(), identity.clone(), secret.clone()),
    };

    Ok((dns_provider, proxy_provider))
}

fn ensure_runtime_file(path: &str, kind: &str) -> Result<(), Box<dyn std::error::Error>> {
    if Path::new(path).exists() {
        return Ok(());
    }

    Err(format!("{kind} file not found at {path}. Run `sudo avel-pilot init` first.").into())
}

fn init_config() -> Result<(), Box<dyn std::error::Error>> {
    logging::info(format_args!(
        "Creating {SYSTEM_CONFIG_PATH} and {SYSTEM_SERVICES_PATH}"
    ));

    let config_path = Path::new(SYSTEM_CONFIG_PATH);
    let services_path = Path::new(SYSTEM_SERVICES_PATH);
    if config_path.exists() && !confirm("Config already exists. Overwrite?", false)? {
        logging::warn("Aborted");
        return Ok(());
    }
    if services_path.exists() && !confirm("Services file already exists. Overwrite?", false)? {
        logging::warn("Aborted");
        return Ok(());
    }

    let zone = prompt_required("Cloudflare zone", None)?;
    let cloudflare_token = prompt_required("Cloudflare API token", None)?;
    let propagation_seconds = prompt_required("DNS propagation seconds", Some("120"))?
        .parse::<u32>()
        .map_err(|_| "DNS propagation seconds must be a number")?;
    let npm_url = prompt_required("Nginx Proxy Manager URL", Some("http://localhost:81"))?;
    let proxy_host = prompt_required("Public proxy host for DNS records", None)?;
    let npm_identity = prompt_required("NPM identity", None)?;
    let npm_secret = prompt_required("NPM secret", None)?;

    let config = Config {
        dns: DnsConfig::Cloudflare {
            zone: zone.clone(),
            api_token: cloudflare_token,
            propagation_seconds: Some(propagation_seconds),
        },
        proxy: ProxyConfig::Npm {
            host: proxy_host,
            url: npm_url,
            identity: npm_identity,
            secret: npm_secret,
        },
        certificates: CertificateConfig::default(),
    };
    let content = serde_yaml::to_string(&config)?;

    if let Some(parent) = config_path.parent() {
        logging::timed(
            format!("create config directory {}", parent.display()),
            || fs::create_dir_all(parent),
        )?;
    }

    logging::timed(format!("write {SYSTEM_CONFIG_PATH}"), || {
        fs::write(config_path, content)
    })?;
    logging::timed(format!("write {SYSTEM_SERVICES_PATH}"), || {
        fs::write(services_path, services_example(&zone))
    })?;
    logging::timed(
        format!("set private permissions on {SYSTEM_CONFIG_PATH}"),
        || set_private_permissions(config_path),
    )?;
    logging::info(format_args!("Wrote {SYSTEM_CONFIG_PATH}"));
    logging::info(format_args!("Wrote {SYSTEM_SERVICES_PATH}"));

    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;

    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

fn services_example(zone: &str) -> String {
    format!(
        "\
# Declare one entry for each service Avel Pilot should expose.
# Uncomment and edit the example below.
#
# services:
#   jellyfin:
#     domain: tv.{zone}
#     upstream:
#       scheme: http
#       host: 10.0.5.101
#       port: 8096
#     tls: true
#     websocket: true
services: {{}}
"
    )
}

fn prompt_required(
    label: &str,
    default: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    loop {
        let value = prompt(label, default)?;
        if !value.trim().is_empty() {
            return Ok(value);
        }

        logging::error(format_args!("{label} is required"));
    }
}

fn prompt(label: &str, default: Option<&str>) -> Result<String, io::Error> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    Ok(if input.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        input.to_owned()
    })
}

fn confirm(label: &str, default: bool) -> Result<bool, io::Error> {
    let default_hint = if default { "Y/n" } else { "y/N" };
    print!("{label} [{default_hint}]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(match input.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

fn modified_at(path: impl AsRef<Path>) -> Result<SystemTime, std::io::Error> {
    fs::metadata(path)?.modified()
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<(), Box<dyn std::error::Error>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;

    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }

    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<(), Box<dyn std::error::Error>> {
    tokio::signal::ctrl_c().await?;

    Ok(())
}
