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
# gst-plugins-rs now follows GStreamer's own version numbers.
ARG GST_PLUGINS_RS_VERSION=gstreamer-1.28.5
ARG GST_PLUGINS_RS_REV=cee45224cb2d10e1523af4f256d0dd64c8b29491
# Ubuntu 26.04 ships GStreamer 1.28.2; the patched demuxers are built
# from the current stable release instead. gst-plugins-good declares
# ">= major.minor.0" for a stable series, so 1.28.5 plugins against the
# distro's 1.28.2 core and base is a supported combination.
ARG GSTREAMER_VERSION=1.28.5
ARG GSTREAMER_REV=727ceb91886862d200f423baf36cde2bb7ce5b4d

FROM rust:${RUST_VERSION}-bookworm AS rust-toolchain

# Build against the same userspace ABI as the runtime while retaining a pinned,
# current Rust toolchain. Rust's glibc binaries are forward-compatible here.
FROM ubuntu:26.04 AS builder

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        git \
        libgstreamer-plugins-base1.0-dev \
        libgstreamer1.0-dev \
        libass-dev \
        libleptonica-dev \
        libtesseract-dev \
        meson \
        ninja-build \
        pkg-config \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# The upstream fixes this image carries, with their reports and
# reproducers: patches/*/. Copied before the plugin builds so a change
# to a patch rebuilds only what depends on it.
COPY patches /usr/src/patches

# gst-plugins-good: avi and matroska only. Everything else stays on the
# distro's 1.28.2 — these are the two the patches touch, and the two
# whose failures are fatal rather than cosmetic. They land in
# /usr/local, which GST_PLUGIN_PATH searches ahead of the system path,
# so they shadow the packaged copies.
ARG GSTREAMER_VERSION
ARG GSTREAMER_REV
RUN git clone --depth 1 --branch "$GSTREAMER_VERSION" \
        https://gitlab.freedesktop.org/gstreamer/gstreamer.git /tmp/gstreamer \
    && test "$(git -C /tmp/gstreamer rev-parse HEAD)" = "$GSTREAMER_REV" \
    && for p in /usr/src/patches/gstreamer/*.patch; do \
           echo "applying $(basename "$p")"; \
           git -C /tmp/gstreamer apply "$p"; \
       done \
    && meson setup /tmp/gst-good-build /tmp/gstreamer/subprojects/gst-plugins-good \
        --buildtype=release \
        -Dauto_features=disabled -Davi=enabled -Dmatroska=enabled \
        -Dexamples=disabled -Dtests=disabled -Dnls=disabled -Ddoc=disabled \
    && meson compile -C /tmp/gst-good-build \
    && install -D -m 0755 -s /tmp/gst-good-build/gst/avi/libgstavi.so \
        /out/gstreamer-1.0/libgstavi.so \
    && install -D -m 0755 -s /tmp/gst-good-build/gst/matroska/libgstmatroska.so \
        /out/gstreamer-1.0/libgstmatroska.so

# gst-plugin-fmp4 became gst-plugin-isobmff (libgstfmp4.so ->
# libgstisobmff.so) between 0.14 and 1.28. The element kahawai asks for,
# isofmp4mux, is unchanged and still lives in that plugin.
# 0000 is an upstream commit, not ours — the leading zero says so, and
# says it goes first: 0001 fixes its abort by storing no running time,
# which is only safe once 0000 stops the emission unwrapping it. Both
# landed after the 1.28.5 tag, so a release build has to carry them.
# Applied in filename order, which is why the numbering carries it.
ARG GST_PLUGINS_RS_VERSION
ARG GST_PLUGINS_RS_REV
RUN git clone --depth 1 --branch "$GST_PLUGINS_RS_VERSION" \
        https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git \
        /tmp/gst-plugins-rs \
    && test "$(git -C /tmp/gst-plugins-rs rev-parse HEAD)" = "$GST_PLUGINS_RS_REV" \
    && for p in /usr/src/patches/gst-plugins-rs/*.patch; do \
           echo "applying $(basename "$p")"; \
           git -C /tmp/gst-plugins-rs apply "$p"; \
       done

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=gst-plugins-rs-target-ubuntu2604,target=/tmp/gst-plugins-rs/target \
    cargo build --locked --release \
        --manifest-path /tmp/gst-plugins-rs/Cargo.toml \
        -p gst-plugin-isobmff \
        -p gst-plugin-hlssink3 \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgstisobmff.so \
        /out/gstreamer-1.0/libgstisobmff.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgsthlssink3.so \
        /out/gstreamer-1.0/libgsthlssink3.so

WORKDIR /usr/src/kahawai
COPY . .

ARG KAHAWAI_BUILD=container
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kahawai-target-ubuntu2604,target=/usr/src/kahawai/target \
    KAHAWAI_BUILD="${KAHAWAI_BUILD}" cargo build --locked --release --bin kahawai \
    && install -D -m 0755 -s target/release/kahawai /out/kahawai

# GStreamer 1.24 from Ubuntu 24.04 recognizes Blackwell GPUs but its NVENC
# preset negotiation fails against current drivers. 1.28 supports the same
# older devices while also making H.264/HEVC/AV1 NVENC usable on Blackwell.
FROM ubuntu:26.04 AS runtime

ARG DEBIAN_FRONTEND=noninteractive
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    intel_packages=""; \
    if [ "$arch" = amd64 ]; then \
        # iHD covers Gen8+; i965-shaders keeps older Quick Sync machines useful.
        intel_packages="intel-media-va-driver-non-free i965-va-driver-shaders libvpl2 onevpl-tools"; \
    fi; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        fontconfig \
        gstreamer1.0-gl \
        gstreamer1.0-libav \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-ugly \
        gstreamer1.0-tools \
        gstreamer1.0-vaapi \
        gstreamer1.0-x \
        mesa-va-drivers \
        tesseract-ocr \
        tesseract-ocr-eng \
        vainfo \
        $intel_packages; \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/kahawai /usr/local/bin/kahawai
COPY --from=builder /out/gstreamer-1.0/ /usr/local/lib/gstreamer-1.0/

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
