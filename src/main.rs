use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use avel_pilot::{
    cert_provider::CertProvider,
    config::{
        CertificateConfig, Config, DnsConfig, ProxyConfig, ServiceConfig, ServicesFile,
        UpstreamConfig, UpstreamSchemeConfig, load_app_files,
    },
    dns_provider::{DnsChange, cloudflare::CloudflareDnsProvider},
    logging,
    proxy_provider::{ProxyChange, npm::NPMProxyProvider},
    reconciler::{
        ReconcilePlan, build_desired_state, describe_dns_change, describe_proxy_change,
        diff_certificates, existing_wildcard_certificate_id, plan_reconcile, reconcile, validate,
        zone,
    },
};
use clap::{Parser, Subcommand};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde::Deserialize;

const DEFAULT_CONFIG_PATH: &str = "~/.config/avel-pilot/config.yml";
const DEFAULT_SERVICES_PATH: &str = "~/.config/avel-pilot/services.yml";
const GITHUB_REPO: &str = "davide-leva/avel-pilot";
const UPDATE_ASSET: &str = "avel-pilot-linux-amd64";

#[derive(Debug, Parser)]
#[command(
    name = "avel-pilot",
    version,
    about = "Manage Cloudflare DNS and Nginx Proxy Manager hosts from YAML.",
    after_help = "Every command except update checks GitHub for a newer Avel Pilot release."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "AVEL_PILOT_CONFIG",
        default_value = DEFAULT_CONFIG_PATH,
        help = "Path to config.yml"
    )]
    config: String,

    #[arg(
        long,
        global = true,
        env = "AVEL_PILOT_SERVICES",
        default_value = DEFAULT_SERVICES_PATH,
        help = "Path to services.yml"
    )]
    services: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(about = "Create config and services files interactively.")]
    Init,

    #[command(about = "Show a managed and unmanaged Cloudflare/NPM resource table.")]
    Status,

    #[command(about = "Show the changes Avel Pilot would apply.")]
    Diff,

    #[command(about = "Validate config.yml and services.yml without contacting providers.")]
    Validate,

    #[command(about = "Download and install the latest GitHub release binary.")]
    Update,

    #[command(about = "Apply the desired state once and exit.")]
    Apply,

    #[command(about = "Manage services.yml entries interactively.")]
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
}

#[derive(Debug, Subcommand)]
enum ServiceCommands {
    #[command(about = "List services declared in services.yml.")]
    List,

    #[command(about = "Add a service to services.yml with interactive prompts.")]
    Add {
        #[arg(help = "Service name to add")]
        name: Option<String>,
    },

    #[command(about = "Remove a service from services.yml with confirmation.")]
    Remove {
        #[arg(help = "Service name to remove")]
        name: Option<String>,
    },

