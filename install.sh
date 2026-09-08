#!/bin/sh
# Install anomalift on macOS or Linux. No Rust, no package manager, no root.
#
#   curl -fsSL https://raw.githubusercontent.com/kailash16dev/Anomalift/main/install.sh | sh
#
# POSIX sh on purpose: /bin/sh is dash on Debian and Ubuntu, and a bashism here
# fails on exactly the machines least likely to have anything else installed.

set -eu

REPO="${ANOMALIFT_REPO:-kailash16dev/Anomalift}"
VERSION="${ANOMALIFT_VERSION:-latest}"
BIN_DIR="${ANOMALIFT_BIN_DIR:-$HOME/.local/bin}"

say()  { printf '  %s\n' "$1"; }
die()  { printf '\n  error: %s\n\n' "$1" >&2; exit 1; }

need() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but not installed"
}

# --- what are we on ------------------------------------------------------

os=$(uname -s)
arch=$(uname -m)

case "$os" in
  Darwin) os_name=macos ;;
  Linux)  os_name=linux ;;
  *) die "unsupported operating system: $os (Windows users: use install.ps1)" ;;
esac

case "$arch" in
  arm64|aarch64) arch_name=arm64 ;;
  x86_64|amd64)  arch_name=x86_64 ;;
  *) die "unsupported architecture: $arch" ;;
esac

asset="anomalift-${os_name}-${arch_name}"

# Only fires when the *calling shell* is itself translated - an Intel Homebrew
# terminal, say. A normal `curl | sh` on Apple Silicon runs native and reports
# arm64 already, so this is a narrow safety net, not the general path.
if [ "$os_name" = macos ] && [ "$arch_name" = x86_64 ]; then
  if sysctl -n sysctl.proc_translated 2>/dev/null | grep -q 1; then
    say "detected Rosetta; installing the arm64 build instead"
    asset="anomalift-macos-arm64"
  fi
fi

need curl
need mkdir

# --- download ------------------------------------------------------------

if [ "$VERSION" = latest ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t anomalift)
# Clean up whatever happens, including a failed download half-written to disk.
trap 'rm -rf "$tmp"' EXIT INT TERM

printf '\n  anomalift\n\n'
say "downloading ${asset}"
curl -fsSL "$url" -o "$tmp/anomalift" \
  || die "download failed: $url
     If this is a fresh repository, there may be no release yet."

# --- verify --------------------------------------------------------------
#
# A piped installer already asks for a lot of trust. Verifying the published
# checksum is the least it can do; a mismatch means stop, never "probably fine".

if curl -fsSL "${url%/*}/${asset}.sha256" -o "$tmp/expected" 2>/dev/null; then
  if command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/anomalift" | cut -d' ' -f1)
  elif command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/anomalift" | cut -d' ' -f1)
  elif [ "${ANOMALIFT_SKIP_CHECKSUM:-0}" = 1 ]; then
    say "warning: no sha256 tool found; checksum skipped at your request"
    actual=""
    expected=""
  else
    # Never silently skip. An earlier version left `actual` empty here and fell
    # through printing nothing at all, so a user could get a completely
    # unverified binary and have no way to know it. The opt-out has to be
    # honoured on this branch too: it was named in the message below while
    # only being read on the "no checksum published" branch, so following the
    # printed instruction produced the identical error, for ever.
    die "no sha256 tool found (shasum or sha256sum) — cannot verify the download.
     Install one, or re-run with ANOMALIFT_SKIP_CHECKSUM=1 to accept the risk."
  fi
  expected=$(cut -d' ' -f1 < "$tmp/expected")
  if [ "$actual" != "$expected" ]; then
    die "checksum mismatch — refusing to install
     expected $expected
     got      $actual"
  fi
  say "checksum verified"
elif [ "${ANOMALIFT_SKIP_CHECKSUM:-0}" = 1 ]; then
  say "warning: checksum skipped at your request"
else
  # The workflow publishes a .sha256 beside every asset unconditionally, so a
  # missing one means something is wrong with the release - not a normal
  # condition to shrug at.
  die "no published checksum for ${asset} — refusing to install.
     Re-run with ANOMALIFT_SKIP_CHECKSUM=1 if you accept the risk."
fi

# --- install -------------------------------------------------------------

mkdir -p "$BIN_DIR"
chmod +x "$tmp/anomalift"

# macOS quarantines anything downloaded, and the first run is then a dialog
# saying the developer cannot be verified. Stripping the attribute here means
# a friend never meets that - it is the same trust decision they already made
# by running this script.
if [ "$os_name" = macos ]; then
  xattr -d com.apple.quarantine "$tmp/anomalift" 2>/dev/null || true
fi

mv "$tmp/anomalift" "$BIN_DIR/anomalift" \
  || die "could not write to $BIN_DIR — set ANOMALIFT_BIN_DIR to somewhere writable"

say "installed to $BIN_DIR/anomalift"

# --- PATH ----------------------------------------------------------------

case ":$PATH:" in
  *":$BIN_DIR:"*)
    printf '\n  Run it:\n\n    anomalift\n\n'
    ;;
  *)
    # Do not edit anyone's shell profile. Silently modifying a dotfile is a
    # worse surprise than one extra line of instruction.
    printf '\n  %s is not on your PATH. Add it:\n\n' "$BIN_DIR"
    printf '    echo '"'"'export PATH="%s:$PATH"'"'"' >> ~/.zshrc && exec zsh\n\n' "$BIN_DIR"
    printf '  Or run it directly:\n\n    %s/anomalift\n\n' "$BIN_DIR"
    ;;
esac
