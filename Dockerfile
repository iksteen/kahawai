# syntax=docker/dockerfile:1.7

# Build with:
#   docker build --build-arg KAHAWAI_BUILD="$(git describe --always --dirty)" -t kahawai .
#
# Intel Quick Sync / AMD VA-API (add the render-node group when running rootless):
#   docker run --rm --device=/dev/dri:/dev/dri kahawai doctor
#
# NVIDIA NVENC/NVDEC (requires NVIDIA Container Toolkit on the host):
#   docker run --rm --gpus all kahawai doctor

ARG RUST_VERSION=1.97.1
# gst-plugins-rs follows GStreamer's version numbers. 1.28.6 is the
# minimum: below it, hlssink3 aborts the process — not the session —
# on a fragment whose first buffer has no PTS, and on an unwrapped
# running time when a segment is added. Both panic inside an FFI
# callback, which cannot unwind.
ARG GST_PLUGINS_RS_VERSION=gstreamer-1.28.6
ARG GST_PLUGINS_RS_REV=75e46c3a1b868e9a08fd688d091476b76a498df1
# The whole GStreamer stack — core, base, good, bad, ugly, libav —
# comes from this tag, patched with patches/gstreamer. None of Ubuntu's
# gstreamer packages are installed: one tree means one version, one ABI,
# and the fixes in patches/ apply to everything that could load them.
ARG GSTREAMER_VERSION=1.28.6
ARG GSTREAMER_REV=2d3e05cbdad68e47d645f548899b432dc9fb4473

FROM rust:${RUST_VERSION}-bookworm AS rust-toolchain

# Build against the same userspace ABI as the runtime while retaining a pinned,
# current Rust toolchain. Rust's glibc binaries are forward-compatible here.
FROM ubuntu:26.04 AS builder

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH

# Dependencies come from Ubuntu's build-dep sets for the gstreamer
# source packages, not from a list maintained here. That set is what
# builds the plugins the distro ships, so the source build can reach
# every one of them — including plugins nothing in kahawai names, which
# decodebin can still autoplug and a library can still need.
#
# libgstreamer*-dev is deliberately absent: GStreamer is built below,
# and the distro's headers would be what everything later links
# against.
ARG DEBIAN_FRONTEND=noninteractive
# Ubuntu Resolute publishes libvpl-dev only for amd64:
# https://packages.ubuntu.com/libvpl-dev
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    intel_build_packages=""; \
    if [ "$arch" = amd64 ]; then \
        intel_build_packages="libvpl-dev"; \
    fi; \
    sed -i 's/^Types: deb$/Types: deb deb-src/' /etc/apt/sources.list.d/ubuntu.sources \
    && apt-get update \
    && apt-get build-dep -y --no-install-recommends \
        gstreamer1.0 gst-plugins-base1.0 gst-plugins-good1.0 \
        gst-plugins-bad1.0 gst-plugins-ugly1.0 gst-libav1.0 \
    && apt-get install -y --no-install-recommends \
        bison build-essential ca-certificates clang cmake ffmpeg flex git meson \
        nasm ninja-build pkg-config protobuf-compiler python3 python3-gi \
        libsvtav1enc-dev libaom-dev libdav1d-dev libfdk-aac-dev \
        libleptonica-dev libtesseract-dev tesseract-ocr-eng \
        $intel_build_packages \
    && rm -rf /var/lib/apt/lists/*

# The upstream fixes this image carries, with their reports and
# reproducers: patches/*/. Copied before the plugin builds so a change
# to a patch rebuilds only what depends on it.
COPY patches /usr/src/patches

