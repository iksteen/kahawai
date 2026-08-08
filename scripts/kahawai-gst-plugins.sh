#!/usr/bin/env bash
# Build and install the GStreamer plugins carrying patches/, and prove
# they are the ones loading.
#
#   kahawai-gst-plugins.sh verify    # are the patches live? (default)
#   kahawai-gst-plugins.sh verify --library-dir /usr/local/lib \
#       --plugin-dir /usr/local/lib/gstreamer-1.0 --exclusive
#   kahawai-gst-plugins.sh build     # rebuild everything and reinstall
#
# Why this exists: the plugins on a dev box drift. They were last built
# by hand from trees in /tmp that no longer exist, so which patches were
# actually in them stopped being knowable — indistinguishable from a
# correct set until a file that needed a missing fix failed. The
# container cannot drift (its Dockerfile applies every patch and fails if
# one will not apply); this is the same guarantee for the machine you
# work on.
#
# Everything goes in ONE directory that this script owns and wipes. It
# WRITES nowhere else — not to GStreamer's per-user directory, not to the
# system's — so a build here cannot change what any other program on the
# box loads. That isolation is required rather than tidy: patch 0004
# changes the size of a public H.264 struct, so the plugins holding one
# and the library they hold it from must come from the same build and
# must not be reachable by anything built against the other ABI.
#
# VERIFY is the useful half. Every patch ships a reproducer that exits 0
# when the plugin is fixed and non-zero when the bug is still there, so
# running them against the installed plugins says which patches are
# actually live — a stronger claim than "the build ran", and the one that
# was missing when the plugins drifted.
set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"
PATCHES="$REPO/patches/gstreamer"
RS_PATCHES="$REPO/patches/gst-plugins-rs"

# The one directory this script owns. Kahawai-only on purpose: invisible
# to every other GStreamer program on the box.
KAHAWAI_GST="$HOME/.local/lib/kahawai-gst"
VERIFY_LIBRARY_DIR="$KAHAWAI_GST/lib"
VERIFY_PLUGIN_DIR="$KAHAWAI_GST/plugins"
VERIFY_EXCLUSIVE=""
# GStreamer's own per-user directory. READ ONLY, and only to warn: it is
# the user's, nothing here writes to it or removes from it. It is worth
# looking at because it takes PRECEDENCE over GST_PLUGIN_PATH, so a copy
# left there wins over ours and verify would then be reporting on a
# plugin this script did not build.
USER_PLUGINS="$HOME/.local/share/gstreamer-1.0/plugins"

# Below this the patches are not known to apply and the ABI is not the
# one they were written against.
MIN_GST=1.28.5
# hlssink3 releases that ship WITHOUT the fixes in patches/gst-plugins-rs.
# Anything else installed is assumed to be a build that already carries
# them (0.16.0-alpha-… is what this box has), and is left alone.
HLSSINK3_STOCK="1.28.5 0.15.3"
# The gst-plugins-rs release to build hlssink3 from when the system's is
# too old. Both fixes in patches/gst-plugins-rs landed by here, which is
# also what the image pins.
RS_TAG=gstreamer-1.28.6

src=""
trap '[ -n "$src" ] && rm -rf "$src"' EXIT

die() { echo "error: $*" >&2; exit 1; }

gst_version() {
    gst-inspect-1.0 --version 2>/dev/null | awk '/^gst-inspect-1.0 version/ {print $3}'
}

# sort -V puts the older first; if the older of the pair is not MIN_GST,
# the system is behind it.
older_than() {
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ] && [ "$1" != "$2" ]
}

require_version() {
    local v="$1"
    [ -n "$v" ] || die "no gst-inspect-1.0 on PATH"
    if older_than "$v" "$MIN_GST"; then
        die "system GStreamer is $v; these patches target $MIN_GST or newer.
       Building against older libraries would apply patches to sources
       they were not written for, and link them to a different ABI."
    fi
}

# ---------------------------------------------------------------- verify

