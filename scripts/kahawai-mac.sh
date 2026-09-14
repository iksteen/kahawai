#!/usr/bin/env bash
# Deploy kahawai to the macOS satellite, and provision the code-signing
# identity that keeps macOS from revoking its Local Network permission
# on every rebuild.
#
#   kahawai-mac.sh deploy [host]     # from the dev box: sync, build, sign, restart
#   kahawai-mac.sh setup             # ON the mac, once: create the signing identity
#
# The mac runs TWO launchd daemons from this tree:
#
# GStreamer here is a PATCHED Homebrew keg, not plugins staged beside the
# system's: see HomebrewFormula/kahawai-gstreamer.rb. Staging was tried
# and cannot work on macOS, where a dylib is identified by its path, so a
# patched copy beside Homebrew's means both are mapped and one dies on a
# null vtable.
#
#   all-in-one  its own hub, with the VideoToolbox transcoder in process
#               and no local collections — the media comes from silence's
#               mediahost over the LAN, which is why the box sits next to
#               the NAS.
#   transcoder  unchanged: still dials the dev box's hub, so that hub
#               keeps a VideoToolbox encoder. An in-process transcoder
#               serves only its own hub, so this stays a second process.
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
AIO_AGENT=org.thegraveyard.kahawai.all-in-one
# Both daemons read it; the all-in-one dies in a KeepAlive loop without one.
MAC_CONFIG="$HOME/.config/kahawai/kahawai.toml"

# The transcoder runs as a launchd DAEMON, not an agent: daemons are
# auto-allowed by Local Network privacy (TN3179 — the self-signed
# identity can NOT hold that grant; only Apple-issued ones are
# signature-tracked, everything else keys on the per-build LC_UUID) and
# start at boot without a login session. VideoToolbox hw encode and the
# GL tone-map segment both verified under the daemon (2026-07-31).
# Sudo happens here, once; deploys just pkill and KeepAlive respawns.
# The patched GStreamer is a keg-only Homebrew formula, built from
# HomebrewFormula/kahawai-gstreamer.rb — see that file for why the
# whole stack is patched rather than a handful of plugins staged beside
# Homebrew's. The binaries link it directly, so nothing here has to put
# a plugin path in front of a daemon.
KEG="/opt/homebrew/opt/kahawai-gstreamer"

die() { echo "error: $*" >&2; exit 1; }

# One launchd daemon. $1 label, $2 log file, $3.. the ProgramArguments.
#
# Regenerated rather than left alone once present, so a satellite can
# acquire a setting it did not have on the day it was first provisioned.
# There is no GST_PLUGIN_PATH any more: the binaries link the patched
# keg, so the plugins that come with it are the ones they load.
write_daemon() {
    local label="$1" log="$2"
    shift 2
    local plist="/Library/LaunchDaemons/$label.plist"
    # Retire a pre-daemon user agent so two supervisors never race.
    launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$label.plist"
    local tmpd args=""
    tmpd=$(mktemp)
    local a
    for a in "$@"; do args="$args<string>$a</string>"; done
    cat > "$tmpd" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$label</string>
    <key>ProgramArguments</key>
    <array>$args</array>
    <key>UserName</key><string>$(id -un)</string>
    <key>WorkingDirectory</key><string>$HOME</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$log</string>
    <key>StandardErrorPath</key><string>$log</string>
</dict>
</plist>
PLIST
    if [ -f "$plist" ] && diff -q "$tmpd" "$plist" >/dev/null 2>&1; then
        echo "$label: plist already current" >&2
        rm -f "$tmpd"
        return 0
    fi
    echo "installing $label (sudo)" >&2
    sudo install -o root -g wheel -m 644 "$tmpd" "$plist"
    # bootout before bootstrap: bootstrap alone refuses a label already
    # loaded, and a plist edit does not reach a running job.
    sudo launchctl bootout "system/$label" 2>/dev/null || true
    sudo launchctl bootstrap system "$plist"
    rm -f "$tmpd"
    echo "$label: installed and started" >&2
}

