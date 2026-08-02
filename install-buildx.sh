#!/usr/bin/env bash
# Install Docker BuildX (BuildKit frontend) on Debian/Ubuntu, Arch/Manjaro,
# or any distro (falls back to downloading a static binary into
# ~/.docker/cli-plugins).
#
# Usage:  ./install-buildx.sh [--force] [--buildx-dir DIR]
#   --force         reinstall even if buildx is already present
#   --buildx-dir    where to install on the fallback path (default:
#                   $HOME/.docker/cli-plugins). OWN_CLI_UNUSED
#
# Idempotent: does nothing if a working `docker buildx` is already on PATH
# (unless --force). Safe to run on any distro.
#
set -euo pipefail

# ---------- helpers ----------
info() { printf '\033[1;34m[i]\033[0m %s\n' "$*"; }
ok() { printf '\033[1;32m[ok]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[w]\033[0m %s\n' "$*"; }
die() {
	printf '\033[1;31m[err]\033[0m %s\n' "$*" >&2
	exit 1
}

FORCE=0
BUILDX_DIR="${BUILDX_DIR:-$HOME/.docker/cli-plugins}"
while [[ $# -gt 0 ]]; do
	case "$1" in
	--force) FORCE=1 ;;
	--buildx-dir)
		shift
		BUILDX_DIR="$1"
		;;
	-h | --help)
		sed -n '2,15p' "$0"
		exit 0
		;;
	*) die "unknown arg: $1" ;;
	esac
	shift
done

# ---------- detect distro ----------
distro_id() {
	local id=""
	[[ -r /etc/os-release ]] && id="$(. /etc/os-release && echo "${ID:-}" 2>/dev/null)"
	[[ -z "$id" ]] && [[ -f /etc/arch-release ]] && id="arch"
	[[ -z "$id" ]] && [[ -f /etc/debian_version ]] && id="debian"
	echo "${id,,}"
}
ID="$(distro_id)"

# ---------- already installed? ----------
have_buildx() { command -v docker-buildx >/dev/null 2>&1 || docker buildx version >/dev/null 2>&1; }
if have_buildx; then
	if [[ "$FORCE" -eq 0 ]]; then
		ok "Docker BuildX already installed: $(docker buildx version 2>/dev/null | head -1)"
		exit 0
	fi
	warn "BuildX already present; --force given, reinstalling."
fi

install_fallback() {
	info "Fallback: downloading static buildx binary into '$BUILDX_DIR'"
	mkdir -p "$BUILDX_DIR"
	local ver arch
	ver="$(curl -fsSL https://api.github.com/repos/docker/buildx/releases/latest |
		sed -n 's/.*"tag_name": *"v\([0-9.]*\)".*/\1/p' | head -1)"
	arch="$(uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')"
	[[ -z "$ver" ]] && die "could not determine latest buildx version"
	local url="https://github.com/docker/buildx/releases/download/v${ver}/buildx-v${ver}.linux-${arch}"
	info "Downloading $url"
	curl -fsSL -o "$BUILDX_DIR/docker-buildx" "$url"
	chmod +x "$BUILDX_DIR/docker-buildx"
}

# ---------- distro-specific path ----------
case "$ID" in
arch | manjaro | cachyos | endeavouros | artix | arcolinux | garuda | archlinux)
	info "Distro: $ID — installing from official Arch repos (pacman)"
	if ! command -v sudo >/dev/null; then
		pacman -S --noconfirm --needed docker-buildx
	else
		sudo pacman -S --noconfirm --needed docker-buildx
	fi
	;;

debian | ubuntu | linuxmint | pop | debian-esr | raspbian)
	info "Distro: $ID — installing docker-buildx-plugin via Docker apt repo"
	if command -v docker-buildx >/dev/null 2>&1; then
		ok "buildx already on PATH"
	elif dpkg -s docker-buildx-plugin >/dev/null 2>&1; then
		ok "package docker-buildx-plugin already installed"
	else
		# Ensure Docker's apt repo is present, then install the plugin package.
		codename="$(. /etc/os-release && echo "${VERSION_CODENAME:-}" 2>/dev/null)"
		arch_deb="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
		if [[ -z "$codename" ]]; then
			warn "no VERSION_CODENAME — using fallback binary install"
			install_fallback
		else
			sudo install -m 0755 -d /etc/apt/keyrings
			keyring=/etc/apt/keyrings/docker.asc
			if [[ ! -f "$keyring" ]]; then
				sudo curl -fsSL https://download.docker.com/linux/debian/gpg \
					-o "$keyring"
				sudo chmod a+r "$keyring"
			fi
			src="/etc/apt/sources.list.d/docker.list"
			if ! grep -q "download.docker.com" "$src" 2>/dev/null; then
				echo "deb [arch=${arch_deb} signed-by=${keyring}] https://download.docker.com/linux/debian ${codename} stable" |
					sudo tee "$src" >/dev/null
			fi
			sudo apt-get update -qq
			sudo apt-get install -y docker-buildx-plugin
		fi
	fi
	;;

fedora | centos | rhel | rocky | almalinux)
	info "Distro: $ID — installing docker-buildx-plugin via dnf"
	sudo dnf -y install docker-buildx-plugin
	;;

*)
	warn "Unknown distro '${ID:-?}' — falling back to static binary"
	install_fallback
	;;
esac

# ---------- verify ----------
if command -v docker-buildx >/dev/null 2>&1 || docker buildx version >/dev/null 2>&1; then
	ok "Docker BuildX ready: $(docker buildx version 2>/dev/null | head -1)"
else
	die "buildx install finished but 'docker buildx' is not working — see logs above"
fi
