#!/usr/bin/env bash
# Create a stable, self-signed code-signing identity ("muxterm-local") in the
# login keychain, so `make app` can sign the bundle with a fixed designated
# requirement instead of an ad-hoc signature.
#
# Why this matters: macOS TCC ("<app> would like to access data from other
# apps", triggered by muxterm installing agent lifecycle hooks into ~/.claude,
# ~/.codex and ~/.pi) remembers a grant by the app's *designated requirement*.
# An ad-hoc signature's requirement is the raw binary hash, which changes on
# every rebuild - so every `make install` makes macOS forget the grant and
# re-prompt. Signing with a persistent certificate anchors the requirement on
# the cert instead, so the grant survives rebuilds. Allow it once, never again.
#
# Idempotent: if the identity already exists this is a no-op, so re-running it
# never rotates the cert (which would itself cost one more prompt).
set -euo pipefail

NAME="muxterm-local"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"

if security find-identity -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "\"$NAME\""; then
    echo "code-signing identity \"$NAME\" already present — nothing to do"
    exit 0
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
pass="$(openssl rand -hex 12)"

cat > "$tmp/cert.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = muxterm-local
[ext]
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = critical,codeSigning
EOF

openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/cert.cnf" >/dev/null 2>&1

# OpenSSL 3+ defaults to a PKCS#12 encryption/MAC macOS `security` can't read;
# the legacy SHA1/3DES flags produce an importable file. LibreSSL (the system
# /usr/bin/openssl) already writes the compatible form and rejects those flags.
p12_flags=()
if openssl version | grep -q '^OpenSSL'; then
    p12_flags=(-legacy -macalg sha1 -certpbe PBE-SHA1-3DES -keypbe PBE-SHA1-3DES)
fi
openssl pkcs12 -export "${p12_flags[@]}" -name "$NAME" \
    -inkey "$tmp/key.pem" -in "$tmp/cert.pem" -out "$tmp/id.p12" -passout "pass:$pass"

# -T /usr/bin/codesign pre-authorizes codesign to use the key without a
# keychain GUI prompt at signing time.
security import "$tmp/id.p12" -k "$KEYCHAIN" -T /usr/bin/codesign -P "$pass"

echo "created code-signing identity \"$NAME\" in the login keychain"
echo "next: make install, then Allow the one-time macOS prompt — it will stick."
