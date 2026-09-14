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
# A reproducer may not run for ever. They finish in seconds; the ones that
# do not are wedged, and a wedged one used to hang the whole run with no
# output at all.
REPRO_TIMEOUT=120

# gst-libs libraries that must come from OUR build, and the ONLY ones.
#
# 0004 changes the size of a public struct in codecparsers, so anything
# holding one must be built against the same header: codecparsers itself,
# and codecs, whose decoder base classes embed them.
#
# Everything else resolves to the system's copy on purpose: a second
# copy of an UNPATCHED library buys nothing and invites two of it being
# loaded at once. macOS does not come through here at all — it patches
# its whole GStreamer instead (HomebrewFormula/kahawai-gstreamer.rb),
# because there a dylib is identified by path rather than soname, so a
# staged copy beside the system's is a crash rather than a warning.
OWNED_LIBS="codecparsers codecs"
# hlssink3 releases that ship WITHOUT the fixes in patches/gst-plugins-rs.
# Anything else installed is assumed to be a build that already carries
# them (0.16.0-alpha-… is what this box has), and is left alone.
HLSSINK3_STOCK="1.28.5 0.15.3"
# RS_TAG, RS_UNRELEASED and apply_rs_patches. The other two builds of the
# same sink — the Dockerfile and HomebrewFormula/kahawai-gstreamer.rb —
# classify the patches the same way, so every box runs the same hlssink3.
#
# $REPO, not $(dirname "$0"): the cd above already happened, so a relative
# $0 now resolves against the repo root and misses. It failed quietly —
# `set -u` then killed `build` on an unbound RS_UNRELEASED, but only after
# the staged directory had been wiped.
. "$REPO/scripts/kahawai-gst-rs.sh"

src=""
trap '[ -n "$src" ] && rm -rf "$src"' EXIT

die() { echo "error: $*" >&2; exit 1; }

# pkg-config, not gst-inspect: the .pc file answers the question with no
# runtime at all, and matched gst-inspect exactly where both were tried.
# gst-inspect has to load every plugin on the box to print a version, so
# it can be wedged by one of them; a text file cannot.
gst_version() {
    pkg-config --modversion gstreamer-1.0 2>/dev/null
}

# sort -V puts the older first; if the older of the pair is not MIN_GST,
# the system is behind it.
older_than() {
    [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ] && [ "$1" != "$2" ]
}

require_version() {
    local v="$1"
    [ -n "$v" ] || die "no gstreamer-1.0.pc — is pkg-config installed and GStreamer's development data present?"
    if older_than "$v" "$MIN_GST"; then
        die "system GStreamer is $v; these patches target $MIN_GST or newer.
       Building against older libraries would apply patches to sources
       they were not written for, and link them to a different ABI."
    fi
}

# ---------------------------------------------------------------- verify