    #[command(about = "Modify a service in services.yml with interactive prompts.")]
    Modify {
        #[arg(help = "Service name to modify")]
        name: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[tokio::main]
async fn main() {
    logging::set_enabled(false);

    let cli = Cli::parse();
    let result = async {
        print_header();

        if !matches!(cli.command, Commands::Update) {
            update_check().await;
        }

        match &cli.command {
            Commands::Init => init_config(&cli),
            Commands::Status => status(&cli).await,
            Commands::Diff => diff(&cli).await,
            Commands::Validate => validate_files(&cli),
            Commands::Update => update().await,
            Commands::Apply => apply(&cli).await,
            Commands::Service { command } => manage_service(&cli, command),
        }
    }
    .await;

    if let Err(error) = result {
        print_error(&error.to_string());
        std::process::exit(1);
    }
}

async fn status(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let files = load_required_files(cli)?;
    let (dns_provider, proxy_provider) = providers(&files.config);
    let dns = dns_provider.summary(zone(&files.config)).await?;
    let npm = proxy_provider.summary(zone(&files.config)).await?;

    section("Status");
    print_table(
        &["Provider", "Resource", "Managed", "Unmanaged"],
        &[
            vec![
                "Cloudflare".to_owned(),
                "DNS records".to_owned(),
                dns.managed.to_string(),
                dns.unmanaged.to_string(),
            ],
            vec![
                "NPM".to_owned(),
                "Proxy hosts".to_owned(),
                npm.proxy_managed.to_string(),
                npm.proxy_unmanaged.to_string(),
            ],
            vec![
                "NPM".to_owned(),
                "SSL certificates".to_owned(),
                npm.ssl_managed.to_string(),
                npm.ssl_unmanaged.to_string(),
            ],
        ],
    );
    ok("Status loaded successfully.");

    Ok(())
}

async fn diff(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let files = load_required_files(cli)?;
    let (dns_provider, proxy_provider) = providers(&files.config);
    let plan = build_plan(
        &files.config,
        &files.services,
        &dns_provider,
        &proxy_provider,
    )
    .await?;

    section("Diff");
    print_dns_changes(zone(&files.config), &plan.dns_changes);
    print_proxy_changes(&plan.proxy_changes);
    print_certificate_changes(plan.certificate_changes.len());

    let total = plan.dns_changes.len() + plan.proxy_changes.len() + plan.certificate_changes.len();
    if total == 0 {
        ok("Everything is already in sync.");
    } else {
        warn(&format!("{total} change(s) pending."));
    }

    Ok(())
}

fn validate_files(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let files = load_required_files(cli)?;
    validate(&files.config, &files.services)?;

    section("Validate");
    row("Services", files.services.services.len());
    ok("Configuration is valid.");

    Ok(())
}

fn manage_service(cli: &Cli, command: &ServiceCommands) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        ServiceCommands::List => service_list(cli),
        ServiceCommands::Add { name } => service_add(cli, name.as_deref()),
        ServiceCommands::Remove { name } => service_remove(cli, name.as_deref()),
        ServiceCommands::Modify { name } => service_modify(cli, name.as_deref()),
    }
}

