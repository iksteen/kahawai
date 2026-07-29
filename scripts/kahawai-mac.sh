#!/usr/bin/env bash
# Deploy kahawai to the macOS satellite, and provision the code-signing
# identity that keeps macOS from revoking its Local Network permission
# on every rebuild.
#
#   kahawai-mac.sh deploy [host]     # from the dev box: sync, build, sign, restart
#   kahawai-mac.sh setup             # ON the mac, once: create the signing identity
#
# Why signing at all: the transcoder dials the hub over the LAN, so
# macOS 15+ gates it behind Local Network privacy. That grant is keyed
# to the binary's code signature — for an ad-hoc signature that is the
# cdhash, which every rebuild changes, so every rebuild silently loses
# network access ("No route to host") until someone re-approves it in
# System Settings. Signed with a stable identity, the grant keys on the
# identity instead and survives rebuilds.
#
# `setup` needs one interactive confirmation (trust settings are
# deliberately not scriptable without a password) and is why it is a
# separate subcommand rather than part of deploy.
set -euo pipefail

HOST_DEFAULT=ingmar@192.168.0.107
IDENTITY="kahawai local signing"
KEYCHAIN="$HOME/Library/Keychains/kahawai-signing.keychain-db"
PASSFILE="$HOME/.config/kahawai/signing-keychain.pass"
BUNDLE_ID=org.thegraveyard.kahawai
AGENT=org.thegraveyard.kahawai.transcoder

setup() {
    [ "$(uname)" = Darwin ] || { echo "run setup ON the mac" >&2; exit 2; }
    mkdir -p "$(dirname "$PASSFILE")" && chmod 700 "$(dirname "$PASSFILE")"
    [ -f "$PASSFILE" ] || { /usr/bin/openssl rand -hex 24 > "$PASSFILE"; chmod 600 "$PASSFILE"; }
    local pass; pass=$(cat "$PASSFILE")
    local tmp; tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    # The SYSTEM openssl, never whatever is on PATH: OpenSSL 3 writes
    # PKCS#12 with AES-256 and a SHA-256 MAC, which Security.framework
    # rejects outright ("MAC verification failed during PKCS12 import"),
    # while macOS's own LibreSSL writes what it accepts. Homebrew's
    # openssl shadows the system one in any normal login shell, so this
    # fails for a human and works over ssh — measured, both ways.
    local openssl=/usr/bin/openssl

    # A code-signing cert of our own. Self-signed is enough: nothing
    # verifies it against a chain, it only has to be STABLE.
    cat > "$tmp/ext.cnf" <<EOF
[req]
distinguished_name=dn
x509_extensions=v3
prompt=no
[dn]
CN=$IDENTITY
[v3]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
EOF
    "$openssl" req -x509 -newkey rsa:2048 -sha256 -days 7300 -nodes \
        -keyout "$tmp/key.pem" -out "$tmp/cert.pem" -config "$tmp/ext.cnf" 2>/dev/null
    # A passphrase is required: Security.framework rejects a PKCS#12
    # with an empty one ("MAC verification failed").
    "$openssl" pkcs12 -export -out "$tmp/id.p12" -inkey "$tmp/key.pem" -in "$tmp/cert.pem" \
        -name "$IDENTITY" -passout pass:import

    # Its own keychain, not the login one: this must be unlockable by a
    # build script without ever touching the user's login password.
    security delete-keychain "$KEYCHAIN" 2>/dev/null || true
    security create-keychain -p "$pass" "$KEYCHAIN"
    security set-keychain-settings -lut 21600 "$KEYCHAIN"
    security unlock-keychain -p "$pass" "$KEYCHAIN"
    security import "$tmp/id.p12" -k "$KEYCHAIN" -P import -A -T /usr/bin/codesign
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$pass" "$KEYCHAIN" >/dev/null
    # Add it to the search list without dropping what is already there.
    # shellcheck disable=SC2046
    security list-keychains -d user -s $(security list-keychains -d user | tr -d '"') "$KEYCHAIN"

    # The one interactive step: codesign refuses an identity whose
    # certificate carries no trust setting, and setting trust needs an
    # authorization no script may fake. Expect a password prompt.
    echo "granting code-signing trust — macOS will ask for your password:" >&2
    security add-trusted-cert -d -r trustRoot -p codeSign -k /Library/Keychains/System.keychain \
        "$tmp/cert.pem"

    security find-identity -v -p codesigning "$KEYCHAIN"
    echo "setup done. Deploy from the dev box: scripts/kahawai-mac.sh deploy" >&2
}

deploy() {
    local host="${1:-$HOST_DEFAULT}"
    local repo; repo=$(cd "$(dirname "$0")/.." && pwd)
    echo "==> syncing source to $host" >&2
    (cd "$repo" && git ls-files | rsync -a --files-from=- . "$host:kahawai-src/")
    # web/dist is gitignored but rust-embed needs it at compile time.
    [ -d "$repo/web/dist" ] && rsync -a "$repo/web/dist/" "$host:kahawai-src/web/dist/"

    echo "==> building + signing + restarting on $host" >&2
    ssh "$host" "IDENTITY='$IDENTITY' KEYCHAIN='$KEYCHAIN' PASSFILE='$PASSFILE' \
        BUNDLE_ID='$BUNDLE_ID' AGENT='$AGENT' bash -s" <<'REMOTE'
set -euo pipefail
export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin"
cd ~/kahawai-src
cargo build --release 2>&1 | tail -1
BIN=target/release/kahawai
if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "$IDENTITY"; then
    security unlock-keychain -p "$(cat "$PASSFILE")" "$KEYCHAIN"
    codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" \
        --identifier "$BUNDLE_ID" --options runtime "$BIN"
    echo "signed: $(codesign -dv "$BIN" 2>&1 | grep -m1 Authority || echo unknown)"
else
    # Honest about the consequence rather than silently ad-hoc: the
    # Local Network grant will need re-approving after this build.
    echo "WARNING: no signing identity (run kahawai-mac.sh setup on this mac);" >&2
    echo "         the binary stays ad-hoc signed and macOS will drop its" >&2
    echo "         Local Network permission — expect 'No route to host'." >&2
fi
launchctl kickstart -k "gui/$(id -u)/$AGENT"
REMOTE

    echo "==> waiting for the link" >&2
    for _ in $(seq 1 12); do
        sleep 2
        if ssh "$host" 'tail -5 ~/kahawai-transcoder.log' 2>/dev/null | grep -q "link established"; then
            ssh "$host" 'grep -E "link established|tone-map" ~/kahawai-transcoder.log | tail -2'
            return 0
        fi
    done
    echo "link not established; last lines:" >&2
    ssh "$host" 'tail -3 ~/kahawai-transcoder.log' >&2
    return 1
}

case "${1:-}" in
    setup) setup ;;
    deploy) shift; deploy "${1:-}" ;;
    *) echo "usage: $0 {setup|deploy [host]}" >&2; exit 2 ;;
esac
