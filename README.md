# Avel Pilot

Avel Pilot riconcilia servizi dichiarati in YAML con Nginx Proxy Manager e Cloudflare DNS.

Il processo legge:

- `/etc/avel-pilot/config.yml`: provider, zona DNS e host pubblico del proxy
- `/etc/avel-pilot/services.yml`: servizi da esporre

Quando parte, applica subito lo stato desiderato. Poi resta in watch su `services.yml` e riconcilia di nuovo ogni volta che il file cambia.

## config.yml

Esempio:

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

Campi DNS:

- `dns.type`: per ora `cloudflare`
- `dns.zone`: zona Cloudflare gestita, per esempio `avel.space`
- `dns.api_token`: token API Cloudflare
- `dns.propagation_seconds`: attesa usata da NPM/Certbot per la DNS-01 challenge

Campi proxy:

- `proxy.type`: per ora `npm`
- `proxy.host`: valore usato nei record DNS, cioe' dove devono puntare i domini pubblici
- `proxy.url`: URL API/admin di Nginx Proxy Manager
- `proxy.identity`: utente NPM
- `proxy.secret`: password NPM

I valori `${VAR}` vengono espansi dall'ambiente prima del parsing YAML. Se usi `avel-pilot init`, invece, i segreti vengono scritti direttamente in `/etc/avel-pilot/config.yml`.

## services.yml

Esempio:

```yaml
services:
  jellyfin:
    domain: tv.avel.space
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

Ogni chiave sotto `services` e' un nome logico del servizio.

Campi servizio:

- `domain`: dominio pubblico da pubblicare
- `upstream.scheme`: `http` o `https`, default `http`
- `upstream.host`: host interno raggiungibile da NPM
- `upstream.port`: porta interna
- `tls`: se `true`, il proxy usa il certificato wildcard
- `websocket`: se `true`, abilita websocket su NPM

I domini devono stare dentro la zona configurata in `config.yml`. Se la zona e' `avel.space`, `tv.avel.space` e' valido, `tv.example.com` no.

## Risorse Gestite

Avel Pilot tocca solo le risorse che considera proprie.

Proxy host NPM:

- creati con `meta.avel_pilot = true`
- la lettura ritorna solo gli host marcati
- il diff puo' creare, aggiornare ed eliminare questi host

DNS record Cloudflare:

- creati con commento contenente `managed-by:avel-pilot`
- la lettura ritorna solo i record con quel marker
- il diff puo' creare, aggiornare ed eliminare questi record

Certificati:

- viene preferito un solo certificato wildcard per zona, per esempio `*.avel.space`
- viene creato via DNS-01 quando almeno un servizio ha `tls: true`
- non vengono eliminati certificati extra in automatico, perche' NPM non conserva un marker affidabile sui certificati Let's Encrypt

## Avvio

Imposta le variabili d'ambiente:

```bash
export CLOUDFLARE_API_TOKEN=...
export NPM_IDENTITY=...
export NPM_SECRET=...
```

Poi avvia:

```bash
cargo run
```

Per usare file locali invece di `/etc/avel-pilot/*.yml`:

```bash
AVEL_PILOT_CONFIG=config.yaml AVEL_PILOT_SERVICES=services.yaml cargo run
```

Il processo:

1. legge `/etc/avel-pilot/config.yml` e `/etc/avel-pilot/services.yml`
2. valida i servizi
3. assicura il certificato wildcard se serve TLS
4. riconcilia DNS record e proxy host
5. resta in watch su `services.yml`

Per fermarlo:

```bash
Ctrl+C
```

## Pacchetto Debian/Ubuntu

La GitHub Action `Debian packages` crea un pacchetto `.deb` e un eseguibile standalone Linux amd64 a ogni push su `main`, pull request, esecuzione manuale e tag `v*`.
Build e packaging girano dentro Debian 12/bookworm e verificano che l'eseguibile non richieda una versione di glibc superiore alla 2.36.

Su tag `v*`, il pacchetto e l'eseguibile vengono anche allegati alla GitHub Release.

### Installazione Da Release

Scarica e installa il pacchetto `.deb` dall'ultima release:

```bash
curl -LO https://github.com/davide-leva/avel-pilot/releases/latest/download/avel-pilot-linux-amd64.deb
sudo apt install ./avel-pilot-linux-amd64.deb
```

Link stabile del pacchetto:

```text
https://github.com/davide-leva/avel-pilot/releases/latest/download/avel-pilot-linux-amd64.deb
```

Link stabile dell'eseguibile standalone:

```text
https://github.com/davide-leva/avel-pilot/releases/latest/download/avel-pilot-linux-amd64
```

In alternativa, se hai scaricato l'artifact della GitHub Action, estrai lo zip e installa il `.deb` contenuto:

```bash
unzip avel-pilot-debian-package.zip
sudo apt install ./avel-pilot-linux-amd64.deb
```

Il pacchetto installa:

- binario: `/usr/bin/avel-pilot`
- unit systemd: `/usr/lib/systemd/system/avel-pilot.service`
- esempi: `/usr/share/doc/avel-pilot/examples/`

### Configurazione

Prepara i file runtime in `/etc/avel-pilot` con il comando interattivo:

```bash
sudo avel-pilot init
```

Il comando crea:

- `/etc/avel-pilot/config.yml`, con i segreti in chiaro e permessi `0600`
- `/etc/avel-pilot/services.yml`, con un esempio commentato

Durante l'inizializzazione vengono richiesti:

- zona Cloudflare
- token API Cloudflare
- secondi di propagazione DNS
- URL di Nginx Proxy Manager
- host pubblico del proxy da usare nei record DNS
- identity e secret NPM

Poi modifica `services.yml` e dichiara i servizi reali:

```bash
sudo editor /etc/avel-pilot/services.yml
```

Puoi anche partire dagli esempi installati dal pacchetto:

```bash
sudo cp /usr/share/doc/avel-pilot/examples/config.yml /etc/avel-pilot/config.yml
sudo cp /usr/share/doc/avel-pilot/examples/services.yml /etc/avel-pilot/services.yml
```

### Avvio Con systemd

Abilita e avvia il servizio:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now avel-pilot
```

Controlla lo stato e i log:

```bash
systemctl status avel-pilot
journalctl -u avel-pilot -f
```

Se systemd non vede ancora il servizio:

```bash
sudo systemctl daemon-reload
systemctl list-unit-files 'avel-pilot*'
```

Dopo modifiche a `/etc/avel-pilot/config.yml`, riavvia:

```bash
sudo systemctl restart avel-pilot
```

Dopo modifiche a `/etc/avel-pilot/services.yml`, Avel Pilot riconcilia automaticamente entro pochi secondi.

## Sviluppo

Comandi utili:

```bash
cargo fmt --check
cargo check
cargo test
```
