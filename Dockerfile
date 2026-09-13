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
    && cargo build --release \
    && rm -rf src
COPY migrations ./migrations
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---------- 阶段 3:运行时 ----------
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && groupadd --gid 1000 tgdrive \
    && useradd --uid 1000 --gid tgdrive --create-home --shell /usr/sbin/nologin tgdrive \
    && install -d -o tgdrive -g tgdrive /data \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=backend /app/target/release/tgdrive /usr/local/bin/tgdrive
COPY --from=frontend /frontend/dist /app/static
ENV TGDRIVE_STATIC_DIR=/app/static \
    TGDRIVE_DB_PATH=/data/tgdrive.db \
    TGDRIVE_LOCAL_DATA_ROOT=/var/lib/telegram-bot-api \
    TGDRIVE_LISTEN=0.0.0.0:8080
VOLUME /data
EXPOSE 8080
USER tgdrive
# 健康检查用二进制自带子命令,无需安装 curl
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s CMD ["tgdrive", "healthcheck"]
ENTRYPOINT ["tgdrive"]
CMD ["serve"]