install_daemon() {
    local bin="$HOME/kahawai-src/target/release"
    # RunAtLoad + KeepAlive in the SYSTEM domain: both come up at boot,
    # with no login session and no terminal. The all-in-one notices it
    # has no tty and says so — enrollments are approved through the
    # admin API instead of by typing a code.
    # A missing config is a KeepAlive crash loop, so skip that daemon
    # rather than install one — and say which file is missing. Not fatal:
    # the transcoder half is independent and must still be provisioned.
    if [ -f "$MAC_CONFIG" ]; then
        # --config explicitly: a daemon has no cwd of its own choosing
        # and XDG resolution is the only other path to this file.
        write_daemon "$AIO_AGENT" "$HOME/kahawai-all-in-one.log" \
            "$bin/kahawai" --config "$MAC_CONFIG" all-in-one
    else
        echo "no $MAC_CONFIG — skipping the all-in-one daemon" >&2
    fi
    write_daemon "$AGENT" "$HOME/kahawai-transcoder.log" \
        "$bin/kahawai-transcoder"
}

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
        install_daemon
    else
        echo "setup INCOMPLETE: the identity exists but codesign refuses it." >&2
        echo "Open Keychain Access, find \"$IDENTITY\" in the" >&2
        echo "kahawai-signing keychain, Get Info → Trust → Code Signing: Always Trust." >&2
        rm -f "$probe"
        exit 1
    fi
    rm -f "$probe"
}

# rsync --files-from copies, it never deletes, so a file the repo REMOVED
# lives on in the satellite tree for ever. Cargo picks up stray files in a
# bin directory, so the tree eventually builds something nobody wrote:
# crates/kahawai/src/bin/kahawai-{mediahost,transcoder}d.rs were still
# there months after those moved into packages of their own, and only a
# whole-package check ever noticed. Everything in that tree is
# reproducible from the repo, so anything the manifest does not name and
# that is not a build product goes.
#
# LC_ALL=C on BOTH sides, because `comm` compares by byte order and the
# two boxes do not sort alike. The first run of this compared a
# locale-sorted manifest against a macOS sort and called 300 current
# files orphans, README.md and .gitignore among them. A delete on that
# list would have emptied the tree.
prune_orphans() {
    local host="$1" repo="$2"
    echo "==> pruning files the repo no longer has" >&2
    git -C "$repo" ls-files | LC_ALL=C sort \
        | ssh "$host" 'cat > kahawai-src/.deploy-manifest'
    ssh "$host" 'bash -s' <<'REMOTE'
set -euo pipefail
export LC_ALL=C
cd ~/kahawai-src
# Build products, and the manifest itself: everything else is the repo's.
find . -type f \
    -not -path './target/*' -not -path './web/dist/*' \
    -not -path './web/node_modules/*' -not -path './.git/*' \
    -not -name .deploy-manifest \
    | sed 's|^\./||' | sort > /tmp/kahawai-remote.$$
orphans=$(comm -23 /tmp/kahawai-remote.$$ .deploy-manifest)
rm -f /tmp/kahawai-remote.$$ .deploy-manifest
if [ -z "$orphans" ]; then
    echo "    none"
    exit 0
fi
# One name per line, never word-split: a path with a space in it must not
# become two half-paths handed to rm.
printf '%s\n' "$orphans" | while IFS= read -r f; do
    echo "    rm $f"
    rm -f "$f"
done
# A crate whose Cargo.toml is gone leaves a directory that the workspace
# glob can still match.
find . -type d -empty \
    -not -path './target/*' -not -path './web/dist/*' \
    -not -path './web/node_modules/*' -delete 2>/dev/null || true
REMOTE
}

# Tail one remote log past $3 lines until $4 shows up, then print the
# lines matching $5. Fails loudly with the new lines when it does not.
wait_for() {
    local host="$1" log="$2" mark="$3" needle="$4" show="$5"
    echo "==> waiting for '$needle' in $log" >&2
    local fresh
    for _ in $(seq 1 20); do
        sleep 2
        fresh=$(ssh "$host" "tail -n +$((mark + 1)) $log" 2>/dev/null || true)
        if grep -qE "$needle" <<<"$fresh"; then
            grep -E "$show" <<<"$fresh" | tail -2
            return 0
        fi
    done
    echo "no '$needle' since the restart; new lines:" >&2
    ssh "$host" "tail -n +$((mark + 1)) $log | tail -5" >&2
    return 1
}

