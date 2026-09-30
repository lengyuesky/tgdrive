# ---------- 阶段 1:构建前端 ----------
FROM node:22.22-alpine AS frontend
WORKDIR /frontend
COPY frontend/package.json frontend/package-lock.json ./
RUN npm ci
COPY frontend/ ./
RUN npm run build

# ---------- 阶段 2:构建后端 ----------
# 注意:构建与运行阶段必须同一 Debian 版本,否则 glibc 不匹配
FROM rust:1.97-slim-bookworm AS backend
WORKDIR /app
# 先编译一个空 main 缓存依赖层,源码变更时无需重编全部依赖
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked \
    && mkdir /app/empty-data

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

# ---------- 阶段 4:运行时 ----------
# distroless:无 shell 与包管理器,自带 glibc 与 CA 证书(与构建阶段同为 bookworm)。
# 镜像内无法 mkdir/chown,/data 由构建阶段建好后以 1000:1000 复制进来,
# 这样新建的命名卷也继承正确属主。
FROM gcr.io/distroless/cc-debian12
COPY --from=backend --chown=1000:1000 /app/empty-data /data
WORKDIR /app
COPY --from=backend /app/target/release/tgdrive /usr/local/bin/tgdrive
COPY --from=usque /opt/usque/usque /usr/local/bin/usque
COPY --from=usque /opt/usque/LICENSE.md /usr/share/licenses/usque/LICENSE.md
COPY --from=frontend /frontend/dist /app/static
ENV TGDRIVE_STATIC_DIR=/app/static \
    TGDRIVE_DB_PATH=/data/tgdrive.db \
    TGDRIVE_LOCAL_DATA_ROOT=/var/lib/telegram-bot-api \
    TGDRIVE_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
USER 1000:1000
# 健康检查用二进制自带子命令,无需安装 curl
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s CMD ["tgdrive", "healthcheck"]
ENTRYPOINT ["tgdrive"]
CMD ["serve"]
