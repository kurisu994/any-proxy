#!/usr/bin/env bash
#
# any-proxy 公网 HTTPS 一键部署（Caddy 自动 TLS）
#
# 用法：
#   ./deploy.sh proxy.example.com
#   ALLOW_TARGETS=api.github.com,.example.com ./deploy.sh proxy.example.com
#
# 也可以把配置写进同目录的 .env（见 .env.example），然后直接 ./deploy.sh。
# 优先级：命令行参数 > shell 环境变量 > .env > 默认值。
#
# 服务器上无需 clone 仓库：脚本会在缺少配置文件时自动生成
# Caddyfile 与 docker-compose.caddy.yml（内容与仓库一致）。
#
# 环境变量（均可选）：
#   DOMAIN             域名，等价于第一个位置参数
#   ALLOW_TARGETS      放行的目标 host，逗号分隔，`.x.com` 匹配子域
#                      留空 = 不限制（任意公网目标都可代理，即开放代理）
#   ALLOW_PORTS        放行的目标端口，留空 = 不限制（1-65535）
#   RATE_LIMIT_RPS     全局限速（默认 10）
#   MAX_EGRESS_BYTES   进程累计出口字节上限（默认 10 GiB，重启重置）
#   AUTH_TOKEN         设置后请求需带 X-Proxy-Token
#   PUBLIC_MODE        显式确认公网匿名开放；无任何访问控制时脚本会自动置 1
#   ANY_PROXY_IMAGE    覆盖镜像（默认 kurisu003/any-proxy:0.1.0）
#   ENV_FILE           指定 .env 路径（默认 ./.env）
#   SKIP_DNS_CHECK=1   跳过 DNS 指向检查
set -euo pipefail

IMAGE_DEFAULT="kurisu003/any-proxy:0.1.0"
IMAGE_FALLBACK="ghcr.io/kurisu994/any-proxy:0.1.0"

red()  { printf '\033[31m%s\033[0m\n' "$*"; }
grn()  { printf '\033[32m%s\033[0m\n' "$*"; }
ylw()  { printf '\033[33m%s\033[0m\n' "$*"; }
die()  { red "✗ $*"; exit 1; }

# ---- 读取 .env ----
# 已在 shell 环境中显式设置的变量优先，.env 只补空缺——与 docker compose 的
# 取值顺序一致，避免「命令行传了值却被 .env 悄悄覆盖」。
ENV_FILE="${ENV_FILE:-.env}"
if [ -f "$ENV_FILE" ]; then
  while IFS='=' read -r _key _val || [ -n "$_key" ]; do
    case "$_key" in ''|\#*) continue ;; esac
    _key="$(printf '%s' "$_key" | tr -d '[:space:]')"
    [ -z "$_key" ] && continue
    # 去掉值两端的成对引号
    _val="${_val%\"}"; _val="${_val#\"}"
    _val="${_val%\'}"; _val="${_val#\'}"
    # 环境里已有该变量则跳过（间接展开判断是否已定义）
    if [ -z "${!_key+x}" ]; then
      export "$_key=$_val"
    fi
  done < "$ENV_FILE"
  unset _key _val
fi

DOMAIN="${1:-${DOMAIN:-}}"
# 留空即不限制，与 any-proxy 自身的配置语义保持一致（空 allowlist = 不过滤）
ALLOW_TARGETS="${ALLOW_TARGETS:-}"
ALLOW_PORTS="${ALLOW_PORTS:-}"
RATE_LIMIT_RPS="${RATE_LIMIT_RPS:-10}"
MAX_EGRESS_BYTES="${MAX_EGRESS_BYTES:-10737418240}"
AUTH_TOKEN="${AUTH_TOKEN:-}"
PUBLIC_MODE="${PUBLIC_MODE:-}"
ANY_PROXY_IMAGE="${ANY_PROXY_IMAGE:-$IMAGE_DEFAULT}"

