# controller と worker と publisher の 3 イメージを 1 つの Dockerfile から作る
FROM rust:1.97-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked --bin edamame-controller --bin edamame-worker --bin edamame-publisher
# distroless には shell が無く RUN でファイルを足せないため、DNS 解決に使う設定をここで作る
RUN printf 'hosts: files dns\n' > /etc/nsswitch.conf

# glibc と libgcc を含む distroless。実行ユーザーは既定の root を使う
FROM gcr.io/distroless/cc-debian12 AS runtime
COPY --from=builder /etc/nsswitch.conf /etc/nsswitch.conf

FROM runtime AS controller
COPY --from=builder /build/target/release/edamame-controller /usr/local/bin/edamame-controller
ENTRYPOINT ["edamame-controller"]

FROM runtime AS worker
COPY --from=builder /build/target/release/edamame-worker /usr/local/bin/edamame-worker
ENTRYPOINT ["edamame-worker"]

# publisher の ffmpeg。Intel VSR (raisr) は ffmpeg 本体にパッチを当てて組み込むプラグインのため、
# 配布されている ffmpeg 入りの image では用意できず、自前でビルドする
FROM ubuntu:24.04 AS publisher-ffmpeg

ENV DEBIAN_FRONTEND=noninteractive

# ffmpeg と raisr のビルドに必要なもの
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        gnupg \
        libssl-dev \
        libsrt-openssl-dev \
        libvpl-dev \
        libx264-dev \
        nasm \
        ocl-icd-opencl-dev \
        opencl-headers \
        patch \
        pkg-config \
        xz-utils \
        zlib1g-dev \
    && rm -rf /var/lib/apt/lists/*

# Intel IPP は Intel の apt リポジトリから入れる（Intel VSR ライブラリのビルド手順と同じ）
ARG IPP_PACKAGE=intel-oneapi-ipp-devel-2022.0
RUN curl -fsSL "https://apt.repos.intel.com/intel-gpg-keys/GPG-PUB-KEY-INTEL-SW-PRODUCTS.PUB" \
        | gpg --dearmor > /usr/share/keyrings/oneapi-archive-keyring.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/oneapi-archive-keyring.gpg] https://apt.repos.intel.com/oneapi all main" \
        > /etc/apt/sources.list.d/intel-oneAPI.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends "${IPP_PACKAGE}" \
    && rm -rf /var/lib/apt/lists/*
ENV IPPROOT=/opt/intel/oneapi/ipp/latest

# NVENC のヘッダー。Ubuntu には無いためタグを固定して取る
ARG NV_CODEC_HEADERS=n12.2.72.0
RUN curl -fsSL -o /tmp/nv-codec-headers.tar.gz \
        "https://github.com/FFmpeg/nv-codec-headers/archive/refs/tags/${NV_CODEC_HEADERS}.tar.gz" \
    && mkdir -p /tmp/nv-codec-headers \
    && tar -xzf /tmp/nv-codec-headers.tar.gz --strip-components=1 -C /tmp/nv-codec-headers \
    && make -C /tmp/nv-codec-headers install PREFIX=/usr/local

# ffmpeg のソースに Intel VSR (raisr) のプラグインを組み込む
ARG FFMPEG_VERSION=7.1
ARG FFMPEG_SHA256=40973d44970dbc83ef302b0609f2e74982be2d85916dd2ee7472d30678a7abe6
ARG RAISR_COMMIT=ea37e77bcf705a5f8ffeab56b83424bed6ea2461
RUN curl -fsSL -o /tmp/ffmpeg.tar.xz "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" \
    && echo "${FFMPEG_SHA256}  /tmp/ffmpeg.tar.xz" | sha256sum -c - \
    && mkdir -p /opt/ffmpeg-src \
    && tar -xJf /tmp/ffmpeg.tar.xz --strip-components=1 -C /opt/ffmpeg-src \
    && curl -fsSL -o /tmp/raisr.tar.gz \
        "https://github.com/OpenVisualCloud/Video-Super-Resolution-Library/archive/${RAISR_COMMIT}.tar.gz" \
    && mkdir -p /opt/raisr-src \
    && tar -xzf /tmp/raisr.tar.gz --strip-components=1 -C /opt/raisr-src \
    && cd /opt/ffmpeg-src \
    && patch -p1 < /opt/raisr-src/ffmpeg/0001-Upgrade-Raisr-ffmpeg-plugin-to-n7.1-from-n6.1.1.patch \
    && cp /opt/raisr-src/ffmpeg/vf_raisr.c libavfilter/

# raisr 本体。CPU 側のカーネルは AVX2 相当にそろえ、拡大は OpenCL で Intel GPU に任せる
RUN cd /opt/raisr-src \
    && sed -i 's/-march=native/-march=x86-64-v3/g' CMakeLists.txt \
    && ./build.sh \
        -DENABLE_RAISR_OPENCL=ON \
        -DCMAKE_INSTALL_PREFIX=/opt/raisr \
        -DCMAKE_LIBRARY_PATH="/opt/intel/oneapi/ipp/latest/lib;/opt/intel/oneapi/ipp/latest/lib/intel64" \
        -DCMAKE_C_FLAGS="-I/opt/intel/oneapi/ipp/latest/include -I/opt/intel/oneapi/ipp/latest/include/ipp" \
        -DCMAKE_CXX_FLAGS="-I/opt/intel/oneapi/ipp/latest/include -I/opt/intel/oneapi/ipp/latest/include/ipp"

# ffmpeg 本体。QSV (libvpl)、SRT、NVENC、libx264 と raisr を有効にする
RUN cd /opt/ffmpeg-src \
    && ./configure \
        --disable-debug \
        --disable-doc \
        --enable-libipp \
        --enable-libvpl \
        --enable-libsrt \
        --enable-ffnvcodec \
        --enable-nvenc \
        --enable-libx264 \
        --enable-gpl \
        --extra-cflags="-fopenmp -I/opt/raisr/include -I/opt/intel/oneapi/ipp/latest/include -I/opt/intel/oneapi/ipp/latest/include/ipp" \
        --extra-ldflags="-fopenmp -L/opt/intel/oneapi/ipp/latest/lib -L/opt/intel/oneapi/ipp/latest/lib/intel64 -L/opt/raisr/lib" \
        --extra-libs="-lraisr -lstdc++ -lippcore -lippvm -lipps -lippi -lOpenCL -lm" \
        --prefix=/opt/ffmpeg \
    && make -j"$(nproc)" \
    && make install

# 実行に使うファイルだけを 1 か所に集める
RUN mkdir -p /opt/raisr-runtime/bin /opt/raisr-runtime/lib /opt/raisr-runtime/share \
    && cp -a /opt/ffmpeg/bin/ffmpeg /opt/ffmpeg/bin/ffprobe /opt/raisr-runtime/bin/ \
    && find /opt/intel/oneapi/ipp -name 'libipp*.so*' -exec cp -a {} /opt/raisr-runtime/lib/ \; \
    && cp -a /usr/lib/x86_64-linux-gnu/libsrt*.so* /opt/raisr-runtime/lib/ \
    && cp -a /usr/lib/x86_64-linux-gnu/libx264*.so* /opt/raisr-runtime/lib/ \
    && cp -a /opt/raisr-src/filters_2x /opt/raisr-runtime/share/

# publisher は ffmpeg で NVEnc、QSV、SRT、RTMP を扱うため、distroless ではなく ffmpeg を含む image にする
# GPU は NVIDIA Container Toolkit か --device /dev/dri でホストのドライバごと渡す。
# QSV には VA-API のドライバと oneVPL の GPU ランタイムが要る。
# 4K の超解像 (raisr) は OpenCL を使うため、intel-opencl-icd も入れる
FROM ubuntu:26.04 AS publisher
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        intel-media-va-driver-non-free \
        intel-opencl-icd \
        libgomp1 \
        libmfx-gen1.2 \
        libssl3t64 \
        libvpl2 \
        vainfo \
    && rm -rf /var/lib/apt/lists/*
# raisr 入りの ffmpeg と、その実行に必要なライブラリを ffmpeg のビルド結果から持ってくる
COPY --from=publisher-ffmpeg /opt/raisr-runtime/bin/ /usr/local/bin/
COPY --from=publisher-ffmpeg /opt/raisr-runtime/lib/ /opt/raisr/lib/
COPY --from=publisher-ffmpeg /opt/raisr-runtime/share/filters_2x /usr/local/share/raisr/filters_2x
RUN printf '/opt/raisr/lib\n' > /etc/ld.so.conf.d/raisr.conf \
    && ldconfig
COPY --from=builder /build/target/release/edamame-publisher /usr/local/bin/edamame-publisher
RUN mkdir -p /work
ENTRYPOINT ["edamame-publisher"]
