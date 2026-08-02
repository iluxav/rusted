# Deploying rusted

A single small VPS runs the whole thing: Postgres, rusted, and Caddy in front.
A Hetzner CX22 (2 vCPU, 4 GB, ~€4/mo) is comfortable.

Caddy is the only container with published ports. rusted itself is reachable
only on the compose network — a server whose job is executing other people's
code should not be the thing facing the internet.

## Creating the server

```bash
brew install hcloud
export HCLOUD_TOKEN=...          # read/write, in a Hetzner project used only for rusted
./provision.sh ~/.ssh/id_ed25519.pub
```

That creates a **CX22** (2 vCPU, 4 GB, ~€4.5/mo), attaches a firewall allowing
only 22, 80 and 443, and boots it with `cloud-init.yaml`.

### When everything is sold out

Hetzner's cheap lines sell out in waves, and it has nothing to do with your
account. The script searches five types across six locations before giving up,
and can wait for capacity to appear:

```bash
./provision.sh --wait           # keep looking for an hour
./provision.sh --wait=21600     # for six
```

It usually clears within hours. If you'd rather not wait, the deploy is plain
docker compose and runs anywhere — **Netcup** (~€3), **Scaleway** (~€4),
**OVH** (~€4), **Vultr** or **DigitalOcean** ($6–12) all work, and nothing but
`provision.sh` is Hetzner-specific. Create a box with Ubuntu 24.04, paste
`cloud-init.yaml` into its user-data field, and the rest of this runbook is
unchanged.

### Picking a type

Any of these run rusted comfortably — releases cover both architectures and the
Dockerfile picks the right one, so the choice is purely price and availability:

| Type | | ~cost | Where |
|---|---|---|---|
| `cx22` | 2 vCPU Intel, 4 GB | €4.5/mo | everywhere |
| `cax11` | 2 vCPU ARM, 4 GB | €3.8/mo | EU only (fsn1, nbg1, hel1), often sold out |
| `cpx11` | 2 vCPU AMD, 2 GB | €4.4/mo | everywhere; 2 GB is tight with Postgres alongside |

```bash
RUSTED_SERVER_TYPE=cax11 RUSTED_SERVER_LOCATION=hel1 ./provision.sh
```

Hetzner sells out of individual types per location, so the script checks before
creating and tells you what's available rather than failing with an API error.
To look for yourself:

```bash
hcloud server-type list
hcloud datacenter list
``` The machine
arrives with Docker installed, root login and password auth disabled, `ufw` and
`fail2ban` running, unattended security upgrades on, 2 GB of swap so a spike
degrades instead of OOM-killing Postgres, and this repository cloned.

SSH is restricted to the address you provisioned from. Widen it when that
changes:

```bash
hcloud firewall add-rule rusted-fw --direction in --protocol tcp --port 22 --source-ips <your-ip>/32
```

Once traffic is proxied through Cloudflare, narrowing 80/443 to [Cloudflare's
ranges](https://www.cloudflare.com/ips/) stops anyone reaching your origin
directly and bypassing the edge.

Turn on Hetzner's backups too — separate from `pg_dump`, and worth the ~€0.8:

```bash
hcloud server enable-backup rusted
```

## First deploy

```bash
ssh rusted@<ip>
cd ~/rusted/deploy
$EDITOR .env                             # domain, password, GitHub OAuth
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

## Reaching the database

Postgres is published to the server's loopback only, so nothing external can
see it and no firewall port is needed. Tunnel over the SSH you already have:

```bash
ssh -N -L 5432:localhost:5432 rusted@<ip>
```

Leave that running and connect locally as if the database were on your machine:

```bash
psql postgres://rusted:<POSTGRES_PASSWORD>@127.0.0.1:5432/rusted
```

TablePlus, DataGrip, and pgAdmin all speak SSH tunnelling natively — point them
at `localhost:5432` over `rusted@<ip>` and skip the manual tunnel.

For a quick look without any of that:

```bash
ssh rusted@<ip> 'cd rusted/deploy && docker compose exec -T db psql -U rusted rusted -c "select count(*) from functions"'
```

**Don't open 5432 in the firewall.** It buys you nothing the tunnel doesn't
already give you, and it puts password-authenticated Postgres in front of the
whole internet. If you genuinely need direct access — a managed BI tool, say —
restrict it to that single source and require TLS:

```bash
hcloud firewall add-rule rusted-fw --direction in --protocol tcp --port 5432 --source-ips <their-ip>/32
```

and publish the port on `0.0.0.0` rather than loopback. Treat that as a
deliberate exception, not the default.

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
