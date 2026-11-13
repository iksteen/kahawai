#!/usr/bin/env bash
# Prove the patched GStreamer under test carries every patch in patches/.
#
#   kahawai-gst-verify.sh                     # the installed kahawai-gstreamer package
#   kahawai-gst-verify.sh --library-dir DIR --plugin-dir DIR [--exclusive]
#
# With no directories it verifies what this box's kahawai processes load:
# the kahawai-gstreamer package in /opt (see kahawai-gst-env.sh). The
# Dockerfile and the package's PKGBUILD pass their staged trees explicitly,
# with --exclusive so nothing outside that tree can answer for a plugin.
#
# Every patch ships a reproducer that exits 0 when the plugin is fixed and
# non-zero when the bug is still there, so running them against the plugins
# under test says which patches are actually live — a stronger claim than
# "the build ran". A patch with no reproducer, or one whose reproducer never
# reached a verdict, fails the run: a green run means every patch was
# measured.
set -uo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"
PATCHES="$REPO/patches/gstreamer"
RS_PATCHES="$REPO/patches/gst-plugins-rs"

VERIFY_LIBRARY_DIR=""
VERIFY_PLUGIN_DIR=""
VERIFY_EXCLUSIVE=""
# GStreamer's own per-user directory. READ ONLY, and only to warn: it is
# the user's, nothing here writes to it or removes from it. It is worth
# looking at because it takes PRECEDENCE over GST_PLUGIN_PATH, so a copy
# left there wins over the tree under test, and verify would then be
# reporting on a plugin that tree does not hold.
USER_PLUGINS="$HOME/.local/share/gstreamer-1.0/plugins"

# Below this the patches are not known to apply and the ABI is not the
# one they were written against.
MIN_GST=1.28.5
# A reproducer may not run for ever. They finish in seconds; the ones that
# do not are wedged, and a wedged one used to hang the whole run with no
# output at all.
REPRO_TIMEOUT=120

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
    # verdicts below would describe it rather than the tree under test. Said, not
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
    [ "$clash" = 1 ] && echo "         the verdicts below may describe those, not the tree under test"
    echo

    local patch
    for patch in "$PATCHES"/*.patch "$RS_PATCHES"/*.patch; do
        [ -e "$patch" ] || continue
        local n name dir patch_failed=0 ran=0 repro out rc unrunnable=0
        dir="$(dirname "$patch")"
        n="$(basename "$patch" | cut -c1-4)"
        name="$(basename "$patch" .patch | cut -c6-)"
        # applemedia is not built on Linux; a VideoToolbox encode there
        # cannot verify this patch. Keep this distinct from a LIVE result.
        if [[ "$name" = vtenc-* ]] && [ "$(uname -s)" != Darwin ]; then
            printf '  %s  %-52s N/A (Apple VideoToolbox)\n' "$n" "$name"
            continue
        fi
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
            #
            # Not on the terminal, either: that group is a background one,
            # and ffmpeg (0004's reference decoder) puts a terminal on its
            # stdin into raw mode for its keys. The kernel stops the whole
            # group with SIGTTOU for that, so under `makepkg` in a terminal
            # the reproducer sat stopped until the timeout voided the patch.
            local repro_pid waited=0 timed_out=0
            set -m
            ( cd "$out" && python3 "$repro" ) </dev/null >"$log" 2>&1 &
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
        echo "the plugins under test do not carry every patch in patches/ — rebuild the" >&2
        echo "kahawai-gstreamer package (or the image) that produced them" >&2
        return 1
    fi
    if [ -n "${registry_dir:-}" ]; then
        rm -rf "$registry_dir"
    fi
}

explicit_dirs=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --library-dir) [ "$#" -ge 2 ] || die "--library-dir needs a path"; VERIFY_LIBRARY_DIR="$2"; explicit_dirs=1; shift 2 ;;
        --plugin-dir) [ "$#" -ge 2 ] || die "--plugin-dir needs a path"; VERIFY_PLUGIN_DIR="$2"; explicit_dirs=1; shift 2 ;;
        --exclusive) VERIFY_EXCLUSIVE=1; shift ;;
        -h|--help) sed -n '2,/^set /{ /^#/s/^# \{0,1\}//p; }' "$0"; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done

# No directories given: the kahawai-gstreamer package. Only then — the
# PKGBUILD's check() passes its staged tree explicitly, possibly on a box
# that already has /opt, and must not be answered by the installed copy.
if [ -z "$explicit_dirs" ]; then
    [ -d /opt/kahawai-gstreamer/lib/gstreamer-1.0 ] ||
        die "no kahawai-gstreamer in /opt; pass --library-dir and --plugin-dir"
    VERIFY_LIBRARY_DIR=/opt/kahawai-gstreamer/lib
    VERIFY_PLUGIN_DIR=/opt/kahawai-gstreamer/lib/gstreamer-1.0
    VERIFY_EXCLUSIVE=1
    export PATH="/opt/kahawai-gstreamer/bin:$PATH"
    export PKG_CONFIG_PATH="/opt/kahawai-gstreamer/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export GI_TYPELIB_PATH="/opt/kahawai-gstreamer/lib/girepository-1.0${GI_TYPELIB_PATH:+:$GI_TYPELIB_PATH}"
fi
[ -n "$VERIFY_LIBRARY_DIR" ] && [ -n "$VERIFY_PLUGIN_DIR" ] ||
    die "--library-dir and --plugin-dir go together"

verify
