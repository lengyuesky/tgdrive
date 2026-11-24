# syntax=docker/dockerfile:1.7
# 必须提供 --build-context tgdrive-app=../tgdrive-app，两个仓库均无需推送。
# ---------- 阶段 1:构建前端 ----------
FROM node:22.22-alpine AS frontend
COPY --from=tgdrive-app /contract/types /tgdrive-app/contract/types
WORKDIR /frontend
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

# ---------- 阶段 2:构建后端 ----------
# 注意:构建与运行阶段必须同一 Debian 版本,否则 glibc 不匹配
FROM rust:1.99-slim-bookworm AS backend
# 契约与测试 SDK 通过附加上下文提供；Cargo 的 workspace 继承需要根清单。
COPY --from=tgdrive-app /Cargo.toml /tgdrive-app/Cargo.toml
COPY --from=tgdrive-app /contract /tgdrive-app/contract
COPY --from=tgdrive-app /backend-sdk /tgdrive-app/backend-sdk
WORKDIR /app
# 先编译一个空 main 缓存依赖层,源码变更时无需重编全部依赖
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked \
    && mkdir /app/empty-data && chmod 711 /app/empty-data

# ---------- 阶段 3:校验并安装 usque ----------
FROM alpine:3.22 AS usque
ARG TARGETARCH
# 固定上游版本并校验发布包，避免构建时静默引入新版本。
RUN apk add --no-cache curl unzip \
    && case "$TARGETARCH" in \
      amd64) checksum=4117e20695078af9c11edecd1a826c009bbc7ea0b7f64458612b4198910bc313 ;; \
      arm64) checksum=c88b061c2a567f30813d7505637d1fb6fe7ec5898b5b61fd05122409fa5ad925 ;; \
      *) echo "Unsupported usque architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && curl -fL --retry 3 "https://github.com/Diniboy1123/usque/releases/download/v4.2.1/usque_4.2.1_linux_${TARGETARCH}.zip" -o /tmp/usque.zip \
    && echo "$checksum  /tmp/usque.zip" | sha256sum -c - \
    && unzip /tmp/usque.zip -d /opt/usque \
    && chmod 755 /opt/usque/usque

# ---------- 阶段 4:媒体工具 ----------
# 自行编译而不装 Debian 的 ffmpeg 包：后者按桌面用途链接了 SDL、OpenGL、ICU、语音合成等
# 两百多个运行库（约 215 MB），而应用只做探测、解码和取帧。这里保留全部原生解码器、
# 封装格式与滤镜，外部库只留 AV1 解码（dav1d）、H.264 编码（x264）、zlib 和基础镜像已有的 OpenSSL。
FROM debian:bookworm-slim AS media-probe
ARG TARGETARCH
ARG FFMPEG_VERSION=9.0.2
# 发布包签名（FFmpeg release signing key，指纹末段 B4322F04D67658D8）引入此版本时已核验，构建时固定校验和。
ARG FFMPEG_SHA256=8c3850283eb25fa026482078a04051e0be17347b09ef81a0849bec15a96e002e
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl xz-utils gcc libc6-dev make nasm pkg-config \
      libx264-dev libdav1d-dev zlib1g-dev libssl-dev \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fL --retry 3 "https://ffmpeg.org/releases/ffmpeg-${FFMPEG_VERSION}.tar.xz" -o /tmp/ffmpeg.tar.xz \
    && echo "$FFMPEG_SHA256  /tmp/ffmpeg.tar.xz" | sha256sum -c - \
    && mkdir /tmp/ffmpeg && tar -xJf /tmp/ffmpeg.tar.xz -C /tmp/ffmpeg --strip-components=1 \
    && cd /tmp/ffmpeg \
    && ./configure --prefix=/opt/ffmpeg --enable-shared --disable-static \
         --disable-autodetect --disable-debug --disable-doc --disable-ffplay \
         --disable-indevs --enable-indev=lavfi --disable-outdevs \
         --enable-gpl --enable-version3 \
         --enable-libx264 --enable-libdav1d --enable-zlib --enable-iconv --enable-openssl \
    && make -j"$(nproc)" && make install \
    && mkdir -p /opt/ffmpeg/licenses \
    && cp COPYING.GPLv3 LICENSE.md /opt/ffmpeg/licenses/ \
    && printf '源码：https://ffmpeg.org/releases/ffmpeg-%s.tar.xz\nSHA-256：%s\n' "$FFMPEG_VERSION" "$FFMPEG_SHA256" \
         > /opt/ffmpeg/licenses/SOURCE.txt \
    && cd / && rm -rf /tmp/ffmpeg /tmp/ffmpeg.tar.xz
