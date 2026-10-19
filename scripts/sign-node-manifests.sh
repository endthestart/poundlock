#!/usr/bin/env bash
# Signs a node update manifest for every <os>-<arch>/poundlock under DIR.
#
#   POUNDLOCK_UPDATE_SIGNING_KEY='<key>' bash scripts/sign-node-manifests.sh DIR VERSION
#
# The key is an Ed25519 private key as a PKCS#8 PEM, or that PEM base64-encoded
# on one line (the form kept in CI secrets and the macOS Keychain).
#
# Beside each binary it writes poundlock.manifest.json, the exact bytes that are
# signed ({"os","arch","version","sha256"}), and poundlock.manifest.sig, the
# base64 Ed25519 signature. The controller relays both; a node built with the
# matching public key installs nothing else. The private key is read from the
# environment and never written to disk. When POUNDLOCK_UPDATE_PUBLIC_KEY is
# also set, the signing key must match it. Requires OpenSSL 3 (not LibreSSL).
# See docs/node-update-signing.md.
set -euo pipefail

fail() { echo "sign-node-manifests: $*" >&2; exit 1; }

dir=${1:-}
version=${2:-}
[[ -n "$dir" && -n "$version" ]] || fail 'usage: sign-node-manifests.sh DIR VERSION'
[[ -d "$dir" ]] || fail "$dir is not a directory"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] || fail "invalid version: $version"
[[ -n "${POUNDLOCK_UPDATE_SIGNING_KEY:-}" ]] || fail 'POUNDLOCK_UPDATE_SIGNING_KEY is not set'
case "$(openssl version)" in
  'OpenSSL 3.'*|'OpenSSL 4.'*) ;;
  *) fail "OpenSSL 3 or later is required, found: $(openssl version)" ;;
esac

case "$POUNDLOCK_UPDATE_SIGNING_KEY" in
  -----BEGIN*) pem=$POUNDLOCK_UPDATE_SIGNING_KEY ;;
  *) pem=$(printf '%s' "$POUNDLOCK_UPDATE_SIGNING_KEY" | tr -d ' \n\r' | openssl base64 -d -A 2>/dev/null) \
       || fail 'POUNDLOCK_UPDATE_SIGNING_KEY is neither a PEM key nor base64' ;;
esac
key() { printf '%s\n' "$pem"; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

key | openssl pkey -pubout -out "$work/public.pem" 2>/dev/null \
  || fail 'POUNDLOCK_UPDATE_SIGNING_KEY is not a PEM private key'
openssl pkey -pubin -in "$work/public.pem" -text -noout | grep -q '^ED25519 Public-Key' \
  || fail 'POUNDLOCK_UPDATE_SIGNING_KEY is not an Ed25519 key'
public=$(openssl pkey -pubin -in "$work/public.pem" -outform DER | tail -c 32 | openssl base64 -A)
if [[ -n "${POUNDLOCK_UPDATE_PUBLIC_KEY:-}" && "$public" != "$POUNDLOCK_UPDATE_PUBLIC_KEY" ]]; then
  fail 'the signing key does not match POUNDLOCK_UPDATE_PUBLIC_KEY'
fi

signed=0
for binary in "$dir"/*/poundlock; do
  [[ -f "$binary" && ! -L "$binary" ]] || continue
  folder=$(dirname "$binary")
  platform=$(basename "$folder")
  case "$platform" in
    linux-x86_64|linux-aarch64|macos-x86_64|macos-aarch64) ;;
    *) echo "sign-node-manifests: skipping unknown platform $platform" >&2; continue ;;
  esac
  sha256=$(openssl dgst -sha256 -r "$binary" | cut -d' ' -f1)
  [[ "$sha256" =~ ^[0-9a-f]{64}$ ]] || fail "could not hash $binary"
  manifest="$folder/poundlock.manifest.json"
  printf '{"os":"%s","arch":"%s","version":"%s","sha256":"%s"}' \
    "${platform%%-*}" "${platform#*-}" "$version" "$sha256" > "$manifest"
  key | openssl pkeyutl -sign -rawin -inkey /dev/stdin -in "$manifest" -out "$work/signature" \
    || fail "could not sign $manifest"
  openssl pkeyutl -verify -rawin -pubin -inkey "$work/public.pem" -in "$manifest" \
    -sigfile "$work/signature" >/dev/null || fail "signature check failed for $manifest"
  openssl base64 -A -in "$work/signature" > "$folder/poundlock.manifest.sig"
  echo "sign-node-manifests: signed $platform $version $sha256"
  signed=$((signed + 1))
done
[[ "$signed" -gt 0 ]] || fail "no node binaries found under $dir"
echo "sign-node-manifests: public key $public"
