# ---------- 前端构建 ----------
FROM node:22-alpine AS frontend
WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml ./
RUN npm install -g pnpm@10 && pnpm install --frozen-lockfile
COPY admin-ui ./
RUN pnpm build

# ---------- Rust 构建 ----------
# 用 Debian glibc：wreq 走 BoringSSL(btls-sys)，源码编译需要 cmake + C/C++ 工具链，
# 在 musl/alpine 上折腾这类 -sys crate 很麻烦，glibc 直接可用。
# （build-essential 里的 g++ 是 BoringSSL 必需的，它有 .cc 源文件；perl 供其汇编生成用。）
#
# btls-sys 在这个 slim 镜像上比在 CI runner / 开发机上多要两样东西——两者都因为
# runner 与 macOS 自带而长期看不见，只有这里会踩：
#   - **git**：构建脚本在解压出的 BoringSSL 源码树里跑 `git init` 再 apply 自带的补丁，
#     缺了就是 `boring-sys failed: can't run git`。
#   - **libclang**：BoringSSL 编完后要用 bindgen 生成绑定，缺了就是
#     `Unable to find libclang`。libclang-dev 提供不带版本号的 libclang.so。
# 除此之外它只调 cmake(走 cmake crate)与 xcrun(仅 macOS)；objcopy/nm 挂在
# prefix-symbols feature 下、我们没开；**不需要 Go**。
FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      cmake build-essential perl pkg-config git clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# rust-embed 在编译期读取 admin-ui/dist（相对 crate 根 /app）。
COPY --from=frontend /app/admin-ui/dist ./admin-ui/dist
RUN cargo build --release

# ---------- 运行时 ----------
FROM debian:bookworm-slim
# libstdc++6：BoringSSL 是 C++ 的，btls-sys 发的是 `cargo:rustc-link-lib=stdc++`
# （动态，不是 static），所以二进制运行时需要 libstdc++.so.6。换 wreq 之前用的
# aws-lc-rs 是纯 C，从来不需要它——漏了的话表现是容器起不来而不是构建失败。
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/luban /usr/local/bin/luban

# 凭证持久化目录（挂载卷）
ENV LUBAN_HOME=/app/config
VOLUME ["/app/config"]

EXPOSE 4600
# 容器内绑 0.0.0.0；默认即不自动开浏览器
CMD ["luban", "--host", "0.0.0.0", "--port", "4600"]
