# ---------- 前端构建 ----------
FROM node:22-alpine AS frontend
WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml ./
RUN npm install -g pnpm@10 && pnpm install --frozen-lockfile
COPY admin-ui ./
RUN pnpm build

# ---------- Rust 构建 ----------
# 用 Debian glibc：reqwest 走 rustls(aws-lc-rs)，需要 cmake + C 工具链，
# 在 musl/alpine 上编译 aws-lc-sys 很麻烦，glibc 直接可用。
FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake build-essential perl pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# rust-embed 在编译期读取 admin-ui/dist（相对 crate 根 /app）。
COPY --from=frontend /app/admin-ui/dist ./admin-ui/dist
RUN cargo build --release

# ---------- 运行时 ----------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/luban /usr/local/bin/luban

# 凭证持久化目录（挂载卷）
ENV LUBAN_HOME=/app/config
VOLUME ["/app/config"]

EXPOSE 4600
# 容器内绑 0.0.0.0；默认即不自动开浏览器
CMD ["luban", "--host", "0.0.0.0", "--port", "4600"]
