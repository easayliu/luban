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

# ---- 依赖预编译层 ----
# 只拷清单、用一个空 main 先把**依赖**编出来。这一层的失效条件仅是 Cargo.toml/Cargo.lock
# 变化，改业务代码不会动它，于是 BoringSSL 那几分钟只在换依赖时才付一次。
# 没有这一层的话，`COPY src` 在 `cargo build` 之前，任何一行代码改动都会让整层失效、
# 把全部依赖连同 BoringSSL 重编一遍——换 wreq 之后这个代价明显变大，故补上。
#
# 空 main 编译不需要 admin-ui/dist：rust-embed 的宏在**我们自己的 crate** 里才展开，
# 这一层还没有真实的 src，轮不到它读目录。
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# ---- 真实构建 ----
COPY src ./src
# rust-embed 在编译期读取 admin-ui/dist（相对 crate 根 /app）。
COPY --from=frontend /app/admin-ui/dist ./admin-ui/dist
# 先删掉空 main 留下的产物：crate 名没变，不删的话有让 cargo 误判为「已是最新」的余地，
# 那会把一个空壳二进制打进镜像（起来就是 CMD 立刻退出，且不报错）。依赖的产物不动，
# 所以这一步只重编 luban 自己。
RUN rm -f target/release/luban target/release/deps/luban-* \
    && cargo build --release

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