deploy() {
    local host="${1:-$HOST_DEFAULT}"
    local repo; repo=$(cd "$(dirname "$0")/.." && pwd)
    echo "==> syncing source to $host" >&2
    (cd "$repo" && git ls-files | rsync -a --files-from=- . "$host:kahawai-src/")

    # The web bundle is a build product, so git ls-files never carries it,
    # and node_modules is not synced either — which means build.rs on the
    # mac skips npm entirely and would embed NOTHING, leaving a hub whose
    # UI 404s. Ship the bundle this box already built, and let
    # KAHAWAI_REQUIRE_WEB turn a missing one into a build failure rather
    # than a silently empty UI.
    [ -d "$repo/web/dist" ] || {
        echo "no web/dist — run 'npm run build' in web/ first" >&2
        return 1
    }
    rsync -a --delete "$repo/web/dist/" "$host:kahawai-src/web/dist/"
    prune_orphans "$host" "$repo"

    # Where each log ends BEFORE the restart: "link established" and "hub
    # up" are lines the previous run also wrote, and grepping the tail
    # would report a start that never happened.
    local mark aio_mark
    mark=$(ssh "$host" 'wc -l < ~/kahawai-transcoder.log 2>/dev/null || echo 0')
    # The redirection itself fails when the log does not exist yet, and
    # the shell says so on stderr before `|| echo 0` supplies the answer.
    aio_mark=$(ssh "$host" '{ wc -l < ~/kahawai-all-in-one.log; } 2>/dev/null || echo 0')

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
    ssh "$host" "IDENTITY='$IDENTITY' BUNDLE_ID='$BUNDLE_ID' AGENT='$AGENT' \
        AIO_AGENT='$AIO_AGENT' KAHAWAI_BUILD='$stamp' bash -s" <<'REMOTE'
set -euo pipefail
export PATH="$PATH:/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin"
KEYCHAIN="$HOME/Library/Keychains/kahawai-signing.keychain-db"
PASSFILE="$HOME/.config/kahawai/signing-keychain.pass"
cd ~/kahawai-src
export KAHAWAI_BUILD
# Build against the PATCHED GStreamer, not Homebrew's stock one.
#
# HomebrewFormula/kahawai-gstreamer.rb is the whole stack with
# patches/ applied, installed keg-only precisely so it does not shadow
# the system's — which means nothing finds it unless pointed at it. Miss
# this and the binaries link stock gstreamer, and any patched plugin
# beside it is a second copy of a library in one process, which on macOS
# is a crash rather than a warning.
KEG=/opt/homebrew/opt/kahawai-gstreamer
if [ -d "$KEG/lib/pkgconfig" ]; then
    export PKG_CONFIG_PATH="$KEG/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    echo "building against $(pkg-config --modversion gstreamer-1.0) from $KEG"
else
    echo "WARNING: no patched GStreamer keg at $KEG — building against the" >&2
    echo "         system's unpatched GStreamer. See HomebrewFormula/." >&2
fi
# Two binaries, because this box is two things. The lean transcoder (no
# hub, no mediahost, no Tesseract) still dials the dev box's hub; the
# everything binary runs this box's own all-in-one hub, and that one does
# need Homebrew's tesseract for the OCR tier.
export KAHAWAI_REQUIRE_WEB=1
cargo build --release -p kahawai-transcoderd \
    --bin kahawai-transcoder 2>&1 | tail -1
cargo build --release -p kahawai --bin kahawai 2>&1 | tail -1
BINS="target/release/kahawai-transcoder target/release/kahawai"
# The transcoder runs as a launchd DAEMON (system domain): daemons are
# auto-allowed by Local Network privacy (TN3179) and start at boot.
# Deploys stay sudo-free: KeepAlive respawns the process we kill.
if [ ! -f "/Library/LaunchDaemons/$AGENT.plist" ]; then
    echo "WARNING: no LaunchDaemon installed — see docs/kahawai-deployment.md" >&2
    echo "         (one-time sudo install); falling back to the user agent." >&2
fi
if security find-identity -v -p codesigning "$KEYCHAIN" 2>/dev/null | grep -q "$IDENTITY"; then
    security unlock-keychain -p "$(cat "$PASSFILE")" "$KEYCHAIN"
    # NOT --options runtime: Hardened Runtime turns on library
    # validation, which then refuses every Homebrew dylib this binary
    # links ("mapping process and mapped file (non-platform) have
    # different Team IDs") and the transcoder dies in dyld before main.
    # Hardened Runtime buys notarization, which a LAN satellite does not
    # need; the stable signing identity is the whole point here.
    for BIN in $BINS; do
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
        echo "signed $BIN: ${auth:-authority unknown}"
        echo "requirement: ${req:-unknown}"
    done
else
    # Honest about the consequence rather than silently ad-hoc: the
    # Local Network grant will need re-approving after this build.
    echo "WARNING: no signing identity (run kahawai-mac.sh setup on this mac);" >&2
    echo "         the binary stays ad-hoc signed and macOS will drop its" >&2
    echo "         Local Network permission — expect 'No route to host'." >&2
fi
# Daemon: kill and let KeepAlive respawn (kickstart on the system
# domain would need sudo). Agent fallback: kickstart as before.
if [ -f "/Library/LaunchDaemons/$AIO_AGENT.plist" ]; then
    pkill -f "[k]ahawai-src/target/release/kahawai --config" || true
else
    echo "WARNING: no all-in-one LaunchDaemon — run 'kahawai-mac.sh provision'" >&2
fi
if [ -f "/Library/LaunchDaemons/$AGENT.plist" ]; then
    pkill -f "kahawai-src/target/release/kahawai-transcoder" || true
    pkill -f "kahawai-src/target/release/kahawai transcoder" || true
else
    launchctl kickstart -k "gui/$(id -u)/$AGENT"
fi
REMOTE

    # Both, each from its own log: a deploy that brings the hub up and
    # leaves the transcoder dead reads as a success if only one is checked.
    # Waiting for a daemon that is not installed would fail for a reason
    # the deploy cannot fix, so say which it is.
    if ssh "$host" "test -f /Library/LaunchDaemons/$AIO_AGENT.plist"; then
        wait_for "$host" '~/kahawai-all-in-one.log' "$aio_mark" "hub up" "hub up" || return 1
    else
        echo "==> all-in-one daemon not installed; skipping its check" >&2
    fi
    wait_for "$host" '~/kahawai-transcoder.log' "$mark" "link established" \
        "link established|tone-map" || return 1
}