fn service_list(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let files = load_required_files(cli)?;

    section("Services");
    if files.services.services.is_empty() {
        println!("  {}", paint("No services declared", Color::Dim));
        return Ok(());
    }

    let rows = files
        .services
        .services
        .iter()
        .map(|(name, service)| {
            vec![
                name.clone(),
                service.domain.clone(),
                service
                    .proxy_host
                    .clone()
                    .unwrap_or_else(|| default_proxy_host(&files.config).to_owned()),
                format!(
                    "{}://{}:{}",
                    upstream_scheme_name(&service.upstream.scheme),
                    service.upstream.host,
                    service.upstream.port
                ),
                yes_no(service.tls).to_owned(),
                yes_no(service.websocket).to_owned(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &[
            "Name",
            "Domain",
            "DNS Target",
            "Upstream",
            "TLS",
            "Websocket",
        ],
        &rows,
    );

    Ok(())
}

fn service_add(cli: &Cli, name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut files = load_required_files(cli)?;
    let services_path = services_path(cli)?;
    let name = service_name_from_arg_or_prompt(name, None)?;

    if files.services.services.contains_key(&name) {
        return Err(format!("service already exists: {name}").into());
    }

    section("Add Service");
    let service = prompt_verified_service(&name, &files.config, &files.services, None)?;
    files.services.services.insert(name.clone(), service);
    save_services(&services_path, &files.services)?;
    ok(&format!("Added service `{name}`."));

    Ok(())
}

fn service_remove(cli: &Cli, name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut files = load_required_files(cli)?;
    let services_path = services_path(cli)?;
    let name = service_name_from_arg_or_prompt(name, None)?;

    if !files.services.services.contains_key(&name) {
        return Err(format!("service not found: {name}").into());
    }

    section("Remove Service");
    if !confirm(&format!("Remove service `{name}`?"), false)? {
        warn("Aborted.");
        return Ok(());
    }

    files.services.services.remove(&name);
    validate(&files.config, &files.services)?;
    save_services(&services_path, &files.services)?;
    ok(&format!("Removed service `{name}`."));

    Ok(())
}

fn service_modify(cli: &Cli, name: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let mut files = load_required_files(cli)?;
    let services_path = services_path(cli)?;
    let name = service_name_from_arg_or_prompt(name, None)?;
    let current = files
        .services
        .services
        .get(&name)
        .cloned()
        .ok_or_else(|| format!("service not found: {name}"))?;

    section("Modify Service");
    let service = prompt_verified_service(&name, &files.config, &files.services, Some(&current))?;
    files.services.services.insert(name.clone(), service);
    save_services(&services_path, &files.services)?;
    ok(&format!("Updated service `{name}`."));

    Ok(())
}

async fn apply(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let files = load_required_files(cli)?;
    let (dns_provider, proxy_provider) = providers(&files.config);

    section("Apply");
    reconcile(
        &files.config,
        &files.services,
        &dns_provider,
        &proxy_provider,
        &proxy_provider,
    )
    .await?;
    ok("Desired state applied.");

    Ok(())
}

async fn update() -> Result<(), Box<dyn std::error::Error>> {
    section("Update");
    let release = latest_release().await?;
    let current = env!("CARGO_PKG_VERSION");
    let latest = release.tag_name.trim_start_matches('v');

    if latest == current || !version_is_newer(latest, current) {
        ok(&format!("Avel Pilot is already up to date ({current})."));
        return Ok(());
    }

    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == UPDATE_ASSET)
        .ok_or_else(|| format!("release asset not found: {UPDATE_ASSET}"))?;
    warn(&format!(
        "Updating {current} -> {latest} from {}",
        release.html_url
    ));

    let bytes = reqwest::get(&asset.browser_download_url)
        .await?
        .bytes()
        .await?;
    let current_exe = std::env::current_exe()?;
    let tmp = update_temp_path(&current_exe);
    fs::write(&tmp, bytes)?;
    set_executable(&tmp)?;
    fs::rename(&tmp, &current_exe)?;

    ok(&format!("Updated Avel Pilot to {latest}."));
    Ok(())
}

async fn build_plan(
    config: &Config,
    services: &ServicesFile,
    dns_provider: &CloudflareDnsProvider,
    proxy_provider: &NPMProxyProvider,
) -> Result<ReconcilePlan, Box<dyn std::error::Error>> {
    validate(config, services)?;

    let actual_certificates = proxy_provider.certificates().await?;
    let needs_tls = services.services.values().any(|service| service.tls);
    let certificate_id = existing_wildcard_certificate_id(zone(config), &actual_certificates)
        .or_else(|| needs_tls.then(|| avel_pilot::cert_provider::CertificateId("0".to_owned())));
    let mut plan = plan_reconcile(
        config,
        services,
        dns_provider,
        proxy_provider,
        certificate_id,
    )
    .await?;

    let certificate_desired = build_desired_state(config, services, None)?.certificates;
    plan.certificate_changes = diff_certificates(&certificate_desired, &actual_certificates);

    Ok(plan)
}

fn load_required_files(
    cli: &Cli,
) -> Result<avel_pilot::config::AppFiles, Box<dyn std::error::Error>> {
    let config_path = config_path(cli)?;
    let services_path = services_path(cli)?;

    ensure_runtime_file(&config_path, "config")?;
    ensure_runtime_file(&services_path, "services")?;

    Ok(load_app_files(&config_path, &services_path)?)
}

fn config_path(cli: &Cli) -> Result<PathBuf, Box<dyn std::error::Error>> {
    expand_home(&cli.config)
}

fn services_path(cli: &Cli) -> Result<PathBuf, Box<dyn std::error::Error>> {
    expand_home(&cli.services)
}

fn save_services(path: &Path, services: &ServicesFile) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(path, serde_yaml::to_string(services)?)?;

    Ok(())
}

fn providers(config: &Config) -> (CloudflareDnsProvider, NPMProxyProvider) {
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

    (dns_provider, proxy_provider)
}

fn prompt_verified_service(
    name: &str,
    config: &Config,
    services: &ServicesFile,
    current: Option<&ServiceConfig>,
) -> Result<ServiceConfig, Box<dyn std::error::Error>> {
    let mut defaults = current
        .cloned()
        .unwrap_or_else(|| default_service(name, config));

    loop {
        let service = prompt_service(name, &defaults)?;
        let mut candidate = services.clone();
        candidate.services.insert(name.to_owned(), service.clone());

        sub_section("Verify");
        match validate(config, &candidate) {
            Ok(()) => return Ok(service),
            Err(error) => {
                print_error(&format!("Service is not valid: {error}"));
                if !confirm("Edit values and try again?", true)? {
                    return Err("service edit aborted".into());
                }
                defaults = service;
            }
        }
    }
}

fn prompt_service(
    name: &str,
    defaults: &ServiceConfig,
) -> Result<ServiceConfig, Box<dyn std::error::Error>> {
    let domain = prompt_required("Domain", Some(&defaults.domain))?;
    let proxy_host = prompt_optional("DNS proxy host override", defaults.proxy_host.as_deref())?;
    let scheme = prompt_scheme("Upstream scheme", &defaults.upstream.scheme)?;
    let default_port = port_default(&scheme, defaults.upstream.port);
    let host = prompt_required("Upstream host", Some(&defaults.upstream.host))?;
    let port = prompt_u16("Upstream port", default_port)?;
    let tls = prompt_bool("TLS", defaults.tls)?;
    let websocket = prompt_bool("Websocket", defaults.websocket)?;

    println!();
    println!("  {} {}", paint("Name", Color::Bold), name);
    println!("  {} {}", paint("Domain", Color::Bold), domain);
    println!(
        "  {} {}",
        paint("DNS proxy host", Color::Bold),
        proxy_host.as_deref().unwrap_or("default")
    );
    println!(
        "  {} {}://{}:{}",
        paint("Upstream", Color::Bold),
        upstream_scheme_name(&scheme),
        host,
        port
    );
    println!("  {} {}", paint("TLS", Color::Bold), yes_no(tls));
    println!(
        "  {} {}",
        paint("Websocket", Color::Bold),
        yes_no(websocket)
    );

    let service = ServiceConfig {
        domain,
        proxy_host,
        upstream: UpstreamConfig { host, port, scheme },
        tls,
        websocket,
    };

    Ok(service)
}

fn default_service(name: &str, config: &Config) -> ServiceConfig {
    ServiceConfig {
        domain: default_domain(name, zone(config)),
        proxy_host: None,
        upstream: UpstreamConfig {
            host: default_proxy_host(config).to_owned(),
            port: 80,
            scheme: UpstreamSchemeConfig::Http,
        },
        tls: true,
        websocket: false,
    }
}

fn service_name_from_arg_or_prompt(
    name: Option<&str>,
    default: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let name = match name {
        Some(name) => name.trim().to_owned(),
        None => prompt_required("Service name", default)?,
    };

    if name.is_empty() {
        return Err("service name cannot be empty".into());
    }

    Ok(name)
}

fn default_domain(name: &str, zone: &str) -> String {
    if name.contains('.') {
        name.to_owned()
    } else {
        format!("{name}.{zone}")
    }
}

fn default_proxy_host(config: &Config) -> &str {
    match &config.proxy {
        ProxyConfig::Npm { host, .. } => host,
    }
}

fn prompt_optional(
    label: &str,
    default: Option<&str>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let value = prompt(label, default)?;
    let value = value.trim();

    Ok((!value.is_empty()).then(|| value.to_owned()))
}

fn prompt_bool(label: &str, default: bool) -> Result<bool, Box<dyn std::error::Error>> {
    loop {
        let value = prompt(label, Some(if default { "yes" } else { "no" }))?;
        match value.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "1" => return Ok(true),
            "n" | "no" | "false" | "0" => return Ok(false),
            _ => print_error(&format!("{label} must be yes or no")),
        }
    }
}

