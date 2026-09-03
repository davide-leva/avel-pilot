# Avel Pilot

Avel Pilot is a CLI for managing homelab services declared in YAML. It compares the desired state with Cloudflare DNS and Nginx Proxy Manager, then shows or applies the required changes.

It reads:

- `~/.config/avel-pilot/config.yml`: providers, DNS zone, and public proxy host
- `~/.config/avel-pilot/services.yml`: services to expose

All command output is in English. Every command except `update` checks GitHub for a newer Avel Pilot release before running.

## Commands

```bash
avel-pilot --help
avel-pilot <command> --help
```

Available commands:

- `status`: show a table of managed and unmanaged Cloudflare DNS records, NPM proxy hosts, and NPM SSL certificates
- `diff`: show the DNS, proxy, and certificate changes Avel Pilot would apply
- `validate`: validate `config.yml` and `services.yml` without contacting providers
- `apply`: apply the desired state once and exit
- `update`: download and replace the current binary with the latest GitHub release
- `init`: create `~/.config/avel-pilot/config.yml` and `~/.config/avel-pilot/services.yml` interactively
- `service list`: list services declared in `services.yml`
- `service add [name]`: add a service with interactive prompts
- `service remove [name]`: remove a service with confirmation
- `service modify [name]`: modify a service with interactive prompts

Use local files instead of `~/.config/avel-pilot/*.yml`:

```bash
avel-pilot --config ./config.yml --services ./services.yml diff
```

Environment overrides are also supported:

```bash
AVEL_PILOT_CONFIG=./config.yml AVEL_PILOT_SERVICES=./services.yml avel-pilot validate
```

## config.yml

```yaml
dns:
  type: cloudflare
  zone: avel.space
  api_token: ${CLOUDFLARE_API_TOKEN}
  propagation_seconds: 120

proxy:
  type: npm
  host: 10.0.5.104
  url: http://10.0.5.104:81
  identity: ${NPM_IDENTITY}
  secret: ${NPM_SECRET}
```

DNS fields:

- `dns.type`: currently `cloudflare`
- `dns.zone`: managed Cloudflare zone, for example `avel.space`
- `dns.api_token`: Cloudflare API token
- `dns.propagation_seconds`: wait used by NPM/Certbot for DNS-01 challenges

Proxy fields:

- `proxy.type`: currently `npm`
- `proxy.host`: value used in DNS records, where public domains should point
- `proxy.url`: Nginx Proxy Manager API/admin URL
- `proxy.identity`: NPM user
- `proxy.secret`: NPM password

`${VAR}` values are expanded from the environment before YAML parsing. If you use `avel-pilot init`, secrets are written directly to `~/.config/avel-pilot/config.yml` with `0600` permissions.

## services.yml

```yaml
services:
  jellyfin:
    domain: tv.avel.space
    proxy_host: edge.avel.space
    upstream:
      scheme: http
      host: 10.0.5.101
      port: 8096
    tls: true
    websocket: true

  sonarr:
    domain: sonarr.avel.space
    upstream:
      scheme: http
      host: 10.0.5.101
      port: 8989
    tls: true
```

Each key under `services` is a logical service name.

You can manage this file interactively:

```bash
avel-pilot service list
avel-pilot service add jellyfin
avel-pilot service modify jellyfin
avel-pilot service remove jellyfin
```

`service add` uses sensible defaults from `config.yml`: `<name>.<zone>` for the domain, no DNS proxy host override, `proxy.host` as the initial upstream host, `http`, port `80`, TLS enabled, and websocket disabled. `service modify` uses the current service values as defaults.

`service add` and `service modify` verify the full services file before saving. If verification fails, no changes are written; Avel Pilot keeps the interactive session open so you can correct the values or abort.

Service fields:

- `domain`: public domain to publish
- `proxy_host`: optional DNS target for this service; defaults to `proxy.host` from `config.yml`
- `upstream.scheme`: `http` or `https`, default `http`
- `upstream.host`: internal host reachable by NPM
- `upstream.port`: internal port
- `tls`: if `true`, the proxy uses the wildcard certificate
- `websocket`: if `true`, enables websocket support in NPM

Domains must belong to the configured zone. If the zone is `avel.space`, `tv.avel.space` is valid and `tv.example.com` is not.

## Managed Resources

Avel Pilot only changes resources it owns.

Cloudflare DNS records:

- created with a comment containing `managed-by:avel-pilot`
- `diff` and `apply` only consider records with that marker
- `status` reports both managed and unmanaged records

NPM proxy hosts:

- created with `meta.avel_pilot = true`
- `diff` and `apply` only consider hosts with that marker
- `status` reports both managed and unmanaged hosts

NPM SSL certificates:

- Avel Pilot prefers one wildcard certificate per zone, for example `*.avel.space`
- it creates the certificate through DNS-01 when at least one service has `tls: true`
- new certificates include an `avel_pilot` metadata marker
- existing Let's Encrypt wildcard certificates for the configured zone are also recognized as managed
- extra certificates are not deleted automatically
- `status` reports managed and unmanaged NPM certificates

## Install From GitHub Release

```bash
curl -LO https://github.com/davide-leva/avel-pilot/releases/latest/download/avel-pilot-linux-amd64
chmod +x avel-pilot-linux-amd64
sudo install -m 0755 avel-pilot-linux-amd64 /usr/bin/avel-pilot
```

The Debian package is also published on tagged releases:

```bash
curl -LO https://github.com/davide-leva/avel-pilot/releases/latest/download/avel-pilot-linux-amd64.deb
sudo apt install ./avel-pilot-linux-amd64.deb
```

Update the installed standalone binary:

```bash
sudo avel-pilot update
```

## Development

Useful commands:

```bash
cargo fmt --check
cargo check
cargo test
```