verify() {
    local version live=0 missing=0 skipped=0
    export GST_PLUGIN_PATH="$VERIFY_PLUGIN_DIR${GST_PLUGIN_PATH:+:$GST_PLUGIN_PATH}"
    export LD_LIBRARY_PATH="$VERIFY_LIBRARY_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    if [ -n "$VERIFY_EXCLUSIVE" ]; then
        export GST_PLUGIN_SYSTEM_PATH_1_0="$VERIFY_PLUGIN_DIR"
        local registry_dir
        registry_dir="$(mktemp -d -t kahawai-gst-registry-XXXXXX)"
        export GST_REGISTRY="$registry_dir/registry.bin"
    fi
    version="$(gst_version)"; require_version "$version"
    echo "system GStreamer: $version"

    # A same-named plugin in GStreamer's per-user directory wins over
    # GST_PLUGIN_PATH, so one left there is what actually loads and the
    # verdicts below would describe it rather than our build. Said, not
    # touched: removing it is the owner's call.
    local clash=0 so
    for so in "$VERIFY_PLUGIN_DIR"/*.so; do
        [ -e "$so" ] || continue
        if [ -e "$USER_PLUGINS/$(basename "$so")" ]; then
            [ "$clash" = 0 ] && echo && echo "WARNING: $USER_PLUGINS also has, and it wins —"
            echo "         $(basename "$so")"
            clash=1
        fi
    done
    [ "$clash" = 1 ] && echo "         the verdicts below may describe those, not this build"
    echo

    local patch
    for patch in "$PATCHES"/*.patch "$RS_PATCHES"/*.patch; do
        [ -e "$patch" ] || continue
        local n name dir patch_failed=0 ran=0 repro out rc
        dir="$(dirname "$patch")"
        n="$(basename "$patch" | cut -c1-4)"
        name="$(basename "$patch" .patch | cut -c6-)"
        for repro in "$dir/$n"-*-repro-*.py; do
            [ -e "$repro" ] || continue
            ran=$((ran + 1))
            out="$(mktemp -d)"
            local log="$out/run.log"
            # No arguments: the reproducers do not share one. Most take an
            # output directory, 0003 takes a size in MiB, and handing a path
            # to that one crashes it — which then reads as a missing patch.
            ( cd "$out" && python3 "$repro" ) >"$log" 2>&1
            rc=$?
            # A reproducer that dies on its own fixture also exits non-zero,
            # which would read as "patch missing". Keep that verdict distinct.
            local crashed=0
            grep -q 'Traceback (most recent call last)' "$log" && crashed=1
            if [ "$crashed" = 1 ]; then
                printf '  %s  %-43s %-8s INCONCLUSIVE (reproducer crashed)\n' \
                    "$n" "${name:0:43}" "$(basename "$repro" | sed -n 's/.*-repro-\([0-9]*\)\.py/repro-\1/p')"
                sed 's/^/      /' "$log" >&2
                patch_failed=1
            elif [ "$rc" = 0 ]; then
                printf '  %s  %-43s %-8s LIVE\n' \
                    "$n" "${name:0:43}" "$(basename "$repro" | sed -n 's/.*-repro-\([0-9]*\)\.py/repro-\1/p')"
            else
                printf '  %s  %-43s %-8s MISSING\n' \
                    "$n" "${name:0:43}" "$(basename "$repro" | sed -n 's/.*-repro-\([0-9]*\)\.py/repro-\1/p')"
                sed 's/^/      /' "$log" >&2
                patch_failed=1
            fi
            rm -rf "$out"
        done
        if [ "$ran" -eq 0 ]; then
            printf '  %s  %-52s no reproducer\n' "$n" "${name:0:52}"
            skipped=$((skipped + 1))
            continue
        fi
        if [ "$patch_failed" = 1 ]; then
            missing=$((missing + 1))
        else
            live=$((live + 1))
        fi
    done

    echo
    echo "live=$live missing=$missing no-reproducer=$skipped"
    if [ "$missing" -ne 0 ] || [ "$skipped" -ne 0 ]; then
        echo "run '$(basename "$0") build' to rebuild the plugins from patches/" >&2
        return 1
    fi
    if [ -n "${registry_dir:-}" ]; then
        rm -rf "$registry_dir"
    fi
}

# ----------------------------------------------------------------- build

# Which plugins a patch set touches, read from the patches themselves: a
# tenth patch touching a new plugin must not be silently left out of the
# build the way it was left out of the install.
plugins_in() {   # $1 = gst-plugins-good | gst-plugins-bad
    grep -hoE "subprojects/$1/(gst|sys)/[a-z0-9]+/" "$PATCHES"/*.patch 2>/dev/null \
        | awk -F/ '{print $4}' | sort -u
}

# Does any patch touch this plugin set at all? Asked separately because a
# patch can land in gst-libs rather than in a plugin directory — 0004
# edits gst-plugins-bad/gst-libs/gst/codecparsers, which plugins_in()
# cannot see. Deciding the whole gst-plugins-bad build from that regex
# skipped it silently, after the wipe had already removed what it should
# have replaced.
touches() { grep -lq "subprojects/$1/" "$PATCHES"/*.patch 2>/dev/null; }

build() {
    local version
    version="$(gst_version)"; require_version "$version"
    echo "building for system GStreamer $version"
    command -v meson >/dev/null || die "meson not installed"
    command -v ninja >/dev/null || die "ninja not installed"

    src="$(mktemp -d -t kahawai-gst-src-XXXXXX)"
    echo "==> source: $src"
    # The plugins link the system's GStreamer libraries, so they are built
    # from the version those libraries came from.
    git clone --depth 1 --branch "$version" \
        https://gitlab.freedesktop.org/gstreamer/gstreamer.git "$src" 2>&1 | tail -1 \
        || die "clone failed — is $version a released tag?"

    # Every patch, or none. A skipped patch is exactly the drift this
    # script exists to end, so it stops the run rather than warning.
    local p
    for p in "$PATCHES"/*.patch; do
        echo "    applying $(basename "$p")"
        git -C "$src" apply "$p" || die "FAILED to apply $(basename "$p") to $version"
    done

    # Wipe. This directory exists to be rebuilt after the system moves: a
    # distro upgrade changes the libraries these plugins link against,
    # and a survivor from before it is worse than nothing — it loads, it
    # looks right, and it is built against an ABI that is gone. So
    # NOTHING is carried across. Everything below is rebuilt from source.
    echo "==> wiping $KAHAWAI_GST"
    rm -rf "$KAHAWAI_GST"
    mkdir -p "$KAHAWAI_GST/plugins" "$KAHAWAI_GST/lib"

    local good build_dir so
    good="$(plugins_in gst-plugins-good)"

    # -- gst-plugins-good ------------------------------------------------
    if [ -n "$good" ]; then
        echo "==> gst-plugins-good: $(echo "$good" | tr '\n' ' ')"
        local args=(--buildtype=release -Dauto_features=disabled)
        for p in $good; do args+=("-D$p=enabled"); done
        build_dir="$(mktemp -d)"
        meson setup "$build_dir" "$src/subprojects/gst-plugins-good" "${args[@]}" >/dev/null \
            || die "gst-plugins-good: meson setup failed"
        ninja -C "$build_dir" >/dev/null || die "gst-plugins-good: build failed"
        for p in $good; do
            so="$build_dir/gst/$p/libgst$p.so"
            [ -f "$so" ] || die "expected $so, not built"
            install -m644 "$so" "$KAHAWAI_GST/plugins/"
            echo "    installed libgst$p.so"
        done
        rm -rf "$build_dir"
    fi

    # -- gst-plugins-bad -------------------------------------------------
    # 0004 changes a public struct in gst-libs/codecparsers, so the
    # library AND every plugin that holds one of its structs must come
    # from this build. Those plugins are not derivable from the patch
    # (nothing edits them); they are the ones that link codecparsers.
    if touches gst-plugins-bad; then
        echo "==> gst-plugins-bad: codecparsers + the plugins that link it"
        local bad_plugins="nvcodec va v4l2codecs codectimestamper mpegtsdemux videoparsers"
        local args=(--buildtype=release -Dauto_features=disabled)
        for p in $bad_plugins; do args+=("-D$p=enabled"); done
        build_dir="$(mktemp -d)"
        meson setup "$build_dir" "$src/subprojects/gst-plugins-bad" "${args[@]}" >/dev/null \
            || die "gst-plugins-bad: meson setup failed"
        ninja -C "$build_dir" >/dev/null || die "gst-plugins-bad: build failed"
        # The libraries first: the plugins carry an RPATH to them.
        #
        # Only the versioned shared objects. A looser glob also matches
        # meson's `.symbols` text artifacts, and naming one as a link
        # target replaced the real library with a pointer to a text file
        # — every plugin that needed it then failed to load, which
        # surfaced as reproducers "crashing" rather than as anything
        # about libraries.
        local lib
        while IFS= read -r lib; do
            install -m644 "$lib" "$KAHAWAI_GST/lib/"
            # libfoo-1.0.so.0.2805.0 -> libfoo-1.0.so.0 -> libfoo-1.0.so
            local base
            base="$(basename "$lib")"; base="${base%%.so.*}"
            ( cd "$KAHAWAI_GST/lib" \
              && ln -sf "$(basename "$lib")" "$base.so.0" \
              && ln -sf "$base.so.0" "$base.so" )
            echo "    installed $(basename "$lib")"
        done < <(find "$build_dir/gst-libs" -type f -name 'libgstcodec*.so.*' \
                     ! -name '*.symbols' ! -name '*.p' | sort)
        while IFS= read -r so; do
            install -m644 "$so" "$KAHAWAI_GST/plugins/"
            echo "    installed $(basename "$so")"
        done < <(find "$build_dir" -name 'libgst*.so' -type f -path '*/sys/*' -o \
                      -name 'libgst*.so' -type f -path '*/gst/*' | sort)
        rm -rf "$build_dir"
    fi

    # -- hlssink3 (gst-plugins-rs) ---------------------------------------
    build_hlssink3

    echo
    verify
}

# The SYSTEM's hlssink3 decides this, read with our directory out of the
# way. "Does this box need us to supply a patched hlssink3?" is a
# question about what the distro ships, not about what we staged last
# time — asking with KAHAWAI_GST on the path answers with our own build
# and can only ever say "no need", whatever the system holds.
hlssink3_system_version() {
    env -u GST_PLUGIN_PATH -u LD_LIBRARY_PATH \
        gst-inspect-1.0 hlssink3 2>/dev/null | awk '/^  Version/ {print $2}'
}

build_hlssink3() {
    local sys stock=0 v
    sys="$(hlssink3_system_version)"
    if [ -z "$sys" ]; then
        echo "==> hlssink3: none on the system — building ours from patches/"
    else
        # Prefix match: the distro calls its build 0.15.3-6302bea23, and
        # that IS the stock 0.15.3 that lacks these fixes. Comparing for
        # equality lets the git suffix hide it.
        for v in $HLSSINK3_STOCK; do
            case "$sys" in "$v"|"$v"-*|"$v".*) stock=1 ;; esac
        done
        if [ "$stock" = 0 ]; then
            echo "==> hlssink3: system has $sys, past the releases that need patching"
            return 0
        fi
        echo "==> hlssink3: system has $sys, a stock release without patches/gst-plugins-rs"
    fi

    command -v cargo >/dev/null || die "hlssink3 needs building but cargo is not installed"
    local rs
    rs="$(mktemp -d -t kahawai-gst-rs-XXXXXX)"
    # A release tag that already carries both fixes, not main and not a
    # patched older tag. patches/gst-plugins-rs is deliberately NOT
    # applied here, for the reason the Dockerfile gives: $RS_TAG has them
    # and git apply refuses a patch already in the tree. Those files are
    # the record of why 0001 needed 0000 first, not something to replay.
    echo "    cloning $RS_TAG (carries both fixes; patches/gst-plugins-rs not applied)"
    git clone --depth 1 --branch "$RS_TAG" \
        https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git "$rs" \
        2>&1 | tail -1 || die "gst-plugins-rs clone failed — is $RS_TAG a tag?"
    ( cd "$rs" && cargo build --release -p gst-plugin-hlssink3 ) >/dev/null 2>&1 \
        || die "hlssink3 build failed"
    install -m644 "$rs/target/release/libgsthlssink3.so" "$KAHAWAI_GST/plugins/" \
        || die "hlssink3: built but not found"
    echo "    installed libgsthlssink3.so"
    rm -rf "$rs"
}

action="${1:-verify}"
[ "$#" -gt 0 ] && shift
while [ "$#" -gt 0 ]; do
    case "$1" in
        --library-dir) [ "$#" -ge 2 ] || die "--library-dir needs a path"; VERIFY_LIBRARY_DIR="$2"; shift 2 ;;
        --plugin-dir) [ "$#" -ge 2 ] || die "--plugin-dir needs a path"; VERIFY_PLUGIN_DIR="$2"; shift 2 ;;
        --exclusive) VERIFY_EXCLUSIVE=1; shift ;;
        *) die "unknown argument: $1" ;;
    esac
done

case "$action" in
    verify) verify ;;
    build)  [ -z "$VERIFY_EXCLUSIVE" ] || die "--exclusive is verify-only"; build ;;
    *) echo "usage: $(basename "$0") [verify|build] [--library-dir DIR --plugin-dir DIR --exclusive]" >&2; exit 2 ;;
esac
