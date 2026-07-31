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
    # NOT `local`: the EXIT trap runs after this function has returned,
    # where a function-local is out of scope and `set -u` turns the
    # cleanup into the script's last words.
    tmp=$(mktemp -d)
    trap 'rm -rf "${tmp:-}"' EXIT
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

    # The one step no script may do for you. Per `security
    # add-trusted-cert -h`: the default domain is USER (no -d) and
    # needs no root, while -d is the admin store, which writes
    # /Library/Keychains/System.keychain and therefore does. Either way
    # changing trust settings needs an authorization, so this shows a
    # dialog in a login session and simply refuses over ssh ("the
    # authorization was denied since no user interaction was possible").
    echo "granting code-signing trust — approve the dialog:" >&2
    if ! security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$tmp/cert.pem"; then
        echo "user-domain trust refused; trying the admin store (needs root):" >&2
        sudo security add-trusted-cert -d -r trustRoot -p codeSign \
            -k /Library/Keychains/System.keychain "$tmp/cert.pem"
    fi

    # Prove it: an identity that lists but cannot sign is the failure
    # mode this whole subcommand exists to avoid.
    security find-identity -v -p codesigning "$KEYCHAIN" || true
    local probe; probe=$(mktemp)
    cp /usr/bin/true "$probe"
    if codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" \
        --identifier "$BUNDLE_ID" "$probe" 2>&1; then
        echo "setup done — signing works. Deploy: scripts/kahawai-mac.sh deploy" >&2
    else
        echo "setup INCOMPLETE: the identity exists but codesign refuses it." >&2
        echo "Open Keychain Access, find \"$IDENTITY\" in the" >&2
        echo "kahawai-signing keychain, Get Info → Trust → Code Signing: Always Trust." >&2
        rm -f "$probe"
        exit 1
    fi
    rm -f "$probe"
}

deploy() {
    local host="${1:-$HOST_DEFAULT}"
    local repo; repo=$(cd "$(dirname "$0")/.." && pwd)
    echo "==> syncing source to $host" >&2
    (cd "$repo" && git ls-files | rsync -a --files-from=- . "$host:kahawai-src/")
    # web/dist is gitignored but rust-embed needs it at compile time.
    [ -d "$repo/web/dist" ] && rsync -a "$repo/web/dist/" "$host:kahawai-src/web/dist/"

    # Where the log ends BEFORE the restart: "link established" is a
    # line the previous run also wrote, and grepping the tail would
    # report a successful link that never happened.
    local mark; mark=$(ssh "$host" 'wc -l < ~/kahawai-transcoder.log 2>/dev/null || echo 0')

    echo "==> building + signing + restarting on $host" >&2
    # Only host-independent values cross the wire: KEYCHAIN and PASSFILE
    # live under $HOME, and interpolating them here would ship the DEV
    # BOX's home directory to the mac — where the keychain then never
    # exists and every deploy silently falls back to ad-hoc signing.
    # The synced tree has no .git, so the build stamp rides an env var
    # (kahawai-core/build.rs honors KAHAWAI_BUILD over git).
    local stamp
    stamp="$(git -C "$repo" rev-parse --short HEAD 2>/dev/null || echo unknown)"
    git -C "$repo" diff --quiet 2>/dev/null || stamp="$stamp+dirty"
    stamp="$stamp $(git -C "$repo" log -1 --format=%cs HEAD 2>/dev/null || true)"
    ssh "$host" "IDENTITY='$IDENTITY' BUNDLE_ID='$BUNDLE_ID' AGENT='$AGENT' KAHAWAI_BUILD='$stamp' bash -s" <<'REMOTE'
set -euo pipefail
export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin"
KEYCHAIN="$HOME/Library/Keychains/kahawai-signing.keychain-db"
PASSFILE="$HOME/.config/kahawai/signing-keychain.pass"
cd ~/kahawai-src
# Satellite build: the lean transcoder binary — no hub, no mediahost,
# no Tesseract (which Homebrew would otherwise have to provide for a
# tier that executes hub-side).
export KAHAWAI_BUILD
cargo build --release --no-default-features --features transcoder \
    --bin kahawai-transcoder 2>&1 | tail -1
BIN=target/release/kahawai-transcoder
# The launchd agent must point at the new binary (older setups ran
# `.../kahawai transcoder`); rewrite + bootstrap when it differs.
PLIST="$HOME/Library/LaunchAgents/$AGENT.plist"
# Check the PROGRAM, not the whole plist: the log path also contains
# "kahawai-transcoder" and matched a naive grep on the first try.
if [ "$(plutil -extract ProgramArguments.0 raw "$PLIST" 2>/dev/null)" \
     != "$HOME/kahawai-src/target/release/kahawai-transcoder" ]; then
    plutil -replace ProgramArguments -json \
        "[\"$HOME/kahawai-src/target/release/kahawai-transcoder\"]" "$PLIST"
    launchctl bootout "gui/$(id -u)/$AGENT" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$PLIST"
    echo "launchd agent repointed at kahawai-transcoder"
fi
if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "$IDENTITY"; then
    security unlock-keychain -p "$(cat "$PASSFILE")" "$KEYCHAIN"
    # NOT --options runtime: Hardened Runtime turns on library
    # validation, which then refuses every Homebrew dylib this binary
    # links ("mapping process and mapped file (non-platform) have
    # different Team IDs") and the transcoder dies in dyld before main.
    # Hardened Runtime buys notarization, which a LAN satellite does not
    # need; the stable signing identity is the whole point here.
    codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" \
        --identifier "$BUNDLE_ID" "$BIN"
    # Authority only appears at -dvv. Report the designated requirement
    # too, because THAT is what decides whether the Local Network grant
    # survives: an identity-and-identifier requirement does, a cdhash
    # one (ad-hoc) does not.
    #
    # Substitutions, not pipelines: `grep -m1` closes the pipe early,
    # codesign dies of SIGPIPE, and `pipefail` then aborts this script
    # between printing the line and restarting the agent.
    auth=$(codesign -dvv "$BIN" 2>&1 | grep Authority || true)
    req=$(codesign -d --requirements - "$BIN" 2>&1 | grep designated || true)
    echo "signed: ${auth:-authority unknown}"
    echo "requirement: ${req:-unknown}"
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
        local fresh
        fresh=$(ssh "$host" "tail -n +$((mark + 1)) ~/kahawai-transcoder.log" 2>/dev/null || true)
        if grep -q "link established" <<<"$fresh"; then
            grep -E "link established|tone-map" <<<"$fresh" | tail -2
            return 0
        fi
    done
    echo "link not established since the restart; new lines:" >&2
    ssh "$host" "tail -n +$((mark + 1)) ~/kahawai-transcoder.log | tail -3" >&2
    return 1
}

case "${1:-}" in
    setup) setup ;;
    deploy) shift; deploy "${1:-}" ;;
    *) echo "usage: $0 {setup|deploy [host]}" >&2; exit 2 ;;
esac