fn prompt_u16(label: &str, default: u16) -> Result<u16, Box<dyn std::error::Error>> {
    loop {
        let value = prompt(label, Some(&default.to_string()))?;
        match value.parse::<u16>() {
            Ok(port) if port > 0 => return Ok(port),
            _ => print_error(&format!("{label} must be a port between 1 and 65535")),
        }
    }
}

fn prompt_scheme(
    label: &str,
    default: &UpstreamSchemeConfig,
) -> Result<UpstreamSchemeConfig, Box<dyn std::error::Error>> {
    loop {
        let value = prompt(label, Some(upstream_scheme_name(default)))?;
        match value.to_ascii_lowercase().as_str() {
            "http" => return Ok(UpstreamSchemeConfig::Http),
            "https" => return Ok(UpstreamSchemeConfig::Https),
            _ => print_error(&format!("{label} must be http or https")),
        }
    }
}

fn port_default(scheme: &UpstreamSchemeConfig, current: u16) -> u16 {
    if current != 0 {
        return current;
    }

    match scheme {
        UpstreamSchemeConfig::Http => 80,
        UpstreamSchemeConfig::Https => 443,
    }
}

fn upstream_scheme_name(scheme: &UpstreamSchemeConfig) -> &'static str {
    match scheme {
        UpstreamSchemeConfig::Http => "http",
        UpstreamSchemeConfig::Https => "https",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn ensure_runtime_file(path: &Path, kind: &str) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() {
        return Ok(());
    }

    Err(format!(
        "{kind} file not found at {}. Run `avel-pilot init` first.",
        path.display()
    )
    .into())
}

