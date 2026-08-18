#!/usr/bin/env bash
# The reference side of intro detection: intro-skipper's own analyzers, built
# from their sources and run over plain files. No Jellyfin server, no Jellyfin
# assemblies — see docs/intro-detection-plan.md.
#
#   scripts/kahawai-intro-ref.sh segments [--anime] FILE...
#   scripts/kahawai-intro-ref.sh fingerprint FILE START END
#   scripts/kahawai-intro-ref.sh compare LHS.txt RHS.txt
#   scripts/kahawai-intro-ref.sh silence FILE START END
#   scripts/kahawai-intro-ref.sh keyframes FILE START END
#   scripts/kahawai-intro-ref.sh --setup        # fetch and build, then stop
#
# Everything it needs lands under ~/.cache/kahawai-introref: a .NET SDK, the
# jellyfin-ffmpeg build that carries the chromaprint muxer their fingerprinting
# requires, and the clone. Nothing is installed system-wide and nothing GPL is
# copied into this repository.
set -euo pipefail

# The commit the port was read from and is measured against. Bump deliberately:
# a moving reference makes yesterday's numbers unreproducible.
PIN=577981ff7fe8b4745ab02040d525f315194732f8
REPO=https://github.com/intro-skipper/intro-skipper.git

CACHE=${KAHAWAI_INTROREF_CACHE:-$HOME/.cache/kahawai-introref}
HERE=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
CLONE=$CACHE/intro-skipper
BIN=$CACHE/bin/introref.dll
export DOTNET_ROOT=${DOTNET_ROOT:-$HOME/.dotnet}
export DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
export PATH=$DOTNET_ROOT:$PATH

mkdir -p "$CACHE"

if ! command -v dotnet >/dev/null; then
    echo "installing the .NET SDK into $DOTNET_ROOT" >&2
    curl -sSL https://dot.net/v1/dotnet-install.sh |
        bash -s -- --channel 9.0 --install-dir "$DOTNET_ROOT" >/dev/null
fi

# Their fingerprinting needs an ffmpeg built with chromaprint, which distro
# builds generally are not. Jellyfin ships one, which is also the build the
# plugin is tuned against.
if [[ ! -x $CACHE/ffmpeg ]]; then
    echo "fetching jellyfin-ffmpeg (chromaprint muxer)" >&2
    # Pinned: the measurements in docs/intro-detection-results.md were taken
    # against this build, and "latest" would silently move the ruler.
    gh release download v7.1.4-3 -R jellyfin/jellyfin-ffmpeg \
        -p '*portable_linux64-gpl.tar.xz' --dir "$CACHE" --clobber
    tar xf "$CACHE"/*portable_linux64-gpl.tar.xz -C "$CACHE"
fi
export INTROREF_FFMPEG=$CACHE/ffmpeg

if [[ ! -d $CLONE/.git ]]; then
    git clone --filter=blob:none "$REPO" "$CLONE" >&2
fi
if [[ $(git -C "$CLONE" rev-parse HEAD) != "$PIN" ]]; then
    git -C "$CLONE" fetch origin "$PIN" >&2 || git -C "$CLONE" fetch --all >&2
    git -C "$CLONE" checkout --detach "$PIN" >&2
fi

# Rebuild when either side moved: their sources, or our harness.
# The csproj too: a Compile/NoWarn edit changes what the dll is built
# from without touching any .cs, and a ruler built from a stale project
# definition is the one kind of drift this rig exists to rule out.
newest=$(find "$CLONE/IntroSkipper" "$HERE/introref" \( -name '*.cs' -o -name '*.csproj' \) -newer "$BIN" -print -quit 2>/dev/null || true)
if [[ ! -f $BIN || -n $newest ]]; then
    dotnet build "$HERE/introref/IntroRef.csproj" -c Release \
        -p:IntroSkipperDir="$CLONE" -o "$CACHE/bin" >&2
fi

[[ ${1:-} == --setup ]] && exit 0
exec dotnet "$BIN" "$@"