ARG GSTREAMER_VERSION
ARG GSTREAMER_REV
# A patch that stops applying must FAIL the build. A for loop exits with
# the status of its last iteration, so without the explicit exit a patch
# that no longer applies after a version bump is skipped, the later ones
# succeed, and the image ships silently missing a fix — indistinguishable
# from a working one until someone plays the file it was written for.
RUN git clone --depth 1 --branch "$GSTREAMER_VERSION" \
        https://gitlab.freedesktop.org/gstreamer/gstreamer.git /tmp/gstreamer \
    && test "$(git -C /tmp/gstreamer rev-parse HEAD)" = "$GSTREAMER_REV" \
    && for p in /usr/src/patches/gstreamer/*.patch; do \
           echo "applying $(basename "$p")"; \
           git -C /tmp/gstreamer apply "$p" \
               || { echo "FAILED to apply $(basename "$p")" >&2; exit 1; }; \
       done

# auto_features stays at its default, so every plugin whose dependency
# is present is built — DVD, chiptunes, Bluetooth codecs and all. What
# this image will be asked to play is not knowable from here, and a
# plugin that costs seconds of compile is a cheaper bet than finding it
# missing against a real file.
#
# Off: Qt, GTK, devtools — hundreds of megabytes to draw pictures on a
# screen this container does not have.
#
# The plugins kahawai cannot work without are named explicitly, so a
# missing dependency fails this build instead of becoming a missing
# element at runtime.
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    # The qsv plugin embeds oneVPL, so it must follow the package selection above.
    qsv=disabled; \
    if [ "$arch" = amd64 ]; then qsv=enabled; fi; \
    meson setup /tmp/gst-build /tmp/gstreamer \
        --buildtype=release --prefix=/usr/local --libdir=lib \
        --wrap-mode=nodownload \
        -Dgpl=enabled \
        -Dexamples=disabled -Dtests=disabled -Ddoc=disabled -Dnls=disabled \
        -Dintrospection=disabled -Ddevtools=disabled -Dges=disabled \
        -Drtsp_server=disabled -Dsharp=disabled -Dpython=disabled \
        -Drs=disabled -Dgst-examples=disabled -Dqt5=disabled -Dqt6=disabled \
        -Dgst-plugins-good:avi=enabled -Dgst-plugins-good:matroska=enabled \
        -Dgst-plugins-good:isomp4=enabled -Dgst-plugins-good:flac=enabled \
        -Dgst-plugins-good:audioparsers=enabled \
        -Dgst-plugins-bad:hls=enabled \
        -Dgst-plugins-bad:videoparsers=enabled -Dgst-plugins-bad:codectimestamper=enabled \
        -Dgst-plugins-bad:mpegtsdemux=enabled -Dgst-plugins-bad:mpegtsmux=enabled \
        -Dgst-plugins-bad:assrender=enabled -Dgst-plugins-bad:nvcodec=enabled \
        -Dgst-plugins-bad:va=enabled -Dgst-plugins-bad:qsv="$qsv" \
        -Dgst-plugins-bad:v4l2codecs=enabled \
        -Dgst-plugins-ugly:x264=enabled -Dgst-plugins-ugly:a52dec=enabled \
        -Dgst-plugins-ugly:mpeg2dec=enabled -Dgst-plugins-ugly:dvdread=enabled \
        -Dlibav=enabled \
    && meson compile -C /tmp/gst-build \
    && meson install -C /tmp/gst-build \
    && meson install -C /tmp/gst-build --destdir /staging \
    && ldconfig

# What the runtime must install, derived from what was built: every
# shared object the plugins and libraries load, mapped back to its
# owning package. A plugin gained or lost upstream updates this by
# itself.
RUN mkdir -p /out \
    && ldd /usr/local/lib/gstreamer-1.0/*.so /usr/local/lib/libgst*.so.* 2>/dev/null \
        | awk '/=> \//{print $3}' | sort -u \
        | grep -v '^/usr/local/' \
        | xargs -r dpkg -S 2>/dev/null \
        | cut -d: -f1 | tr -d ' ' | sort -u > /out/runtime-packages.txt \
    && wc -l < /out/runtime-packages.txt

# Everything built after this — gst-plugins-rs, and kahawai itself —
# must link the GStreamer just installed, not a distro one.
ENV PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
    LD_LIBRARY_PATH=/usr/local/lib

# Three plugins out of the forty-odd in gst-plugins-rs, because this is
# the slowest step here and the rest — webrtc, ndi, spotify, gtk4,
# threadshare — brings its own dependency trees for elements nothing
# would reach.
#
#   isobmff    isofmp4mux, which kahawai muxes CMAF with. The plugin
#              was gst-plugin-fmp4 before 1.28; the element is unchanged.
#   hlssink3   the HLS sink kahawai prefers.
#   dav1d      AV1 decoding. Nothing names it, and it is still needed:
#              decodebin picks decoders out of the registry, so a codec
#              with no decoder is a file that will not play. libaom
#              gives av1dec as well; dav1ddec is first in kahawai's
#              ladder and several times faster.
#
# rav1enc also lives here and is not built: it sits below svtav1enc,
# which gst-plugins-bad provides. Add -p gst-plugin-rav1e for a second
# software AV1 encoder.
#
# patches/gst-plugins-rs is NOT applied: 1.28.6 carries both fixes, and
# git apply refuses a patch already in the tree. The files stay as the
# record; each says where it landed.
ARG GST_PLUGINS_RS_VERSION
ARG GST_PLUGINS_RS_REV
RUN git clone --depth 1 --branch "$GST_PLUGINS_RS_VERSION" \
        https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git \
        /tmp/gst-plugins-rs \
    && test "$(git -C /tmp/gst-plugins-rs rev-parse HEAD)" = "$GST_PLUGINS_RS_REV"

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=gst-plugins-rs-target-ubuntu2604,target=/tmp/gst-plugins-rs/target \
    cargo build --locked --release \
        --manifest-path /tmp/gst-plugins-rs/Cargo.toml \
        -p gst-plugin-isobmff \
        -p gst-plugin-hlssink3 \
        -p gst-plugin-dav1d \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgstisobmff.so \
        /staging/usr/local/lib/gstreamer-1.0/libgstisobmff.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgsthlssink3.so \
        /staging/usr/local/lib/gstreamer-1.0/libgsthlssink3.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgstdav1d.so \
        /staging/usr/local/lib/gstreamer-1.0/libgstdav1d.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgstdav1d.so \
        /usr/local/lib/gstreamer-1.0/libgstdav1d.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgstisobmff.so \
        /usr/local/lib/gstreamer-1.0/libgstisobmff.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgsthlssink3.so \
        /usr/local/lib/gstreamer-1.0/libgsthlssink3.so

WORKDIR /usr/src/kahawai
COPY . .

ARG KAHAWAI_BUILD=container
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kahawai-target-ubuntu2604,target=/usr/src/kahawai/target \
    KAHAWAI_BUILD="${KAHAWAI_BUILD}" cargo build --locked --release --bin kahawai \
    && install -D -m 0755 -s target/release/kahawai /out/kahawai

# This is the release boundary, not an optional sibling stage. Both outputs
# below copy from it, so BuildKit cannot produce an image or binary artifact
# without running the patch reproducers and the non-skippable media suite.
FROM builder AS media-tested

# GL tone-map tests need the same headless window selection as the runtime and
# a real, private runtime directory. Without these, the element factories are
# present but their dry-run fails before negotiation, which is not evidence that
# the shipped path works.
ENV GST_GL_WINDOW=surfaceless \
    XDG_RUNTIME_DIR=/tmp/kahawai-runtime
RUN mkdir -p "$XDG_RUNTIME_DIR" && chmod 0700 "$XDG_RUNTIME_DIR"

RUN scripts/kahawai-gst-plugins.sh verify \
        --library-dir /usr/local/lib \
        --plugin-dir /usr/local/lib/gstreamer-1.0 \
        --exclusive

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kahawai-target-ubuntu2604,target=/usr/src/kahawai/target \
    KAHAWAI_MEDIA_TEST_STRICT=1 cargo test --locked --release --workspace

FROM scratch AS binary-artifact
COPY --from=media-tested /out/kahawai /kahawai

FROM ubuntu:26.04 AS runtime

# The runtime libraries are the list the builder derived, plus the
# drivers and data files nothing links against but everything needs. No
# gstreamer1.0-* packages: the stack comes from the builder.
ARG DEBIAN_FRONTEND=noninteractive
COPY --from=media-tested /out/runtime-packages.txt /tmp/runtime-packages.txt
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    intel_packages=""; \
    if [ "$arch" = amd64 ]; then \
        # iHD covers Gen8+; i965-shaders keeps older Quick Sync machines useful.
        intel_packages="intel-media-va-driver-non-free i965-va-driver-shaders libvpl2 onevpl-tools"; \
    fi; \
    apt-get update; \
    # Every Tesseract LANGUAGE, and no Tesseract SCRIPT. The hub maps a
    # track's language tag to a model name and, for any tag it does not
    # map, passes the three-letter tag through as the model name
    # (hub/ocr.rs) — so the set it can ask for is exactly the languages,
    # and shipping only English silently drops the OCR tier for every
    # other one. Derived rather than listed: a hand-list is a second
    # place to forget a language. 124 packages, ~338 MiB.
    #
    # The script packs are the other ~329 MiB and are unreachable here:
    # they hold Latin.traineddata, HanS.traineddata and friends, and no
    # language tag turns into those names.
    tesseract_langs="$(apt-cache pkgnames tesseract-ocr- \
        | grep -vE '^tesseract-ocr-(all|script-)' | sort | tr '\n' ' ')"; \
    apt-get install -y --no-install-recommends \
        $(tr '\n' ' ' < /tmp/runtime-packages.txt) \
        ca-certificates fontconfig \
        mesa-va-drivers vainfo \
        tesseract-ocr $tesseract_langs \
        $intel_packages; \
    rm -rf /var/lib/apt/lists/* /tmp/runtime-packages.txt

COPY --from=media-tested /out/kahawai /usr/local/bin/kahawai
COPY --from=media-tested /staging/usr/local/ /usr/local/

RUN ldconfig

# The NVIDIA runtime injects its driver libraries. "video" is NVENC/NVDEC;
# "graphics" also permits the headless OpenGL tone-mapping path.
ENV NVIDIA_VISIBLE_DEVICES=all \
    NVIDIA_DRIVER_CAPABILITIES=compute,video,utility,graphics \
    GST_GL_WINDOW=surfaceless \
    GST_PLUGIN_PATH=/usr/local/lib/gstreamer-1.0 \
    XDG_CONFIG_HOME=/config \
    XDG_DATA_HOME=/data \
    XDG_CACHE_HOME=/cache \
    XDG_RUNTIME_DIR=/tmp/kahawai-runtime \
    KAHAWAI_HUB__DATA_DIR=/data/kahawai \
    KAHAWAI_MEDIAHOST__STATE_DIR=/data/kahawai-mediahost \
    KAHAWAI_TRANSCODER__STATE_DIR=/data/kahawai-transcoder

RUN mkdir -p /config /data /cache "$XDG_RUNTIME_DIR" \
    && chmod 0700 "$XDG_RUNTIME_DIR"

VOLUME ["/config", "/data", "/cache"]
EXPOSE 8420 8421

WORKDIR /config
ENTRYPOINT ["/usr/local/bin/kahawai"]
CMD ["all-in-one"]
