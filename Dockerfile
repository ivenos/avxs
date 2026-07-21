# syntax=docker/dockerfile:1

ARG SVT_AV1_VERSION=v4.2.0
# Rolling-release repo: pin a main commit, bump via PR.
ARG SVT_AV1_HDR_REF=8b4b9f5624cb70c2363a7cebb553110c1447dd4c
ARG FFMS2_VERSION=5.0
ARG VSHIP_VERSION=v5.1.0
ARG RUST_VERSION=1.97.1

FROM alpine:3.24 AS builder

ARG SVT_AV1_VERSION
ARG SVT_AV1_HDR_REF
ARG FFMS2_VERSION
ARG VSHIP_VERSION
ARG RUST_VERSION
ARG TARGETARCH

# clang + vulkan-headers/loader build Vship's FFVship (Vulkan backend, no CUDA/HIP).
# nasm/yasm are x86-only assemblers; SVT-AV1 uses NEON on arm64 instead.
RUN apk add --no-cache \
        build-base \
        clang \
        cmake \
        git \
        curl \
        pkgconf \
        autoconf \
        automake \
        libtool \
        ffmpeg-dev \
        vulkan-headers \
        vulkan-loader-dev \
        zlib-dev \
        ca-certificates && \
    [ "$TARGETARCH" != "amd64" ] || apk add --no-cache nasm yasm

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain ${RUST_VERSION} --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"

RUN git clone --depth 1 --branch ${SVT_AV1_VERSION} \
        https://gitlab.com/AOMediaCodec/SVT-AV1.git /svt-av1 && \
    cmake -B /svt-av1/build /svt-av1 \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local \
        -DBUILD_SHARED_LIBS=OFF \
        -DENABLE_AVX512=$([ "$TARGETARCH" = "amd64" ] && echo ON || echo OFF) \
        -DNATIVE=OFF && \
    cmake --build /svt-av1/build --parallel $(nproc) && \
    cmake --install /svt-av1/build && \
    rm -rf /svt-av1

RUN git clone --filter=blob:none --no-checkout \
        https://github.com/juliobbv-p/svt-av1-hdr.git /svt-av1-hdr && \
    git -C /svt-av1-hdr checkout ${SVT_AV1_HDR_REF} && \
    cmake -B /svt-av1-hdr/build /svt-av1-hdr \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX=/usr/local/hdr \
        -DBUILD_SHARED_LIBS=OFF \
        -DENABLE_AVX512=$([ "$TARGETARCH" = "amd64" ] && echo ON || echo OFF) \
        -DNATIVE=OFF && \
    cmake --build /svt-av1-hdr/build --parallel $(nproc) && \
    cmake --install /svt-av1-hdr/build && \
    rm -rf /svt-av1-hdr

# Build FFMS2 as a shared library to avoid C++ static-init crashes when
# embedded in a Rust binary
RUN git clone --depth 1 --branch ${FFMS2_VERSION} \
        https://github.com/FFMS/ffms2.git /ffms2 && \
    cd /ffms2 && \
    mkdir -p src/config && \
    autoreconf -fiv && \
    ./configure --prefix=/usr/local --enable-shared=yes --enable-static=no && \
    make -j$(nproc) && \
    make install && \
    rm -rf /ffms2

RUN git clone --depth 1 --branch ${VSHIP_VERSION} \
        https://codeberg.org/Line-fr/Vship.git /vship && \
    cd /vship && \
    make buildVulkan && \
    PKG_CONFIG_PATH=/usr/local/lib/pkgconfig make buildFFVSHIP && \
    install -m755 FFVship /usr/local/bin/FFVship && \
    install -m755 libvship.so /usr/local/lib/libvship.so && \
    rm -rf /vship

WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src

ENV PKG_CONFIG_PATH=/usr/local/lib/pkgconfig
ENV RUSTFLAGS="-C target-feature=-crt-static"
RUN --mount=type=cache,target=/root/.cargo/registry,id=cargo-registry-${TARGETARCH} \
    --mount=type=cache,target=/root/.cargo/git,id=cargo-git-${TARGETARCH} \
    --mount=type=cache,target=/src/target,id=cargo-target-${TARGETARCH} \
    cargo build --release && \
    cp /src/target/release/avxs /avxs

FROM alpine:3.24 AS runtime

ARG TARGETARCH

RUN apk add --no-cache \
        ffmpeg \
        mkvtoolnix \
        libstdc++ \
        libgcc \
        vulkan-loader \
        mesa-vulkan-swrast && \
    [ "$TARGETARCH" != "amd64" ] || apk add --no-cache mesa-vulkan-intel mesa-vulkan-ati

COPY --from=builder /usr/local/bin/SvtAv1EncApp     /usr/local/bin/SvtAv1EncApp
COPY --from=builder /usr/local/hdr/bin/SvtAv1EncApp /usr/local/bin/SvtAv1EncApp-hdr
COPY --from=builder /usr/local/bin/ffmsindex         /usr/local/bin/ffmsindex
COPY --from=builder /avxs                             /usr/local/bin/avxs
# libffms2.so is not in Alpine's package manager - copy from builder
COPY --from=builder /usr/local/lib/libffms2.so*      /usr/local/lib/
# FFVship (GPU metric tool, Vulkan) + libvship for target_quality
COPY --from=builder /usr/local/bin/FFVship           /usr/local/bin/FFVship
COPY --from=builder /usr/local/lib/libvship.so       /usr/local/lib/
# Add /usr/local/lib to musl dynamic linker search path (filename is arch-specific)
RUN printf '/lib\n/usr/lib\n/usr/local/lib\n' > /etc/ld-musl-$(uname -m).path

VOLUME ["/input", "/output"]

ENV AVXS_INPUT_DIR=/input
ENV AVXS_OUTPUT_DIR=/output
ENV AVXS_POLL_INTERVAL=60

ENTRYPOINT ["/usr/local/bin/avxs"]
