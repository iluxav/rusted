#!/usr/bin/env bash
# Creates the Hetzner server rusted runs on. You hold the token; this script
# only ever reads it from the environment.
#
#   export HCLOUD_TOKEN=...                    # read/write, in its own project
#   ./provision.sh ~/.ssh/id_ed25519.pub
#   ./provision.sh --wait                      # keep trying until capacity frees up
#
# Re-running is safe: it stops if the server already exists rather than
# creating a second one.

set -euo pipefail

NAME="${RUSTED_SERVER_NAME:-rusted}"
# Lists, tried in order. Hetzner's cheap lines sell out in waves, so naming one
# type in one location is how provisioning fails for reasons that have nothing
# to do with you. Any of these runs rusted — releases cover both architectures
# and the Dockerfile picks the right one.
TYPES="${RUSTED_SERVER_TYPE:-cx22 cax11 cpx11 cx32 cpx21}"
LOCATIONS="${RUSTED_SERVER_LOCATION:-fsn1 nbg1 hel1 ash hil sin}"
IMAGE="${RUSTED_SERVER_IMAGE:-ubuntu-24.04}"
WAIT_SECONDS="${RUSTED_WAIT:-0}"

key_file=""
for arg in "$@"; do
	case "$arg" in
		--wait) WAIT_SECONDS=3600 ;;
		--wait=*) WAIT_SECONDS="${arg#--wait=}" ;;
		*) key_file="$arg" ;;
	esac
done
key_file="${key_file:-$HOME/.ssh/id_ed25519.pub}"

die() { printf 'provision: %s\n' "$*" >&2; exit 1; }

command -v hcloud >/dev/null || die "hcloud CLI not found — brew install hcloud"
[ -n "${HCLOUD_TOKEN:-}" ] || die "set HCLOUD_TOKEN (a token scoped to a project used only for rusted)"
[ -f "$key_file" ] || die "no SSH public key at $key_file"

if hcloud server describe "$NAME" >/dev/null 2>&1; then
	die "a server named $NAME already exists — delete it first, or set RUSTED_SERVER_NAME"
fi

# --- ssh key -----------------------------------------------------------------
key_name="${NAME}-admin"
if ! hcloud ssh-key describe "$key_name" >/dev/null 2>&1; then
	hcloud ssh-key create --name "$key_name" --public-key-from-file "$key_file"
fi

# --- firewall ----------------------------------------------------------------
# Outside the VM, so it holds even if the machine is misconfigured. SSH is
# limited to the address provisioning from; 80/443 stay open for Cloudflare.
me="$(curl -fsSL https://ipv4.icanhazip.com 2>/dev/null || echo 0.0.0.0)/32"
fw_name="${NAME}-fw"
if ! hcloud firewall describe "$fw_name" >/dev/null 2>&1; then
	hcloud firewall create --name "$fw_name"
	hcloud firewall add-rule "$fw_name" --direction in --protocol tcp --port 22 \
		--source-ips "$me" --description "ssh from provisioning host"
	hcloud firewall add-rule "$fw_name" --direction in --protocol tcp --port 80 \
		--source-ips 0.0.0.0/0 --source-ips ::/0 --description http
	hcloud firewall add-rule "$fw_name" --direction in --protocol tcp --port 443 \
		--source-ips 0.0.0.0/0 --source-ips ::/0 --description https
fi

# --- cloud-init --------------------------------------------------------------
user_data="$(mktemp)"
trap 'rm -f "$user_data"' EXIT
# Substituted rather than templated, so cloud-init.yaml stays valid on its own
# and can be read before it ever runs.
awk -v key="$(cat "$key_file")" \
	'{ gsub(/SSH_PUBLIC_KEY_PLACEHOLDER/, key); print }' \
	"$(dirname "$0")/cloud-init.yaml" > "$user_data"

# --- create ------------------------------------------------------------------
# Hetzner reports unavailability at creation time, so attempting is the only
# reliable signal: a datacenter can list a type it won't currently sell you.
created_type=""
created_location=""

attempt() {
	local type="$1" location="$2" out
	if out="$(hcloud server create --name "$NAME" --type "$type" --image "$IMAGE" \
		--location "$location" --ssh-key "$key_name" --firewall "$fw_name" \
		--user-data-from-file "$user_data" --label role=rusted 2>&1)"; then
		created_type="$type"
		created_location="$location"
		return 0
	fi
	# A real problem — bad token, quota, name taken — is not something waiting
	# will fix, so only capacity messages fall through to the next candidate.
	if printf '%s' "$out" | grep -qiE 'unavailable|not available|sold|no_space'; then
		printf '  %-7s %-5s unavailable\n' "$type" "$location" >&2
		return 1
	fi
	die "$out"
}

search() {
	local location type
	for location in $LOCATIONS; do
		for type in $TYPES; do
			if attempt "$type" "$location"; then
				return 0
			fi
		done
	done
	return 1
}

echo "looking for capacity: ${TYPES// /, } in ${LOCATIONS// /, }"
deadline=$(( $(date +%s) + WAIT_SECONDS ))
until search; do
	if [ "$(date +%s)" -ge "$deadline" ]; then
		die "no capacity for any of those right now.
  Hetzner sells out in waves — this usually clears within hours.
    ./provision.sh --wait          keep trying for an hour
    ./provision.sh --wait=21600    keep trying for six
  Or widen the search with RUSTED_SERVER_TYPE / RUSTED_SERVER_LOCATION, or use
  another host: the deploy is plain docker compose and runs anywhere."
	fi
	echo "  nothing free; retrying in 60s"
	sleep 60
done

ip="$(hcloud server ip "$NAME")"
cat <<NEXT

created. IPv4: $ip  ($created_type in $created_location)

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
