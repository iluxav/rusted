#!/usr/bin/env bash
# Creates the Hetzner server rusted runs on. You hold the token; this script
# only ever reads it from the environment.
#
#   export HCLOUD_TOKEN=...            # read/write, in a project of its own
#   ./provision.sh ~/.ssh/id_ed25519.pub
#
# Re-running is safe: it stops if the server already exists rather than
# creating a second one.

set -euo pipefail

NAME="${RUSTED_SERVER_NAME:-rusted}"
# cx22 (Intel, x86_64) is available everywhere. cax11 (ARM) is cheaper but
# EU-only and often sold out; both work, since releases cover both
# architectures and the Dockerfile picks the right one.
TYPE="${RUSTED_SERVER_TYPE:-cx22}"
IMAGE="${RUSTED_SERVER_IMAGE:-ubuntu-24.04}"
LOCATION="${RUSTED_SERVER_LOCATION:-fsn1}" # fsn1/nbg1/hel1 EU · ash/hil US · sin APAC

key_file="${1:-$HOME/.ssh/id_ed25519.pub}"

die() { printf 'provision: %s\n' "$*" >&2; exit 1; }

command -v hcloud >/dev/null || die "hcloud CLI not found — brew install hcloud"
[ -n "${HCLOUD_TOKEN:-}" ] || die "set HCLOUD_TOKEN (a token scoped to a project used only for rusted)"
[ -f "$key_file" ] || die "no SSH public key at $key_file"

if hcloud server describe "$NAME" >/dev/null 2>&1; then
	die "a server named $NAME already exists — delete it first, or set RUSTED_SERVER_NAME"
fi

# --- availability ------------------------------------------------------------
# Hetzner sells out of individual types per location, and the ARM line isn't
# offered outside the EU at all. Check before creating, so a sold-out type
# reads as "here is what you can have" rather than an API error.
available="$(hcloud server-type list -o noheader -o columns=name 2>/dev/null || true)"
if [ -n "$available" ] && ! printf '%s\n' "$available" | grep -qx "$TYPE"; then
	die "server type $TYPE does not exist; try: $(printf '%s ' $available)"
fi

if ! hcloud server-type describe "$TYPE" -o json 2>/dev/null |
	grep -q "\"location\": *\"$LOCATION\""; then
	echo "warning: $TYPE may not be offered in $LOCATION" >&2
	echo "         locations for $TYPE:" >&2
	hcloud server-type describe "$TYPE" -o json 2>/dev/null |
		sed -n 's/.*"location": *"\([a-z0-9]*\)".*/           \1/p' | sort -u >&2 || true
	echo "         set RUSTED_SERVER_LOCATION, or RUSTED_SERVER_TYPE to something offered there" >&2
fi

# --- ssh key -----------------------------------------------------------------
key_name="${NAME}-admin"
if ! hcloud ssh-key describe "$key_name" >/dev/null 2>&1; then
	hcloud ssh-key create --name "$key_name" --public-key-from-file "$key_file"
fi

# --- firewall ----------------------------------------------------------------
# Outside the VM, so it holds even if the machine is misconfigured. Restrict
# SSH to your own address; leave 80/443 open for Cloudflare (or narrow it to
# Cloudflare's ranges once traffic is proxied).
me="$(curl -fsSL https://ipv4.icanhazip.com || echo 0.0.0.0)/32"
fw_name="${NAME}-fw"
if ! hcloud firewall describe "$fw_name" >/dev/null 2>&1; then
	hcloud firewall create --name "$fw_name"
	hcloud firewall add-rule "$fw_name" --direction in --protocol tcp --port 22 --source-ips "$me" --description "ssh from provisioning host"
	hcloud firewall add-rule "$fw_name" --direction in --protocol tcp --port 80 --source-ips 0.0.0.0/0 --source-ips ::/0 --description http
	hcloud firewall add-rule "$fw_name" --direction in --protocol tcp --port 443 --source-ips 0.0.0.0/0 --source-ips ::/0 --description https
fi

# --- cloud-init --------------------------------------------------------------
user_data="$(mktemp)"
trap 'rm -f "$user_data"' EXIT
# The placeholder is replaced rather than templated so the file stays valid
# cloud-config on its own and can be reviewed before it ever runs.
awk -v key="$(cat "$key_file")" \
	'{ gsub(/SSH_PUBLIC_KEY_PLACEHOLDER/, key); print }' \
	"$(dirname "$0")/cloud-init.yaml" > "$user_data"

echo "creating $NAME ($TYPE, $IMAGE, $LOCATION)…"
hcloud server create \
	--name "$NAME" \
	--type "$TYPE" \
	--image "$IMAGE" \
	--location "$LOCATION" \
	--ssh-key "$key_name" \
	--firewall "$fw_name" \
	--user-data-from-file "$user_data" \
	--label role=rusted

ip="$(hcloud server ip "$NAME")"
cat <<NEXT

created. IPv4: $ip  ($TYPE in $LOCATION)

  1. Point rusted.sh at $ip in Cloudflare (A record, proxied)
  2. Wait for first boot to finish, then:
       ssh rusted@$ip
       cd ~/rusted/deploy
       nano .env                     # domain, password, GitHub OAuth
       mkdir certs                   # paste the Cloudflare origin cert here
       docker compose up -d

Backups are not on by default:
  hcloud server enable-backup $NAME
NEXT
