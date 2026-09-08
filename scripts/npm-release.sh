#!/usr/bin/env bash
# Publish anomalift to npm: five platform packages, then the wrapper.
#
#   scripts/npm-release.sh --assets ./dist            # publish for real
#   scripts/npm-release.sh --assets ./dist --dry-run  # pack and inspect only
#   scripts/npm-release.sh --assets ./dist --otp 123456
#
# An account with 2FA enabled - which any account owning a published name
# should have - needs a one-time code. Six publishes run inside one code's
# validity window, so pass a code that has just appeared rather than one about
# to expire. A granular access token with "bypass 2FA" avoids the whole problem
# and is what the release workflow uses.
#
# `--assets` is a directory holding the release artifacts named exactly as the
# release workflow produces them. Download them from the GitHub Release:
#
#   gh release download v0.1.0 --dir dist
#
# Order matters and is not cosmetic. The wrapper pins its optional dependencies
# to an exact version, so publishing it first opens a window in which
# `npm install anomalift` resolves to a version whose platform packages do not
# exist yet, and every install in that window fails.

set -euo pipefail

ASSETS=""
DRY_RUN=0
OTP=""

while [ $# -gt 0 ]; do
  case "$1" in
    --assets)  ASSETS="${2:?--assets needs a directory}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --otp)     OTP="${2:?--otp needs a code}"; shift 2 ;;
    *) echo "usage: $0 --assets <dir> [--dry-run] [--otp <code>]" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

[ -n "$ASSETS" ] || { echo "error: --assets <dir> is required" >&2; exit 2; }
[ -d "$ASSETS" ] || { echo "error: no such directory: $ASSETS" >&2; exit 2; }
ASSETS="$(cd "$ASSETS" && pwd)"

say() { printf '  %s\n' "$1"; }
die() { printf '\n  error: %s\n\n' "$1" >&2; exit 1; }

# --- who is publishing ---------------------------------------------------

if [ "$DRY_RUN" -eq 0 ]; then
  who="$(npm whoami 2>/dev/null || true)"
  [ -n "$who" ] || die "not logged in to npm. Run \`npm login\` first, then re-run this."
  say "publishing as $who"
fi

version="$(node -p "require('./npm/package.json').version")"
say "version $version"

# --- the assets must all be present before anything is published ---------
#
# A half-published release cannot be undone: npm refuses to reuse a version
# number even after `npm unpublish`. So every check happens before the first
# publish, never between them.

platforms="darwin-arm64:anomalift-macos-arm64:anomalift
darwin-x64:anomalift-macos-x86_64:anomalift
linux-x64:anomalift-linux-x86_64:anomalift
linux-arm64:anomalift-linux-arm64:anomalift
win32-x64:anomalift-windows-x86_64.exe:anomalift.exe"

echo "$platforms" | while IFS=: read -r pkg asset exe; do
  [ -f "$ASSETS/$asset" ] || die "missing release asset: $ASSETS/$asset
     Download the whole set first:  gh release download v$version --dir $ASSETS"
done

# Version agreement across all six manifests. A mismatch means npm installs no
# platform package at all and every user meets "platform package is not
# installed" - after the version is public.
for m in npm/package.json npm/platforms/*/package.json; do
  v="$(node -p "require('$root/$m').version")"
  [ "$v" = "$version" ] || die "$m says $v, npm/package.json says $version"
done
say "six manifests agree on $version"

# --- stage the binaries --------------------------------------------------

echo "$platforms" | while IFS=: read -r pkg asset exe; do
  dest="npm/platforms/$pkg/bin"
  mkdir -p "$dest"
  cp "$ASSETS/$asset" "$dest/$exe"
  # npm preserves the mode bits it finds in the tarball. A binary published
  # without the executable bit fails with EACCES on every user's machine.
  chmod +x "$dest/$exe"
  say "staged $pkg <- $asset"
done

# --- refuse to publish a binary that carries this machine's paths --------
#
# The compiler bakes absolute source paths into panic locations. A binary built
# on a laptop without `--remap-path-prefix` therefore contains the maintainer's
# home directory, and `npm publish` would put that name in front of everyone.
# The release workflow remaps them; this is the check that the artifacts came
# from there and not from a local `cargo build`.

echo "$platforms" | while IFS=: read -r pkg asset exe; do
  if LC_ALL=C grep -q -- "$HOME" "npm/platforms/$pkg/bin/$exe" 2>/dev/null; then
    die "$asset contains this machine's home directory in its embedded paths.
     Build it through the release workflow, or rebuild with:
       RUSTFLAGS=\"--remap-path-prefix=\$HOME=/build --remap-path-prefix=\$PWD=/src\" cargo build --release"
  fi
done
say "no local paths embedded in the binaries"

# --- inspect every tarball before any of them leaves ---------------------
#
# `npm publish` is effectively irreversible, so what goes in the tarball is
# checked here rather than discovered later. Each package declares a `files`
# allow-list; this asserts the result of that allow-list, because a stray
# .anomalift/ or a local scratch file inside a package directory would
# otherwise be published verbatim.

check_contents() {
  local dir="$1" expected="$2"
  local listing
  listing="$(cd "$dir" && npm pack --dry-run --json 2>/dev/null \
    | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{
        const f=JSON.parse(s)[0].files.map(x=>x.path).sort();
        console.log(f.join("\n"));})')"
  if [ "$listing" != "$expected" ]; then
    printf '\n  tarball for %s contains:\n%s\n\n  expected:\n%s\n' \
      "$dir" "$listing" "$expected" >&2
    die "unexpected files in the tarball - refusing to publish"
  fi
  say "$(basename "$dir") tarball: $(echo "$listing" | tr '\n' ' ')"
}

echo "$platforms" | while IFS=: read -r pkg asset exe; do
  check_contents "npm/platforms/$pkg" "$(printf 'bin/%s\npackage.json' "$exe")"
done
check_contents npm "$(printf 'README.md\nbin/anomalift.js\npackage.json')"

if [ "$DRY_RUN" -eq 1 ]; then
  printf '\n  dry run - nothing published\n\n'
  exit 0
fi

# --- publish: platforms first, wrapper last ------------------------------

otp_args=""
[ -n "$OTP" ] && otp_args="--otp=$OTP"

echo "$platforms" | while IFS=: read -r pkg asset exe; do
  say "publishing anomalift-$pkg"
  # shellcheck disable=SC2086
  (cd "npm/platforms/$pkg" && npm publish --access public $otp_args)
done

say "publishing anomalift"
# shellcheck disable=SC2086
(cd npm && npm publish --access public $otp_args)

printf '\n  published %s\n\n  Verify from an empty directory:\n\n    npx --yes anomalift@%s --version\n\n' \
  "$version" "$version"