# Everything a fresh satellite needs, in the order the parts depend on
# each other: GStreamer and the patched plugins first, because the plist
# points at them; then the daemon, which needs the binary that `deploy`
# builds. Idempotent — running it on a working satellite re-verifies and
# changes only what has drifted.
provision() {
    [ "$(uname)" = Darwin ] || { echo "run provision ON the mac" >&2; exit 2; }
    [ -d "$KEG/lib/pkgconfig" ] || die "no patched GStreamer keg at $KEG.
       brew tap iksteen/kahawai https://github.com/iksteen/kahawai
       brew install --build-from-source iksteen/kahawai/kahawai-gstreamer"
    install_daemon
    echo >&2
    echo "provisioned. From the dev box: scripts/kahawai-mac.sh deploy" >&2
    echo "First run only, for the all-in-one's own hub:" >&2
    echo "  kahawai-src/target/release/kahawai --config $MAC_CONFIG hub init-admin" >&2
}

case "${1:-}" in
    setup) setup ;;
    provision) provision ;;
    daemons) [ "$(uname)" = Darwin ] || { echo "run daemons ON the mac" >&2; exit 2; }
             install_daemon ;;
    deploy) shift; deploy "${1:-}" ;;
    prune) shift
           prune_orphans "${1:-$HOST_DEFAULT}" \
               "$(cd "$(dirname "$0")/.." && pwd)" ;;
    *) echo "usage: $0 {setup|provision|daemons|deploy|prune [host]}" >&2
       echo "  setup      ON the mac, once: signing identity, then provision" >&2
       echo "  provision  ON the mac: both launchd daemons (sudo)" >&2
       echo "  daemons    ON the mac: just the launchd daemons (sudo)" >&2
       echo "  deploy     FROM the dev box: sync, build, sign, restart" >&2
       echo "  prune      FROM the dev box: delete satellite files the repo dropped" >&2
       exit 2 ;;
esac
