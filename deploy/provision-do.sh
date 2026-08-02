#!/usr/bin/env bash
# Creates the DigitalOcean droplet rusted runs on. Same result as
# provision.sh, different provider.
#
#   doctl auth init                            # once, with your DO token
#   ./provision-do.sh ~/.ssh/id_ed25519.pub
#
# Re-running is safe: it stops if the droplet already exists rather than
# creating a second one.

set -euo pipefail

NAME="${RUSTED_SERVER_NAME:-rusted}"
# s-1vcpu-2gb ($12) is enough to demo. s-2vcpu-2gb ($18) and s-2vcpu-4gb ($24)
# are the next steps up; see "growing" at the bottom — resizing CPU and RAM is
# reversible, resizing disk is not.
SIZE="${RUSTED_SERVER_SIZE:-s-1vcpu-2gb}"
IMAGE="${RUSTED_SERVER_IMAGE:-ubuntu-24-04-x64}"
# fra1/ams3/lon1 EU · nyc1,nyc3/sfo3/tor1 NA · sgp1/blr1/syd1 APAC.
# Pick for latency to your users: that is the product.
REGION="${RUSTED_SERVER_REGION:-fra1}"

key_file="${1:-$HOME/.ssh/id_ed25519.pub}"

die() { printf 'provision: %s\n' "$*" >&2; exit 1; }

command -v doctl >/dev/null || die "doctl not found — brew install doctl, then doctl auth init"
doctl account get >/dev/null 2>&1 || die "doctl is not authenticated — run: doctl auth init"
[ -f "$key_file" ] || die "no SSH public key at $key_file"

if doctl compute droplet get "$NAME" >/dev/null 2>&1; then
	die "a droplet named $NAME already exists — destroy it first, or set RUSTED_SERVER_NAME"
fi

# --- ssh key -----------------------------------------------------------------
key_name="${NAME}-admin"
key_id="$(doctl compute ssh-key list --no-header --format ID,Name |
	awk -v n="$key_name" '$2 == n { print $1 }')"
if [ -z "$key_id" ]; then
	key_id="$(doctl compute ssh-key import "$key_name" \
		--public-key-file "$key_file" --no-header --format ID)"
fi

# --- cloud-init --------------------------------------------------------------
user_data="$(mktemp)"
trap 'rm -f "$user_data"' EXIT
# Substituted rather than templated, so cloud-init.yaml stays valid on its own
# and can be read before it ever runs.
awk -v key="$(cat "$key_file")" \
	'{ gsub(/SSH_PUBLIC_KEY_PLACEHOLDER/, key); print }' \
	"$(dirname "$0")/cloud-init.yaml" > "$user_data"

echo "creating $NAME ($SIZE, $IMAGE, $REGION)…"
doctl compute droplet create "$NAME" \
	--size "$SIZE" \
	--image "$IMAGE" \
	--region "$REGION" \
	--ssh-keys "$key_id" \
	--user-data-file "$user_data" \
	--tag-name rusted \
	--enable-monitoring \
	--wait

ip="$(doctl compute droplet get "$NAME" --no-header --format PublicIPv4)"

# --- firewall ----------------------------------------------------------------
# Outside the droplet, so it holds even if the machine is misconfigured. SSH is
# limited to the address provisioning from; 80/443 stay open for Cloudflare.
me="$(curl -fsSL https://ipv4.icanhazip.com 2>/dev/null || echo 0.0.0.0)"
fw_name="${NAME}-fw"
if ! doctl compute firewall list --no-header --format Name | grep -qx "$fw_name"; then
	doctl compute firewall create \
		--name "$fw_name" \
		--tag-names rusted \
		--inbound-rules "protocol:tcp,ports:22,address:${me}/32 protocol:tcp,ports:80,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:443,address:0.0.0.0/0,address:::/0" \
		--outbound-rules "protocol:tcp,ports:all,address:0.0.0.0/0,address:::/0 protocol:udp,ports:all,address:0.0.0.0/0,address:::/0 protocol:icmp,address:0.0.0.0/0,address:::/0"
fi

cat <<NEXT

created. IPv4: $ip  ($SIZE in $REGION)

  1. Point rusted.sh at $ip in Cloudflare (A record, proxied)
  2. Wait for first boot to finish, then:
       ssh rusted@$ip
       cd ~/rusted/deploy
       nano .env                     # domain, password, GitHub OAuth
       mkdir certs                   # paste the Cloudflare origin cert here
       docker compose up -d

Growing later, when a demo becomes traffic:
  doctl compute droplet-action power-off $NAME --wait
  doctl compute droplet-action resize $NAME --size s-2vcpu-4gb --wait
  doctl compute droplet-action power-on $NAME --wait

That resizes CPU and RAM only, and is reversible. Adding --resize-disk grows
the disk too and cannot be undone — the droplet can never shrink again.

Backups are not on by default (+20%):
  doctl compute droplet-action enable-backups $NAME
NEXT