# 依赖按 SONAME 平铺到标准库目录，不依赖构建层的 alternatives 和 ld.so.cache；
# glibc、OpenSSL 等基础镜像自带的库不重复复制。
RUN mkdir -p /probe-root/usr/local/bin /probe-root/usr/share/licenses/media-tools \
    && cp /opt/ffmpeg/bin/ffprobe /opt/ffmpeg/bin/ffmpeg /probe-root/usr/local/bin/ \
    && case "$TARGETARCH" in \
         amd64) library_dir=/usr/lib/x86_64-linux-gnu ;; \
         arm64) library_dir=/usr/lib/aarch64-linux-gnu ;; \
         *) echo "不支持的媒体探测架构：$TARGETARCH" >&2; exit 1 ;; \
       esac \
    && mkdir -p "/probe-root$library_dir" \
    && LD_LIBRARY_PATH=/opt/ffmpeg/lib ldd /opt/ffmpeg/bin/ffprobe /opt/ffmpeg/bin/ffmpeg \
       | awk '/=> \// { print $3 }' | sort -u | while read -r library; do \
         case "$(basename "$library")" in \
           libc.so.6|libm.so.6|libpthread.so.0|libdl.so.2|librt.so.1|libgcc_s.so.1|libstdc++.so.6|libssl.so.3|libcrypto.so.3) continue ;; \
         esac; \
         cp -L "$library" "/probe-root$library_dir/$(basename "$library")"; \
       done \
    && cp -a /opt/ffmpeg/licenses "/probe-root/usr/share/licenses/media-tools/ffmpeg" \
    && LD_LIBRARY_PATH=/opt/ffmpeg/lib /opt/ffmpeg/bin/ffmpeg -hide_banner -buildconf > /probe-root/usr/share/licenses/media-tools/ffmpeg/BUILDCONF.txt \
    && for package in libx264-164 libdav1d6 zlib1g; do \
         install -D -m 644 "/usr/share/doc/$package/copyright" "/probe-root/usr/share/licenses/media-tools/$package/copyright"; \
       done \
    && cp -a /usr/share/common-licenses /probe-root/usr/share/common-licenses
# 与依赖下载分层；共享临时目录只读，后端只写自己的 tmp/。
RUN mkdir -p /probe-root/tmp /probe-root/var/tmp \
    && chmod 555 /probe-root/tmp /probe-root/var/tmp

# ---------- 阶段 5:运行时 ----------
# distroless:无 shell 与包管理器,自带 glibc 与 CA 证书(与构建阶段同为 bookworm)。
# root 宿主管理独立应用 UID、私有套接字和只读配置；应用后端绝不继承 root。
FROM gcr.io/distroless/cc-debian12
COPY --from=backend --chown=0:0 /app/empty-data /data
WORKDIR /app
COPY --from=backend /app/target/release/tgdrive /usr/local/bin/tgdrive
COPY --from=usque /opt/usque/usque /usr/local/bin/usque
COPY --from=usque /opt/usque/LICENSE.md /usr/share/licenses/usque/LICENSE.md
COPY --from=media-probe /probe-root/ /
COPY --from=frontend /frontend/dist /app/static
ENV TGDRIVE_STATIC_DIR=/app/static \
    TGDRIVE_DB_PATH=/data/tgdrive.db \
    TGDRIVE_LOCAL_DATA_ROOT=/var/lib/telegram-bot-api \
    TGDRIVE_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
USER 0:0
# 健康检查用二进制自带子命令请求 /admin/healthz，无需安装 curl
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s CMD ["tgdrive", "healthcheck"]
ENTRYPOINT ["tgdrive"]
CMD ["serve"]