verify() {
    local version live=0 missing=0 skipped=0 wedged=0
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
        local n name dir patch_failed=0 ran=0 repro out rc unrunnable=0
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
            #
            # Its OWN process group (set -m), so the kill below takes the
            # children with it. A reproducer that spawns gst-launch and is
            # killed on its own leaves that child wedged for ever — which
            # is how a 34-hour-old gst-inspect turned up on the mac.
            local repro_pid waited=0 timed_out=0
            set -m
            ( cd "$out" && python3 "$repro" ) >"$log" 2>&1 &
            repro_pid=$!
            set +m
            while kill -0 "$repro_pid" 2>/dev/null; do
                [ "$waited" -ge "$REPRO_TIMEOUT" ] && { timed_out=1; break; }
                sleep 1
                waited=$((waited + 1))
            done
            if [ "$timed_out" = 1 ]; then
                kill -9 -"$repro_pid" 2>/dev/null
                wait "$repro_pid" 2>/dev/null
                rc=124
            else
                wait "$repro_pid"
                rc=$?
            fi
            # A reproducer that dies on its own fixture also exits non-zero,
            # which would read as "patch missing". Keep that verdict distinct.
            local crashed=0
            grep -q 'Traceback (most recent call last)' "$log" && crashed=1
            if [ "$timed_out" = 1 ]; then
                # Says nothing about the patch: the reproducer never
                # reached a verdict. Counted apart from live and missing
                # so nobody reads silence as either.
                printf '  %s  %-43s %-8s TIMED OUT after %ss\n' \
                    "$n" "${name:0:43}" \
                    "$(basename "$repro" | sed -n 's/.*-repro-\([0-9]*\)\.py/repro-\1/p')" \
                    "$REPRO_TIMEOUT"
                unrunnable=$((unrunnable + 1))
                ran=$((ran - 1))
                rm -rf "$out"
                continue
            fi
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
        # ANY timeout voids the patch, not just an all-timeout patch. A
        # reproducer that never finished has said nothing about the patch,
        # and a passing sibling does not answer for it: two reproducers
        # exist because they test different things.
        if [ "$unrunnable" -gt 0 ]; then
            wedged=$((wedged + 1))
            continue
        fi
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
    if [ "$wedged" -gt 0 ]; then
        echo "live=$live missing=$missing no-reproducer=$skipped unchecked=$wedged"
        echo
        echo "$wedged patch(es) reached no verdict: a reproducer timed out after"
        echo "${REPRO_TIMEOUT}s. That is a wedged pipeline, not an answer about the"
        echo "patch — the run above says which."
    else
        echo "live=$live missing=$missing no-reproducer=$skipped"
    fi
    # `wedged` FAILS. The whole point of verify is that a green run means
    # the patches were measured; a run that measured nothing and exited 0
    # let the image ship unverified, which is the drift this exists to
    # catch. "I could not tell" is not "yes".
    if [ "$missing" -ne 0 ] || [ "$skipped" -ne 0 ] || [ "$wedged" -ne 0 ]; then
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

# gst-libs libraries, staged beside the plugins.
#
# Narrow on purpose: 0004 changes a public struct in codecparsers, so that
# library and every plugin holding one of its structs must come from the
# same build. Every OTHER gst-libs library is ABI-identical to the
# system's and staging it would shadow a perfectly good copy for no
# reason.
#
# Only real files. A looser glob also matches meson's `.symbols` text
# artifacts, and naming one as a link target replaced the real library
# with a pointer to a text file — every plugin that needed it then failed
# to load, which surfaced as reproducers "crashing" rather than as
# anything about libraries.
# Run a build step quietly, but print the tail of its output when it
# fails. Silence on success, evidence on failure: without this a failing
# meson or ninja said only "build failed", and the EXIT trap deleted the
# source tree before it could be rerun by hand.
run_step() {
    local what="$1"
    shift
    local log
    log="$(mktemp -t kahawai-gst-step)"
    if "$@" >"$log" 2>&1; then
        rm -f "$log"
        return 0
    fi
    # The error lines FIRST, then the tail. A plain tail is not enough:
    # applemedia emits pages of AVFoundation deprecation notes after the
    # failure, which pushed the one line that mattered out of view.
    echo "--- $what: error lines ---" >&2
    grep -E "FAILED:|fatal error|error:|ld: " "$log" | head -8 >&2
    echo "--- $what: last 10 lines ---" >&2
    tail -10 "$log" >&2
    rm -f "$log"
    die "$what failed"
}

stage_libraries() {
    local build_dir="$1" lib base
    while IFS= read -r lib; do
        install -m644 "$lib" "$KAHAWAI_GST/lib/"
        # libfoo-1.0.so.0.2805.0 -> libfoo-1.0.so.0 -> libfoo-1.0.so
        base="$(basename "$lib")"; base="${base%%.so.*}"
        ( cd "$KAHAWAI_GST/lib" \
          && ln -sf "$(basename "$lib")" "$base.so.0" \
          && ln -sf "$base.so.0" "$base.so" )
        echo "    installed $(basename "$lib")"
    done < <(for n in $OWNED_LIBS; do \
                 find "$build_dir/gst-libs" -type f -name "libgst$n-*.so.*" \
                     ! -name '*.symbols' ! -name '*.p'; \
             done | sort -u)
}

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
        run_step "gst-plugins-good: meson setup" \
            meson setup "$build_dir" "$src/subprojects/gst-plugins-good" "${args[@]}"
        run_step "gst-plugins-good: build" ninja -C "$build_dir"
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
        # NOT the complete set that links codecparsers, deliberately.
        # Measured 2026-09-14: twelve system plugins link it here;
        # these are the ones our pipelines load.
        # Unbuilt, and therefore still holding the system's struct:
        # closedcaption, codec2json, jpegformat, openjpeg,
        # smoothstreaming, vulkan. If one of them ever ends up in a
        # kahawai pipeline it has to move into this list — or the list
        # has to become "everything that links it", derived rather than
        # written down, which is the version that needs SDKs for vulkan
        # and nvcodec on every build box.
        local bad_plugins="nvcodec va v4l2codecs codectimestamper mpegtsdemux videoparsers"
        local args=(--buildtype=release -Dauto_features=disabled)
        for p in $bad_plugins; do args+=("-D$p=enabled"); done
        build_dir="$(mktemp -d)"
        run_step "gst-plugins-bad: meson setup" \
            meson setup "$build_dir" "$src/subprojects/gst-plugins-bad" "${args[@]}"
        run_step "gst-plugins-bad: build" ninja -C "$build_dir"
        stage_libraries "$build_dir"
        # Plugins only. gst-libs lives under gst-libs/gst/<name>/, so
        # the */gst/* filter matches it too; without the exclusion every
        # library landed in plugins/ as well and GStreamer would try to
        # load each one as a plugin.
        while IFS= read -r so; do
            install -m644 "$so" "$KAHAWAI_GST/plugins/"
            echo "    installed $(basename "$so")"
        done < <(find "$build_dir" \( -path '*/sys/*' -o -path '*/gst/*' \) \
                      ! -path '*/gst-libs/*' -type f -name 'libgst*.so' | sort)
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
    # NOT probed up front: hlssink3_system_version runs gst-inspect,
    # which is slow and needless on the path that builds regardless. Ask
    # only where the answer is used.
    if [ -n "$RS_UNRELEASED" ]; then
        # No release can carry these, so the system's version tells us
        # nothing: build regardless of what it has.
        echo "==> hlssink3: building ours — unreleased patches to apply:"
        for v in $RS_UNRELEASED; do echo "    $v"; done
    elif sys="$(hlssink3_system_version)"; [ -z "$sys" ]; then
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
    echo "    cloning $RS_TAG"
    git clone --depth 1 --branch "$RS_TAG" \
        https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git "$rs" \
        2>&1 | tail -1 || die "gst-plugins-rs clone failed — is $RS_TAG a tag?"
    apply_rs_patches "$rs" "$RS_PATCHES"
    run_step "hlssink3 build" sh -c "cd '$rs' && cargo build --release -p gst-plugin-hlssink3"
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