usage() {
  # 从文件头的注释块提取用法：跳过 shebang，遇到第一行非注释即停。
  # 不硬编码行号，改注释不会错位。
  awk 'NR==1 {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "$0"
}
case "${DOMAIN}" in
  -h|--help) usage; exit 0 ;;
  "")        usage; echo; red "✗ 缺少域名参数"; exit 1 ;;
esac

echo "==> 检查前置条件"

# ---- 依赖 ----
command -v docker >/dev/null 2>&1 || die "未安装 docker"
if docker compose version >/dev/null 2>&1; then
  COMPOSE="docker compose"
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE="docker-compose"
else
  die "未找到 docker compose（v2 插件或 docker-compose 均可）"
fi
docker info >/dev/null 2>&1 || die "docker daemon 未运行，或当前用户无权限（试试 sudo，或把用户加入 docker 组）"

# ---- 域名格式 ----
# 注意：以下中文提示里的变量一律写成 ${VAR} 显式界定边界。
# 老 bash（如 macOS 自带 3.2）会把紧跟其后的多字节字符首字节当作变量名的一部分，
# 在 `set -u` 下直接报 unbound variable。
case "$DOMAIN" in
  *.*) ;;
  *) die "「${DOMAIN}」不像一个域名。Caddy 需要真实域名才能签发证书。" ;;
esac
# 纯「数字与点」= IP 地址。Let's Encrypt 不为 IP 签发证书，提前拦下。
case "$DOMAIN" in
  *[!0-9.]*) ;;
  *) die "「${DOMAIN}」是 IP 地址。Let's Encrypt 不为 IP 签发证书，请用真实域名。" ;;
esac

# ---- DNS 指向 ----
# 证书签发失败最常见的原因就是域名没指向本机，提前拦下比事后看 ACME 日志强。
if [ "${SKIP_DNS_CHECK:-0}" != "1" ]; then
  public_ip="$(curl -fsS --max-time 10 https://api.ipify.org 2>/dev/null || true)"
  resolved="$(getent hosts "$DOMAIN" 2>/dev/null | awk '{print $1}' | head -1 || true)"
  if [ -z "$resolved" ]; then
    resolved="$(python3 -c "import socket,sys; print(socket.gethostbyname(sys.argv[1]))" "$DOMAIN" 2>/dev/null || true)"
  fi

  if [ -z "$resolved" ]; then
    ylw "⚠ 无法解析 $DOMAIN —— 若 DNS 尚未生效，证书签发会失败"
  elif [ -n "$public_ip" ] && [ "$resolved" != "$public_ip" ]; then
    ylw "⚠ ${DOMAIN} 解析到 ${resolved}，而本机公网 IP 是 ${public_ip}"
    ylw "  若不一致（且没走 CDN/代理），Let's Encrypt 的 HTTP-01 挑战会失败。"
    ylw "  确认无误可用 SKIP_DNS_CHECK=1 跳过此检查。"
  else
    grn "✓ DNS 指向正确（${DOMAIN} → ${resolved}）"
  fi
fi

# ---- 端口占用 ----
# Caddy 需要独占 80/443：80 用于 ACME HTTP-01 挑战与跳转，443 提供服务。
check_port() {
  local port="$1" holder=""
  if command -v ss >/dev/null 2>&1; then
    holder="$(ss -ltnp "sport = :$port" 2>/dev/null | tail -n +2 || true)"
  elif command -v lsof >/dev/null 2>&1; then
    holder="$(lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null | tail -n +2 || true)"
  fi
  if [ -n "$holder" ]; then
    # 已经是本脚本起的 caddy 则不算冲突（重复执行应当幂等）
    if ! echo "$holder" | grep -qi 'caddy\|docker'; then
      red "✗ 端口 $port 已被占用："
      echo "$holder"
      die "请先停掉占用进程（如 nginx/apache），Caddy 需要独占 80/443"
    fi
  fi
}
check_port 80
check_port 443

