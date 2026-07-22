#!/usr/bin/env bash
#
# luban 一键安装脚本（Docker 版）
#
# 用法：
#   curl -fsSL https://raw.githubusercontent.com/easayliu/luban/main/install.sh | bash
#   bash install.sh
#
# 环境变量：
#   INSTALL_DIR   安装目录，默认 ~/luban
#   IMAGE_OWNER   镜像 owner，默认 easayliu
#   IMAGE_TAG     镜像 tag，默认 latest（由 tag 触发的 CI 构建产出）
#   IMAGE_REG     镜像 registry，默认 ghcr.io；国内可用 ghcr.nju.edu.cn
#   PORT          宿主机监听端口，默认 4600
#   LUBAN_API_KEY 接入用 API Key，默认留空（改由网页「接入设置」管理）
#   AUTO_START    安装后是否立即启动，默认 yes
#

set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-$HOME/luban}"
IMAGE_OWNER="${IMAGE_OWNER:-easayliu}"
IMAGE_TAG="${IMAGE_TAG:-latest}"
IMAGE_REG="${IMAGE_REG:-ghcr.io}"
PORT="${PORT:-4600}"
LUBAN_API_KEY="${LUBAN_API_KEY:-}"
AUTO_START="${AUTO_START:-yes}"

RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; BLUE=$'\033[34m'; BOLD=$'\033[1m'; RESET=$'\033[0m'

info()  { printf '%s[info]%s %s\n'  "$BLUE"   "$RESET" "$*"; }
warn()  { printf '%s[warn]%s %s\n'  "$YELLOW" "$RESET" "$*"; }
error() { printf '%s[error]%s %s\n' "$RED"    "$RESET" "$*" >&2; }
ok()    { printf '%s[ok]%s %s\n'    "$GREEN"  "$RESET" "$*"; }

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || { error "缺少依赖：$1，请先安装"; exit 1; }
}

detect_compose() {
  if docker compose version >/dev/null 2>&1; then
    echo "docker compose"
  elif command -v docker-compose >/dev/null 2>&1; then
    echo "docker-compose"
  else
    error "未检测到 docker compose / docker-compose"
    exit 1
  fi
}

main() {
  require_cmd docker
  local COMPOSE
  COMPOSE="$(detect_compose)"
  ok "docker 就绪；compose 命令：$COMPOSE"

  mkdir -p "$INSTALL_DIR/config"
  info "安装目录：$INSTALL_DIR"

  # ---------- docker-compose.yml ----------
  local COMPOSE_PATH="$INSTALL_DIR/docker-compose.yml"
  cat > "$COMPOSE_PATH" <<EOF
services:
  luban:
    image: ${IMAGE_REG}/${IMAGE_OWNER}/luban:${IMAGE_TAG}
    container_name: luban
    extra_hosts:
      - "host.docker.internal:host-gateway"
    ports:
      - "${PORT}:4600"
    environment:
      - LUBAN_API_KEY=${LUBAN_API_KEY}
    volumes:
      - ./config/:/app/config/
    restart: unless-stopped
EOF
  ok "已写入 $COMPOSE_PATH"

  if [[ "$AUTO_START" != "yes" ]]; then
    info "AUTO_START=no，跳过启动"
    print_summary
    return
  fi

  (
    cd "$INSTALL_DIR"
    info "拉取镜像 ${IMAGE_REG}/${IMAGE_OWNER}/luban:${IMAGE_TAG} ..."
    $COMPOSE pull
    info "启动容器 ..."
    $COMPOSE up -d
  )

  ok "启动完成"
  print_summary
}

print_summary() {
  cat <<EOF

${BOLD}${GREEN}✓ luban 安装完成${RESET}

  目录:      ${INSTALL_DIR}
  网页:      http://127.0.0.1:${PORT}/

后续步骤（浏览器打开上面的网页）:
  1. 「添加账号」用 Claude 订阅账号授权登录（可加多个）
  2. 「接入设置」生成/填写接入 Key（或用 LUBAN_API_KEY 环境变量）

Claude Code 接入:
  export ANTHROPIC_BASE_URL=http://127.0.0.1:${PORT}
  export ANTHROPIC_AUTH_TOKEN=<接入设置里的 Key>

常用命令（在 ${INSTALL_DIR} 目录下执行）:
  查看日志   ${BOLD}docker compose logs -f${RESET}
  停止       ${BOLD}docker compose down${RESET}
  升级       ${BOLD}docker compose pull && docker compose up -d${RESET}

  凭证库持久化在 ${INSTALL_DIR}/config/（重启不丢）。
  远程服务器登录：本机 ${BOLD}ssh -L ${PORT}:127.0.0.1:${PORT} <user>@<server>${RESET} 后访问上面的网页。

EOF
}

main "$@"
