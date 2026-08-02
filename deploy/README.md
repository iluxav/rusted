# Deploying rusted

A single small VPS runs the whole thing: Postgres, rusted, and Caddy in front.
A Hetzner CX22 (2 vCPU, 4 GB, ~€4/mo) is comfortable.

Caddy is the only container with published ports. rusted itself is reachable
only on the compose network — a server whose job is executing other people's
code should not be the thing facing the internet.

## First deploy

```bash
# on the server
git clone https://github.com/iluxav/rusted.git && cd rusted/deploy
cp .env.example .env && $EDITOR .env     # domain, password, GitHub OAuth
```

### Certificate

The origin certificate comes from Cloudflare, so nothing has to renew and no
inbound challenge is needed.

1. Cloudflare dashboard → your domain → **SSL/TLS → Origin Server → Create
   Certificate**. Accept the defaults (RSA, 15 years, `rusted.sh` and
   `*.rusted.sh`).
2. Save the two blocks on the server:

```bash
mkdir -p certs
$EDITOR certs/origin.pem      # the certificate
$EDITOR certs/origin.key      # the private key
chmod 600 certs/origin.key
```

3. Cloudflare → **SSL/TLS → Overview → Full (strict)**. Anything less lets the
   hop between Cloudflare and your server be unencrypted or unverified.

### DNS

An `A` record for `rusted.sh` pointing at the server's IPv4, **proxied**
(orange cloud). Cloudflare then terminates TLS at the edge and talks to Caddy
over the origin certificate.

### Start it

```bash
docker compose up -d
docker compose logs -f rusted
```

Migrations run at boot, so there is no separate schema step. Then sign in at
`https://rusted.sh`, create a key, and from your laptop:

```bash
rusted login --admin https://rusted.sh
rusted push index.js
```

## GitHub OAuth

Create the app at github.com/settings/developers with:

- Homepage: `https://rusted.sh`
- Callback: `https://rusted.sh/auth/github/callback`

Put the client id and secret in `.env`. Until you do, the sign-in page explains
what's missing rather than offering a button that fails.

## Upgrading

```bash
$EDITOR .env                 # bump RUSTED_VERSION to the new tag
docker compose build rusted && docker compose up -d rusted
```

Migrations apply on start. `RUSTED_VERSION` is pinned deliberately: an
unattended `latest` is how a deployment changes without anyone deciding it
should.

## Backups

Everything that matters is in Postgres — functions, keys, users, invocations.

```bash
docker compose exec -T db pg_dump -U rusted rusted | gzip > rusted-$(date +%F).sql.gz
```

Put that in cron and ship it off the box; a backup that only exists on the
server it protects is not a backup. To restore:

```bash
gunzip -c rusted-2026-08-01.sql.gz | docker compose exec -T db psql -U rusted rusted
```

## Without Docker

The binary works on its own if you'd rather run Postgres separately:

```bash
curl -fsSL https://raw.githubusercontent.com/iluxav/rusted/main/install.sh | sh
sudo cp rusted.service /etc/systemd/system/
sudo systemctl enable --now rusted
```

See `rusted.service` for the unit; it expects `/etc/rusted/env` to hold the
same variables as `.env`.

## Watch out for

- **`--require-auth`** makes every function endpoint demand an API key. Off by
  default, because public endpoints are the product — turn it on if this
  deployment is private.
- **Function URLs come from `PUBLIC_URL`**, not the socket. Get it wrong and
  the console will hand people URLs that don't resolve.
- **Postgres is not exposed.** Reach it with
  `docker compose exec db psql -U rusted` rather than opening 5432.