# ---- 访问控制姿态 ----
# any-proxy 有启动 gate：非 loopback 监听 + 零防护会拒绝启动，
# 以堵住「无意识地把开放代理推上公网」。这里替用户显式确认，而不是让容器
# 起不来又不知道为什么。
if [ -z "$ALLOW_TARGETS" ] && [ -z "$AUTH_TOKEN" ]; then
  if [ -z "$PUBLIC_MODE" ]; then
    PUBLIC_MODE=1
  fi
  echo
  ylw "⚠  未配置 ALLOW_TARGETS 与 AUTH_TOKEN —— 这是一个【公网开放代理】"
  ylw "   任何人拿到你的域名都能通过它访问任意公网地址，带宽算你的。"
  ylw "   已自动设置 PUBLIC_MODE=1 以通过启动 gate。"
  ylw "   仅靠 RATE_LIMIT_RPS=${RATE_LIMIT_RPS} 与 MAX_EGRESS_BYTES 兜底。"
  echo
  ylw "   想收紧，任选其一重新执行："
  ylw "     ALLOW_TARGETS=api.github.com,.your-api.com ./deploy.sh ${DOMAIN}"
  ylw "     AUTH_TOKEN=\$(openssl rand -hex 16) ./deploy.sh ${DOMAIN}"
  echo
fi

grn "✓ 前置检查通过"

# ---- 配置文件：优先复用仓库内的，缺失则生成 ----
if [ -f Caddyfile ]; then
  echo "==> 复用当前目录的 Caddyfile"
else
  echo "==> 生成 Caddyfile"
  cat > Caddyfile <<'CADDY'
# any-proxy 公网 HTTPS 反向代理配置（由 deploy.sh 生成）
#
# 默认使用匿名 ACME 账户（Let's Encrypt 允许，签发与续期均正常）。
# 想收到证书到期通知，取消下面三行注释并填入真实邮箱：
#
# {
# 	email you@example.com
# }

{$DOMAIN} {
	reverse_proxy any-proxy:8080 {
		# 立即透传每一个数据块，不做响应缓冲。
		# any-proxy 是流式代理，缓冲会破坏「边收边发」语义并放大内存占用。
		flush_interval -1

		# 不设置上游响应超时：any-proxy 自身用逐 frame 空闲超时兜底卡死，
		# 在此加总时长上限会重新引入「长传输被静默截断」的问题。
	}

	log {
		output stdout
		format json
	}
}
CADDY
fi

if [ -f docker-compose.caddy.yml ]; then
  echo "==> 复用当前目录的 docker-compose.caddy.yml"
else
  echo "==> 生成 docker-compose.caddy.yml"
  cat > docker-compose.caddy.yml <<'COMPOSE'
# 公网 HTTPS 部署：Caddy 自动 TLS + any-proxy（由 deploy.sh 生成）
services:
  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
      - "443:443/udp"
    environment:
      - DOMAIN=${DOMAIN:?请设置域名}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      # 证书与 ACME 账户必须持久化，否则每次重建都会重新申请，容易触发 Let's Encrypt 限流
      - caddy_data:/data
      - caddy_config:/config
    cap_drop:
      - ALL
    cap_add:
      - NET_BIND_SERVICE
    security_opt:
      - "no-new-privileges:true"
    depends_on:
      - any-proxy

  any-proxy:
    image: ${ANY_PROXY_IMAGE:-kurisu003/any-proxy:0.1.0}
    restart: unless-stopped
    # 不映射宿主端口：只在内部网络可达，公网入口只有 Caddy
    expose:
      - "8080"
    environment:
      - LISTEN_ADDR=0.0.0.0:8080
      - RUST_LOG=info
      # 留空 = 不限制，与 any-proxy 自身的配置语义一致
      - ALLOW_TARGETS=${ALLOW_TARGETS:-}
      - ALLOW_PORTS=${ALLOW_PORTS:-}
      - RATE_LIMIT_RPS=${RATE_LIMIT_RPS:-10}
      - MAX_EGRESS_BYTES=${MAX_EGRESS_BYTES:-10737418240}
      - AUTH_TOKEN=${AUTH_TOKEN:-}
      - PUBLIC_MODE=${PUBLIC_MODE:-}
    healthcheck:
      test: ["CMD", "any-proxy", "health-check"]
      interval: 30s
      timeout: 5s
      start_period: 3s
      retries: 3
    read_only: true
    cap_drop:
      - ALL
    security_opt:
      - "no-new-privileges:true"
    mem_limit: 256m
    pids_limit: 256

