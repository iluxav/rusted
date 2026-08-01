#!/bin/sh
# Install the rusted CLI.
#
#   curl -fsSL https://raw.githubusercontent.com/iluxav/rusted/main/install.sh | sh
#
# Environment:
#   RUSTED_VERSION       version to install (default: the latest release)
#   RUSTED_INSTALL_DIR   where to put the binary (default: ~/.local/bin)
#   RUSTED_DOWNLOAD_BASE where releases live (default: GitHub; set this to
#                        install from an internal mirror)

set -eu

REPO="${RUSTED_REPO:-iluxav/rusted}"
INSTALL_DIR="${RUSTED_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

need() {
	command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

need curl
need tar

# --------------------------------------------------------------- platform

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
	Darwin/arm64)         target="aarch64-apple-darwin" ;;
	Darwin/x86_64)        target="x86_64-apple-darwin" ;;
	Linux/x86_64|Linux/amd64)  target="x86_64-unknown-linux-gnu" ;;
	Linux/aarch64|Linux/arm64) target="aarch64-unknown-linux-gnu" ;;
	*) die "no prebuilt binary for $os $arch — build from source: cargo install --path crates/rusted-cli" ;;
esac

# --------------------------------------------------------------- version

version="${RUSTED_VERSION:-}"
if [ -z "$version" ]; then
	version="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" |
		sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
	[ -n "$version" ] || die "cannot determine the latest release of $REPO"
fi
case "$version" in v*) ;; *) version="v$version" ;; esac

say "installing rusted $version ($target)"

# --------------------------------------------------------------- download

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

base="${RUSTED_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}/$version"
archive="rusted-$target.tar.gz"

curl -fsSL "$base/$archive" -o "$tmp/$archive" ||
	die "no build for $target in $version — see https://github.com/$REPO/releases"

# Checksums are published per release; verify when the tool is available.
if curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" 2>/dev/null; then
	expected="$(grep " $archive\$" "$tmp/SHA256SUMS" | awk '{print $1}')"
	if [ -n "$expected" ]; then
		if command -v sha256sum >/dev/null 2>&1; then
			actual="$(sha256sum "$tmp/$archive" | awk '{print $1}')"
		elif command -v shasum >/dev/null 2>&1; then
			actual="$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')"
		else
			actual=""
			say "  (no checksum tool available, skipping verification)"
		fi
		if [ -n "$actual" ] && [ "$actual" != "$expected" ]; then
			die "checksum mismatch for $archive — refusing to install"
		fi
	fi
fi

tar xzf "$tmp/$archive" -C "$tmp"
binary="$tmp/rusted-$target/rusted"
[ -f "$binary" ] || die "the archive did not contain a rusted binary"

# --------------------------------------------------------------- install

mkdir -p "$INSTALL_DIR"
install -m 755 "$binary" "$INSTALL_DIR/rusted" 2>/dev/null ||
	{ cp "$binary" "$INSTALL_DIR/rusted" && chmod 755 "$INSTALL_DIR/rusted"; }

say "installed $INSTALL_DIR/rusted"
"$INSTALL_DIR/rusted" --version || true

case ":$PATH:" in
	*":$INSTALL_DIR:"*) ;;
	*)
		say ""
		say "$INSTALL_DIR is not on your PATH. Add it:"
		say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc && exec zsh"
		;;
esac

say ""
say "next: rusted run index.js    (serves a function locally, nothing else needed)"
