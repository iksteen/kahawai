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
ARG GST_PLUGINS_RS_VERSION=0.14.4
ARG GST_PLUGINS_RS_REV=95a7172f82d0ec816e4e89111a762c24d5c47b22

FROM rust:${RUST_VERSION}-bookworm AS builder

ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        clang \
        cmake \
        libgstreamer-plugins-base1.0-dev \
        libgstreamer1.0-dev \
        libass-dev \
        libleptonica-dev \
        libtesseract-dev \
        pkg-config \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

ARG GST_PLUGINS_RS_VERSION
ARG GST_PLUGINS_RS_REV
RUN git clone --depth 1 --branch "$GST_PLUGINS_RS_VERSION" \
        https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs.git \
        /tmp/gst-plugins-rs \
    && test "$(git -C /tmp/gst-plugins-rs rev-parse HEAD)" = "$GST_PLUGINS_RS_REV"

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/tmp/gst-plugins-rs/target \
    cargo build --locked --release \
        --manifest-path /tmp/gst-plugins-rs/Cargo.toml \
        -p gst-plugin-fmp4 \
        -p gst-plugin-hlssink3 \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgstfmp4.so \
        /out/gstreamer-1.0/libgstfmp4.so \
    && install -D -m 0755 -s /tmp/gst-plugins-rs/target/release/libgsthlssink3.so \
        /out/gstreamer-1.0/libgsthlssink3.so

WORKDIR /usr/src/kahawai
COPY . .

ARG KAHAWAI_BUILD=container
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/src/kahawai/target \
    KAHAWAI_BUILD="${KAHAWAI_BUILD}" cargo build --locked --release --bin kahawai \
    && install -D -m 0755 -s target/release/kahawai /out/kahawai

FROM ubuntu:24.04 AS runtime

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