async fn update_check() {
    match latest_release().await {
        Ok(release) => {
            let current = env!("CARGO_PKG_VERSION");
            let latest = release.tag_name.trim_start_matches('v');
            if version_is_newer(latest, current) {
                println!(
                    "{} Avel Pilot {latest} is available. Run `avel-pilot update`.",
                    paint("UPDATE", Color::Yellow)
                );
            }
        }
        Err(error) => {
            println!(
                "{} Update check skipped: {error}",
                paint("UPDATE", Color::Dim)
            );
        }
    }
}

async fn latest_release() -> Result<GitHubRelease, Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let release = client
        .get(format!(
            "https://api.github.com/repos/{GITHUB_REPO}/releases/latest"
        ))
        .header(USER_AGENT, "avel-pilot")
        .header(ACCEPT, "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json::<GitHubRelease>()
        .await?;

    Ok(release)
}

fn init_config(cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    section("Init");
    let config_path = expand_home(&cli.config)?;
    let services_path = expand_home(&cli.services)?;
    println!(
        "Creating {} and {}",
        config_path.display(),
        services_path.display()
    );

    if config_path.exists() && !confirm("Config already exists. Overwrite?", false)? {
        warn("Aborted.");
        return Ok(());
    }
    if services_path.exists() && !confirm("Services file already exists. Overwrite?", false)? {
        warn("Aborted.");
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
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = services_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&config_path, content)?;
    fs::write(&services_path, services_example(&zone))?;
    set_private_permissions(&config_path)?;
    ok(&format!("Wrote {}", config_path.display()));
    ok(&format!("Wrote {}", services_path.display()));

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

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;

    Ok(())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
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
#     # Optional: defaults to proxy.host from config.yml.
#     # proxy_host: edge.example.com
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

        print_error(&format!("{label} is required"));
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

fn print_dns_changes(zone: &str, changes: &[DnsChange]) {
    sub_section("Cloudflare DNS");
    if changes.is_empty() {
        println!("  {}", paint("No changes", Color::Green));
        return;
    }

    for change in changes {
        println!("  - {}", describe_dns_change(zone, change));
    }
}

fn print_proxy_changes(changes: &[ProxyChange]) {
    sub_section("NPM Proxy");
    if changes.is_empty() {
        println!("  {}", paint("No changes", Color::Green));
        return;
    }

    for change in changes {
        println!("  - {}", describe_proxy_change(change));
    }
}

fn print_certificate_changes(count: usize) {
    sub_section("NPM SSL");
    if count == 0 {
        println!("  {}", paint("No changes", Color::Green));
    } else {
        println!("  - {count} certificate change(s)");
    }
}

fn update_temp_path(current_exe: &Path) -> PathBuf {
    let mut tmp = current_exe.to_path_buf();
    tmp.set_extension("avel-pilot-update");
    tmp
}

fn expand_home(path: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if path == "~" {
        return home_dir();
    }

    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }

    Ok(PathBuf::from(path))
}

