#!/bin/sh
# Install zstats from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/vicanso/zstats/main/install.sh | sh
#
# Environment:
#   ZSTATS_VERSION      tag to install, e.g. v0.2.0 (default: latest release)
#   ZSTATS_INSTALL_DIR  destination directory (default: /usr/local/bin)
#
# POSIX sh on purpose: this has to run under dash and busybox ash too.

set -eu

REPO="vicanso/zstats"
VERSION="${ZSTATS_VERSION:-latest}"
INSTALL_DIR="${ZSTATS_INSTALL_DIR:-/usr/local/bin}"

die() {
    echo "error: $*" >&2
    exit 1
}

# --- work out which asset this machine needs -------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
    Darwin/arm64) asset="zstats-darwin-aarch64" ;;
    Darwin/x86_64) asset="zstats-darwin-x86" ;;
    Linux/aarch64 | Linux/arm64) asset="zstats-linux-musl-aarch64" ;;
    Linux/x86_64) asset="zstats-linux-musl-x86" ;;
    *) die "no prebuilt binary for $os/$arch — build from source: cargo install --path ." ;;
esac
tarball="$asset.tar.gz"

# GitHub redirects /releases/latest/download to the newest non-prerelease,
# which is what skips the rolling nightly
if [ "$VERSION" = "latest" ]; then
    base_url="https://github.com/$REPO/releases/latest/download"
else
    base_url="https://github.com/$REPO/releases/download/$VERSION"
fi

# --- download --------------------------------------------------------------
if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    die "need curl or wget"
fi

tmp="$(mktemp -d)"
# Clean up on every exit path, including the failures below
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $tarball ($VERSION)..."
fetch "$base_url/$tarball" "$tmp/$tarball" || die "download failed: $base_url/$tarball"

# --- verify ----------------------------------------------------------------
# The .sha256 is `shasum -a 256` output, so it names the tarball and must be
# checked from the directory holding it
if fetch "$base_url/$tarball.sha256" "$tmp/$tarball.sha256" 2>/dev/null; then
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$tmp" && sha256sum -c "$tarball.sha256" >/dev/null) ||
            die "checksum mismatch — refusing to install"
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$tmp" && shasum -a 256 -c "$tarball.sha256" >/dev/null) ||
            die "checksum mismatch — refusing to install"
    else
        echo "warning: no sha256 tool found, skipping checksum verification" >&2
    fi
    echo "Checksum OK"
else
    echo "warning: no published checksum for $tarball, skipping verification" >&2
fi

# --- unpack ----------------------------------------------------------------
tar -xzf "$tmp/$tarball" -C "$tmp" || die "failed to unpack $tarball"

# Releases ship the binary renamed to the asset name; accept a plain
# `zstats` too so a future packaging change does not break this script
if [ -f "$tmp/$asset" ]; then
    binary="$tmp/$asset"
elif [ -f "$tmp/zstats" ]; then
    binary="$tmp/zstats"
else
    binary="$(find "$tmp" -type f -name 'zstats*' ! -name '*.tar.gz' ! -name '*.sha256' | head -1)"
    [ -n "$binary" ] || die "no zstats binary inside $tarball"
fi
chmod +x "$binary"

# --- install ---------------------------------------------------------------
sudo=""
if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR" 2>/dev/null || sudo="sudo"
fi
if [ -z "$sudo" ] && [ ! -w "$INSTALL_DIR" ]; then
    sudo="sudo"
fi
if [ -n "$sudo" ]; then
    command -v sudo >/dev/null 2>&1 ||
        die "$INSTALL_DIR is not writable and sudo is unavailable — set ZSTATS_INSTALL_DIR"
    echo "Installing to $INSTALL_DIR (needs sudo)..."
    $sudo mkdir -p "$INSTALL_DIR"
else
    echo "Installing to $INSTALL_DIR..."
fi
$sudo install -m 755 "$binary" "$INSTALL_DIR/zstats" ||
    die "failed to install into $INSTALL_DIR"

# --- report ----------------------------------------------------------------
installed="$("$INSTALL_DIR/zstats" --version 2>/dev/null || echo "zstats (version unknown)")"
echo "Installed $installed -> $INSTALL_DIR/zstats"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "note: $INSTALL_DIR is not in PATH; add it to your shell profile" >&2 ;;
esac

# An already-running daemon keeps executing the old binary until restarted.
# Say so rather than restarting it — that is the user's call.
if pgrep -x zstats >/dev/null 2>&1; then
    echo "note: a zstats daemon is still running the previous binary — restart it with:"
    echo "      zstats stop && zstats serve"
fi
