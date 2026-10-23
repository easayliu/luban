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
# 数据存在 PostgreSQL（compose 里的 postgres 服务，只在内部网络可达）。数据库密码首次安装时
# 随机生成，写进安装目录的 .env；重装时沿用已有的 .env，不会换掉。
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

  mkdir -p "$INSTALL_DIR/config" "$INSTALL_DIR/pgdata"
  info "安装目录：$INSTALL_DIR"

  # ---------- .env（数据库密码）----------
  local ENV_PATH="$INSTALL_DIR/.env"
  if [[ -f "$ENV_PATH" ]] && grep -q '^POSTGRES_PASSWORD=' "$ENV_PATH"; then
    info "沿用已有的数据库密码（$ENV_PATH）"
  else
    local PG_PASSWORD
    # head 读够就关管道，tr 吃 SIGPIPE 退出非零；pipefail + set -e 下要兜住，否则脚本就此退出。
    PG_PASSWORD="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom 2>/dev/null | head -c 32 || true)"
    [[ ${#PG_PASSWORD} -eq 32 ]] || { error "生成数据库密码失败（读 /dev/urandom 出错）"; exit 1; }
    # 老 .env 末行没有换行的话，直接追加会粘到上一行后面。
    if [[ -s "$ENV_PATH" && -n "$(tail -c 1 "$ENV_PATH")" ]]; then
      echo >> "$ENV_PATH"
    fi
    echo "POSTGRES_PASSWORD=${PG_PASSWORD}" >> "$ENV_PATH"
    chmod 600 "$ENV_PATH"
    ok "已生成数据库密码并写入 $ENV_PATH"
  fi

  # ---------- .env（PG 内存参数）----------
  # 按本机内存套官方建议：shared_buffers 1/4、effective_cache_size 3/4、maintenance_work_mem
  # 1/16（封顶 1GB），max_wal_size 随 shared_buffers 同步放大。.env 里已有的项不覆盖，
  # 手调过的值重装也保留；读不到 /proc/meminfo 时一项不写，compose 用自带的缺省值。
  local MEM_KB
  MEM_KB="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null || true)"
  if [[ -n "$MEM_KB" ]]; then
    local MEM_MB=$((MEM_KB / 1024))
    local SB_MB=$((MEM_MB / 4))
    local MWM_MB=$((MEM_MB / 16))
    ((MWM_MB > 1024)) && MWM_MB=1024
    ((MWM_MB < 64)) && MWM_MB=64
    local WAL_MB=$((SB_MB > 1024 ? SB_MB : 1024))
    local -a PG_PARAMS=(
      "PG_SHARED_BUFFERS=${SB_MB}MB"
      "PG_EFFECTIVE_CACHE_SIZE=$((MEM_MB * 3 / 4))MB"
      "PG_MAINTENANCE_WORK_MEM=${MWM_MB}MB"
      "PG_MAX_WAL_SIZE=${WAL_MB}MB"
      "PG_MIN_WAL_SIZE=$((WAL_MB / 4))MB"
    )
    if [[ -s "$ENV_PATH" && -n "$(tail -c 1 "$ENV_PATH")" ]]; then
      echo >> "$ENV_PATH"
    fi
    local kv
    for kv in "${PG_PARAMS[@]}"; do
      if ! grep -q "^${kv%%=*}=" "$ENV_PATH"; then
        echo "$kv" >> "$ENV_PATH"
      fi
    done
    ok "已按本机内存 ${MEM_MB}MB 写入 PG 参数（${ENV_PATH}，已有的项不改）"
  fi

  # ---------- docker-compose.yml ----------
  local COMPOSE_PATH="$INSTALL_DIR/docker-compose.yml"
  # 重装时沿用已有 compose 里 postgres 的端口映射：模板默认不对外开放数据库，有人手动加过
  # （外部程序要直连库）的话，重写 compose 不能把它悄悄删掉。只认 postgres 服务块里
  # `ports:` 下紧跟的那几行 `- ...`。
  local PG_PORTS=""
  if [[ -f "$COMPOSE_PATH" ]]; then
    PG_PORTS="$(awk '
      /^  [^ #]/ { in_pg = ($0 ~ /^  postgres:/); grab = 0; next }
      in_pg && /^    ports:/ { grab = 1; print; next }
      in_pg && grab && /^      - / { print; next }
      { grab = 0 }
    ' "$COMPOSE_PATH")"
    if [[ -n "$PG_PORTS" ]]; then
      PG_PORTS=$'\n'"$PG_PORTS"
      info "沿用已有的 postgres 端口映射：$(echo "$PG_PORTS" | awk '/- /{print $2}' | xargs)"
    fi
  fi
  # \${POSTGRES_PASSWORD} 原样写进文件，由 compose 从 .env 取值，密码不落在 compose 文件里。
  cat > "$COMPOSE_PATH" <<EOF
services:
  postgres:
    image: postgres:17
    container_name: luban-postgres
    environment:
      - POSTGRES_USER=luban
      - POSTGRES_PASSWORD=\${POSTGRES_PASSWORD}
      - POSTGRES_DB=luban
    shm_size: 256mb${PG_PORTS}
    command:
      - postgres
      - -c
      - shared_buffers=\${PG_SHARED_BUFFERS:-1GB}
      - -c
      - effective_cache_size=\${PG_EFFECTIVE_CACHE_SIZE:-3GB}
      - -c
      - maintenance_work_mem=\${PG_MAINTENANCE_WORK_MEM:-256MB}
      - -c
      - max_wal_size=\${PG_MAX_WAL_SIZE:-2GB}
      - -c
      - min_wal_size=\${PG_MIN_WAL_SIZE:-512MB}
      - -c
      - random_page_cost=\${PG_RANDOM_PAGE_COST:-1.1}
      - -c
      - effective_io_concurrency=\${PG_EFFECTIVE_IO_CONCURRENCY:-200}
    volumes:
      - ./pgdata/:/var/lib/postgresql/data/
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U luban -d luban"]
      interval: 5s
      timeout: 3s
      retries: 20
    restart: unless-stopped

  luban:
    image: ${IMAGE_REG}/${IMAGE_OWNER}/luban:${IMAGE_TAG}
    container_name: luban
    init: true
    extra_hosts:
      - "host.docker.internal:host-gateway"
    ports:
      - "${PORT}:4600"
    environment:
      - LUBAN_API_KEY=${LUBAN_API_KEY}
      - LUBAN_DATABASE_URL=postgres://luban:\${POSTGRES_PASSWORD}@postgres:5432/luban
    volumes:
      - ./config/:/app/config/
    depends_on:
      postgres:
        condition: service_healthy
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
  1. 首次打开需设置管理密码，页面要求的「初始化口令」见日志：
     ${BOLD}docker logs luban 2>&1 | grep setup_token${RESET}
  2. 「添加账号」用 Claude 订阅账号授权登录（可加多个）
  3. 「接入设置」生成/填写接入 Key（或用 LUBAN_API_KEY 环境变量）

Claude Code 接入:
  export ANTHROPIC_BASE_URL=http://127.0.0.1:${PORT}
  export ANTHROPIC_AUTH_TOKEN=<接入设置里的 Key>

常用命令（在 ${INSTALL_DIR} 目录下执行）:
  查看日志   ${BOLD}docker compose logs -f${RESET}
  停止       ${BOLD}docker compose down${RESET}
  升级       ${BOLD}docker compose pull && docker compose up -d${RESET}

  备份（在 ${INSTALL_DIR} 目录下执行；别直接拷 pgdata/，运行中拷出来的数据不一致）:
    ${BOLD}docker compose exec -T postgres pg_dump -U luban -Fc luban > luban-\$(date +%F).dump${RESET}
  连同 ${INSTALL_DIR}/config/secret.key（加密 token 的密钥）与 ${INSTALL_DIR}/.env（数据库密码）
  一起保存——丢了密钥，库里的账号都得重新授权。
  恢复（恢复到重建的空库里；库里现有的数据会全部丢弃）:
    ${BOLD}docker compose stop luban${RESET}
    ${BOLD}docker compose exec -T postgres dropdb -U luban luban${RESET}
    ${BOLD}docker compose exec -T postgres createdb -U luban luban${RESET}
    ${BOLD}docker compose exec -T postgres pg_restore -U luban -d luban < 备份文件.dump${RESET}
    ${BOLD}docker compose start luban${RESET}
  备份来自旧版本也没关系：luban 启动时会把表结构迁到当前版本。别在已有数据的库上用
  pg_restore --clean 覆盖恢复：新版本多出来的表不在备份里、删不掉，迁移记录却回到了旧版本，
  启动时重跑迁移会撞上已经存在的表。
  数据库大版本（compose 里的 postgres:17）升级不能直接换镜像，要先按上面备份、换新版空库后恢复。

  从旧版（SQLite，${INSTALL_DIR}/config/luban.db）升级：数据不会自动迁移。升级前先在旧版
  网页里导出（设置 > 导出），升级后在新版网页里导入。
  远程服务器登录：本机 ${BOLD}ssh -L ${PORT}:127.0.0.1:${PORT} <user>@<server>${RESET} 后访问上面的网页。

EOF
}

main "$@"
