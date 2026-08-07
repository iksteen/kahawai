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

# The transcoder runs as a launchd DAEMON, not an agent: daemons are
# auto-allowed by Local Network privacy (TN3179 — the self-signed
# identity can NOT hold that grant; only Apple-issued ones are
# signature-tracked, everything else keys on the per-build LC_UUID) and
# start at boot without a login session. VideoToolbox hw encode and the
# GL tone-map segment both verified under the daemon (2026-07-31).
# Sudo happens here, once; deploys just pkill and KeepAlive respawns.
# Where the patched plugins live. Homebrew says it plainly on upgrade:
# "Do not install plugins into GStreamer's prefix. They will be deleted
# by `brew upgrade`." So they go somewhere brew will never touch, and the
# daemon is pointed at it explicitly.
KAHAWAI_GST="$HOME/.local/lib/kahawai-gst"

# GStreamer and the patched plugins, from nothing. Run ON the mac.
gst() {
    [ "$(uname)" = Darwin ] || { echo "run gst ON the mac" >&2; exit 2; }
    local brew_bin
    for brew_bin in /opt/homebrew/bin /usr/local/bin; do
        [ -x "$brew_bin/brew" ] && break
    done
    [ -x "$brew_bin/brew" ] || { echo "Homebrew not found" >&2; exit 1; }
    export PATH="$brew_bin:$PATH"
    export HOMEBREW_NO_AUTO_UPDATE=1

    local before after
    before="$(gst-inspect-1.0 --version 2>/dev/null | awk '/^gst-inspect-1.0 version/ {print $3}')"
    echo "==> gstreamer + build tools" >&2
    brew install gstreamer meson ninja pkg-config >/dev/null 2>&1 || true
    brew upgrade gstreamer >/dev/null 2>&1 || true
    after="$(gst-inspect-1.0 --version 2>/dev/null | awk '/^gst-inspect-1.0 version/ {print $3}')"
    [ -n "$after" ] || { echo "no gstreamer after install" >&2; exit 1; }
    echo "    gstreamer ${before:-none} -> $after" >&2

    # A GStreamer upgrade moves the version-stamped Cellar path that
    # cargo baked into its cached build-script output, and the next build
    # then links against a directory that no longer exists — an error
    # that names the missing path and never mentions the upgrade. So the
    # cache goes whenever the version moved.
    if [ -n "$before" ] && [ "$before" != "$after" ] && [ -d "$HOME/kahawai-src" ]; then
        echo "==> gstreamer moved: cargo clean (stale Cellar paths)" >&2
        ( cd "$HOME/kahawai-src" && cargo clean >/dev/null 2>&1 ) || true
    fi

    local patches="$HOME/kahawai-src/patches/gstreamer"
    [ -d "$patches" ] || { echo "no $patches — run 'deploy' first" >&2; exit 1; }

    local src; src=$(mktemp -d -t kahawai-gst-src)
    echo "==> source: gstreamer $after" >&2
    git clone --depth 1 --branch "$after" \
        https://gitlab.freedesktop.org/gstreamer/gstreamer.git "$src" >/dev/null 2>&1 \
        || { echo "clone of tag $after failed" >&2; exit 1; }
    local p
    for p in "$patches"/*.patch; do
        echo "    applying $(basename "$p")" >&2
        git -C "$src" apply "$p" || {
            echo "FAILED to apply $(basename "$p") to $after" >&2; exit 1; }
    done

    # Which plugins, read from the patches. gst-plugins-bad is different:
    # 0004 edits gst-libs/codecparsers, and what needs rebuilding is
    # every plugin that LINKS it — which no patch names because none
    # edits them. nvcodec/va/v4l2codecs are Linux-only and absent here.
    local good bad_plugins="videoparsers codectimestamper mpegtsdemux"
    good=$(grep -hoE 'subprojects/gst-plugins-good/gst/[a-z0-9]+/' "$patches"/*.patch \
           | awk -F/ '{print $4}' | sort -u)
    echo "==> building: $(echo "$good" | tr '\n' ' ')| $bad_plugins" >&2

    export PKG_CONFIG_PATH="$brew_bin/../lib/pkgconfig:$brew_bin/../share/pkgconfig"
    local gb bb args=()
    gb=$(mktemp -d); bb=$(mktemp -d)
    args=(--buildtype=release -Dauto_features=disabled)
    for p in $good; do args+=("-D$p=enabled"); done
    meson setup "$gb" "$src/subprojects/gst-plugins-good" "${args[@]}" >/dev/null \
        || { echo "gst-plugins-good: setup failed" >&2; exit 1; }
    ninja -C "$gb" >/dev/null || { echo "gst-plugins-good: build failed" >&2; exit 1; }
    args=(--buildtype=release -Dauto_features=disabled)
    for p in $bad_plugins; do args+=("-D$p=enabled"); done
    meson setup "$bb" "$src/subprojects/gst-plugins-bad" "${args[@]}" >/dev/null \
        || { echo "gst-plugins-bad: setup failed" >&2; exit 1; }
    ninja -C "$bb" >/dev/null || { echo "gst-plugins-bad: build failed" >&2; exit 1; }

    # Wipe: this directory is a build product. A survivor from before a
    # system upgrade loads, looks right, and is linked against an ABI
    # that is gone.
    rm -rf "$KAHAWAI_GST"
    mkdir -p "$KAHAWAI_GST/plugins" "$KAHAWAI_GST/lib"

    # The patched codecparsers, real file only — a looser glob also
    # matches meson's .symbols artifacts.
    local lib
    lib=$(find "$bb/gst-libs" -type f -name 'libgstcodecparsers-1.0.0.dylib' ! -name '*.symbols' | head -1)
    [ -n "$lib" ] || { echo "codecparsers not built" >&2; exit 1; }
    install -m644 "$lib" "$KAHAWAI_GST/lib/"
    ln -sf libgstcodecparsers-1.0.0.dylib "$KAHAWAI_GST/lib/libgstcodecparsers-1.0.dylib"
    install_name_tool -id "$KAHAWAI_GST/lib/libgstcodecparsers-1.0.0.dylib" \
        "$KAHAWAI_GST/lib/libgstcodecparsers-1.0.0.dylib"
    codesign -f -s - "$KAHAWAI_GST/lib/libgstcodecparsers-1.0.0.dylib" 2>/dev/null

    local so b
    for so in "$gb"/gst/*/libgst*.dylib "$bb"/gst/*/libgst*.dylib; do
        [ -f "$so" ] || continue
        b=$(basename "$so")
        install -m644 "$so" "$KAHAWAI_GST/plugins/$b"
        # macOS resolves @rpath, and the recorded rpaths run
        # build-tree-then-Homebrew. Once the build tree is gone the
        # plugin silently loads HOMEBREW's unpatched codecparsers — the
        # file is installed and the patch does nothing. Pin it absolutely
        # and re-sign, because install_name_tool voids the signature.
        if otool -L "$KAHAWAI_GST/plugins/$b" | grep -q '@rpath/libgstcodecparsers'; then
            install_name_tool -change @rpath/libgstcodecparsers-1.0.0.dylib \
                "$KAHAWAI_GST/lib/libgstcodecparsers-1.0.0.dylib" "$KAHAWAI_GST/plugins/$b"
            codesign -f -s - "$KAHAWAI_GST/plugins/$b" 2>/dev/null
        fi
        echo "    installed $b" >&2
    done
    rm -rf "$src" "$gb" "$bb"

    # Prove the daemon's own setting resolves to what we just built,
    # rather than to Homebrew's copy of the same plugin.
    echo "==> loading from:" >&2
    local name
    for name in $good videoparsersbad; do
        printf '    %-16s %s\n' "$name" \
            "$(GST_PLUGIN_PATH="$KAHAWAI_GST/plugins" gst-inspect-1.0 "$name" 2>/dev/null \
               | awk '/Filename/{print $2}')" >&2
    done
}

install_daemon() {
    local plist="/Library/LaunchDaemons/$AGENT.plist"
    # Retire a pre-daemon user agent so two supervisors never race.
    launchctl bootout "gui/$(id -u)/$AGENT" 2>/dev/null || true
    rm -f "$HOME/Library/LaunchAgents/$AGENT.plist"
    tmpd=$(mktemp)
    # GST_PLUGIN_PATH is the whole reason this is regenerated rather than
    # left alone once present. A daemon inherits nothing from a login
    # shell, so without it the transcoder loads Homebrew's stock plugins
    # and every patch in patches/gstreamer is inert — installed, and
    # doing nothing. An earlier version returned early when the file
    # existed, which meant a satellite could never acquire a setting it
    # did not have on the day it was first provisioned.
    cat > "$tmpd" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$AGENT</string>
    <key>ProgramArguments</key>
    <array><string>$HOME/kahawai-src/target/release/kahawai-transcoder</string></array>
    <key>UserName</key><string>$(id -un)</string>
    <key>WorkingDirectory</key><string>$HOME</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$HOME/kahawai-transcoder.log</string>
    <key>StandardErrorPath</key><string>$HOME/kahawai-transcoder.log</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>GST_PLUGIN_PATH</key><string>$KAHAWAI_GST/plugins</string>
    </dict>
</dict>
</plist>
PLIST
    if [ -f "$plist" ] && diff -q "$tmpd" "$plist" >/dev/null 2>&1; then
        echo "daemon plist already current" >&2
        rm -f "$tmpd"
        return 0
    fi
    echo "installing the launchd daemon (sudo)" >&2
    sudo install -o root -g wheel -m 644 "$tmpd" "$plist"
    # bootout before bootstrap: bootstrap alone refuses a label already
    # loaded, and a plist edit does not reach a running job.
    sudo launchctl bootout "system/$AGENT" 2>/dev/null || true
    sudo launchctl bootstrap system "$plist"
    rm -f "$tmpd"
    echo "daemon installed and started" >&2
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
cargo build --release -p kahawai-transcoderd \
    --bin kahawai-transcoder 2>&1 | tail -1
BIN=target/release/kahawai-transcoder
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
# Daemon: kill and let KeepAlive respawn (kickstart on the system
# domain would need sudo). Agent fallback: kickstart as before.
if [ -f "/Library/LaunchDaemons/$AGENT.plist" ]; then
    pkill -f "kahawai-src/target/release/kahawai-transcoder" || true
    pkill -f "kahawai-src/target/release/kahawai transcoder" || true
else
    launchctl kickstart -k "gui/$(id -u)/$AGENT"
fi
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

# Everything a fresh satellite needs, in the order the parts depend on
# each other: GStreamer and the patched plugins first, because the plist
# points at them; then the daemon, which needs the binary that `deploy`
# builds. Idempotent — running it on a working satellite re-verifies and
# changes only what has drifted.
provision() {
    [ "$(uname)" = Darwin ] || { echo "run provision ON the mac" >&2; exit 2; }
    gst
    install_daemon
    echo >&2
    echo "provisioned. From the dev box: scripts/kahawai-mac.sh deploy" >&2
}

case "${1:-}" in
    setup) setup ;;
    gst) gst ;;
    provision) provision ;;
    deploy) shift; deploy "${1:-}" ;;
    *) echo "usage: $0 {setup|gst|provision|deploy [host]}" >&2
       echo "  setup      ON the mac, once: signing identity, then provision" >&2
       echo "  gst        ON the mac: GStreamer + the patches/ plugins" >&2
       echo "  provision  ON the mac: gst, then the launchd daemon (sudo)" >&2
       echo "  deploy     FROM the dev box: sync, build, sign, restart" >&2
       exit 2 ;;
esac