volumes:
  caddy_data:
  caddy_config:
COMPOSE
fi

# ---- 拉镜像（主源失败自动换备用源）----
echo "==> 拉取镜像 $ANY_PROXY_IMAGE"
if ! docker pull "$ANY_PROXY_IMAGE" 2>&1 | tail -2; then
  ylw "⚠ 主镜像源拉取失败，改用备用源 $IMAGE_FALLBACK"
  docker pull "$IMAGE_FALLBACK" 2>&1 | tail -2 || die "两个镜像源都拉不动，检查网络或先 docker login"
  ANY_PROXY_IMAGE="$IMAGE_FALLBACK"
fi

# ---- 启动 ----
echo "==> 启动服务"
export DOMAIN ALLOW_TARGETS ALLOW_PORTS RATE_LIMIT_RPS MAX_EGRESS_BYTES AUTH_TOKEN PUBLIC_MODE ANY_PROXY_IMAGE
$COMPOSE -f docker-compose.caddy.yml up -d

# ---- 等待 HTTPS 就绪 ----
# 首次启动需要向 Let's Encrypt 申请证书，通常几秒到几十秒。
echo "==> 等待证书签发与服务就绪（最多 120 秒）"
ok=""
for i in $(seq 1 60); do
  code="$(curl -fsS -o /dev/null -w '%{http_code}' --max-time 5 "https://$DOMAIN/healthz" 2>/dev/null || true)"
  if [ "$code" = "200" ]; then ok=1; break; fi
  printf '.'
  sleep 2
done
echo

if [ -z "$ok" ]; then
  red "✗ https://$DOMAIN/healthz 仍不可用"
  echo
  ylw "常见原因："
  ylw "  1. 域名未指向本机公网 IP（ACME HTTP-01 挑战失败）"
  ylw "  2. 云厂商安全组/防火墙未放行 80 与 443"
  ylw "  3. Let's Encrypt 触发限流（同域名短时间内重复申请）"
  echo
  echo "Caddy 最近日志："
  $COMPOSE -f docker-compose.caddy.yml logs --tail 30 caddy
  exit 1
fi

grn "✓ 部署成功"
echo
echo "  健康检查:  https://$DOMAIN/healthz"
curl -fsS "https://$DOMAIN/healthz"; echo
echo
echo "  冒烟测试:  curl -i https://$DOMAIN/https://api.github.com/zen"
echo
echo "当前配置："
echo "  放行目标: ${ALLOW_TARGETS:-<不限制，任意公网目标>}"
echo "  放行端口: ${ALLOW_PORTS:-<不限制 1-65535>}"
echo "  全局限速: ${RATE_LIMIT_RPS} rps"
echo "  出口预算: $((MAX_EGRESS_BYTES / 1024 / 1024)) MiB（进程重启重置）"
if [ -z "$ALLOW_TARGETS" ] && [ -z "$AUTH_TOKEN" ]; then
  ylw "  访问控制: 无 —— 这是公网开放代理，请确认你接受带宽与滥用风险"
fi
if [ -n "$AUTH_TOKEN" ]; then
  ylw "已启用 AUTH_TOKEN，请求需带 header: X-Proxy-Token: <你的 token>"
fi
echo
echo "放行更多目标后重新执行本脚本即可，例如："
echo "  ALLOW_TARGETS=api.github.com,.your-api.com ./deploy.sh $DOMAIN"
echo
echo "常用运维命令："
echo "  查看日志:  $COMPOSE -f docker-compose.caddy.yml logs -f"
echo "  停止:      $COMPOSE -f docker-compose.caddy.yml down"
echo "  升级镜像:  ANY_PROXY_IMAGE=kurisu003/any-proxy:<新版本> ./deploy.sh $DOMAIN"