fn home_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set; pass --config and --services explicitly".into())
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    version_parts(candidate) > version_parts(current)
}

fn version_parts(version: &str) -> [u32; 3] {
    let mut parts = [0; 3];

    for (index, part) in version
        .split(['.', '-'])
        .take(3)
        .filter_map(|part| part.parse::<u32>().ok())
        .enumerate()
    {
        parts[index] = part;
    }

    parts
}

fn print_header() {
    println!("{}", paint("Avel Pilot", Color::Cyan));
    println!(
        "{}",
        paint("Cloudflare DNS + NPM control plane", Color::Dim)
    );
    println!();
}

fn section(title: &str) {
    println!("{}", paint(title, Color::Cyan));
}

fn sub_section(title: &str) {
    println!("{}", paint(title, Color::Bold));
}

fn row(label: &str, value: usize) {
    println!("  {:<28} {}", label, paint(value, Color::Bold));
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let widths = table_widths(headers, rows);
    let border = table_border(&widths);

    println!("  {border}");
    println!("  {}", table_row(headers, &widths));
    println!("  {border}");
    for row in rows {
        println!("  {}", table_row(row, &widths));
    }
    println!("  {border}");
}

fn table_widths(headers: &[&str], rows: &[Vec<String>]) -> Vec<usize> {
    headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            rows.iter()
                .filter_map(|row| row.get(index))
                .map(|value| value.len())
                .chain(std::iter::once(header.len()))
                .max()
                .unwrap_or(0)
        })
        .collect()
}

fn table_border(widths: &[usize]) -> String {
    let parts = widths
        .iter()
        .map(|width| "-".repeat(width + 2))
        .collect::<Vec<_>>();

    format!("+{}+", parts.join("+"))
}

fn table_row<T>(values: &[T], widths: &[usize]) -> String
where
    T: AsRef<str>,
{
    let cells = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let value = values.get(index).map(AsRef::as_ref).unwrap_or("");

            format!(" {value:<width$} ")
        })
        .collect::<Vec<_>>();

    format!("|{}|", cells.join("|"))
}

fn ok(message: &str) {
    println!("{} {message}", paint("OK", Color::Green));
}

fn warn(message: &str) {
    println!("{} {message}", paint("WARN", Color::Yellow));
}

fn print_error(message: &str) {
    eprintln!("{} {message}", paint("ERROR", Color::Red));
}

enum Color {
    Bold,
    Cyan,
    Dim,
    Green,
    Red,
    Yellow,
}

fn paint(value: impl std::fmt::Display, color: Color) -> String {
    let code = match color {
        Color::Bold => "1",
        Color::Cyan => "36;1",
        Color::Dim => "2",
        Color::Green => "32;1",
        Color::Red => "31;1",
        Color::Yellow => "33;1",
    };

    format!("\x1b[{code}m{value}\x1b[0m")
}
