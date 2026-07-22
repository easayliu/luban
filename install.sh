#!/usr/bin/env bash
#
# luban 一键部署脚本（Docker 版）
#
# 用法：
#   bash install.sh                      # 在克隆好的仓库里：本地构建镜像并启动
#   LUBAN_IMAGE=registry.example.com/you/luban:latest bash install.sh   # 用预构建镜像
#
# 环境变量：
#   INSTALL_DIR   安装目录，默认 ~/luban
#   LUBAN_IMAGE   预构建镜像地址；未设置时用当前仓库的 Dockerfile 本地构建
#   PORT          宿主机监听端口，默认 4600
#   AUTO_START    安装后是否立即启动，默认 yes
#

set -euo pipefail

INSTALL_DIR="${INSTALL_DIR:-$HOME/luban}"
LUBAN_IMAGE="${LUBAN_IMAGE:-}"
PORT="${PORT:-4600}"
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

  # 判定构建方式：有 LUBAN_IMAGE 用预构建镜像，否则要求在仓库内本地构建。
  local REPO_ROOT=""
  if [[ -z "$LUBAN_IMAGE" ]]; then
    local here; here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    if [[ -f "$here/Dockerfile" ]]; then
      REPO_ROOT="$here"
      info "未指定 LUBAN_IMAGE，将用仓库 Dockerfile 本地构建：$REPO_ROOT"
    else
      error "未指定 LUBAN_IMAGE，且当前目录没有 Dockerfile。"
      error "请在克隆好的 luban 仓库内运行，或设置 LUBAN_IMAGE 指向预构建镜像。"
      exit 1
    fi
  fi

  mkdir -p "$INSTALL_DIR/config"
  info "安装目录：$INSTALL_DIR"

  # ---------- docker-compose.yml ----------
  local COMPOSE_PATH="$INSTALL_DIR/docker-compose.yml"
  if [[ -n "$LUBAN_IMAGE" ]]; then
    cat > "$COMPOSE_PATH" <<EOF
services:
  luban:
    image: ${LUBAN_IMAGE}
    container_name: luban
    ports:
      - "${PORT}:4600"
    volumes:
      - ./config/:/app/config/
    restart: unless-stopped
EOF
  else
    cat > "$COMPOSE_PATH" <<EOF
services:
  luban:
    build: ${REPO_ROOT}
    image: luban:latest
    container_name: luban
    ports:
      - "${PORT}:4600"
    volumes:
      - ./config/:/app/config/
    restart: unless-stopped
EOF
  fi
  ok "已写入 $COMPOSE_PATH"

  if [[ "$AUTO_START" != "yes" ]]; then
    info "AUTO_START=no，跳过启动"
    print_summary
    return
  fi

  (
    cd "$INSTALL_DIR"
    if [[ -n "$LUBAN_IMAGE" ]]; then
      info "拉取镜像 ${LUBAN_IMAGE} ..."
      $COMPOSE pull
    else
      info "本地构建镜像（首次较慢）..."
      $COMPOSE build
    fi
    info "启动容器 ..."
    $COMPOSE up -d
  )

  ok "启动完成"
  print_summary
}

print_summary() {
  cat <<EOF

${BOLD}${GREEN}✓ luban 部署完成${RESET}

  目录:      ${INSTALL_DIR}
  登录页:    http://127.0.0.1:${PORT}/

常用命令（在 ${INSTALL_DIR} 目录下执行）:
  查看日志   ${BOLD}docker compose logs -f${RESET}
  停止       ${BOLD}docker compose down${RESET}
  升级       ${BOLD}docker compose pull && docker compose up -d${RESET}  （预构建镜像）
             ${BOLD}docker compose up -d --build${RESET}                 （本地构建）

登录说明:
  luban 登录需要浏览器授权 + 粘贴。远程服务器上可用 SSH 端口转发到本机：
  ${BOLD}ssh -L ${PORT}:127.0.0.1:${PORT} <user>@<server>${RESET}
  然后本机访问 http://127.0.0.1:${PORT}/ 完成登录；凭证持久化在 ${INSTALL_DIR}/config/。

EOF
}

main "$@"
