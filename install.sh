#!/bin/bash
set -e

# ============================================================
#  proxy_ssh 一键部署脚本
#  - 交叉编译 Linux 二进制
#  - 部署 proxy-server 到公网服务器 + systemd 开机启动
#  - 部署 proxy-client 到本地 Linux + systemd 开机启动
# ============================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# ---- 配置 ----
SERVER_HOST="104.244.95.160"
SERVER_USER="root"
SERVER_CONTROL_PORT=7000
SERVER_PROXY_PORT=7001
CLIENT_LOCAL_TARGET="127.0.0.1:22"
HEARTBEAT_INTERVAL=10
HEARTBEAT_TIMEOUT=30
MAX_RECONNECT_DELAY=60
INSTALL_PATH="/usr/local/bin"
RUST_TARGET="x86_64-unknown-linux-musl"

# ---- 交互式输入 ----
echo "========================================"
echo "  proxy_ssh 一键部署"
echo "========================================"
echo ""

read -sp "请输入共享密钥 (用于 server/client 认证): " SECRET
echo ""
if [ -z "$SECRET" ]; then
    error "密钥不能为空"
fi

read -sp "请再次确认密钥: " SECRET_CONFIRM
echo ""
if [ "$SECRET" != "$SECRET_CONFIRM" ]; then
    error "两次输入的密钥不一致"
fi

echo ""
read -p "公网服务器地址 [${SERVER_HOST}]: " input
SERVER_HOST="${input:-$SERVER_HOST}"

read -p "公网服务器 SSH 用户 [${SERVER_USER}]: " input
SERVER_USER="${input:-$SERVER_USER}"

read -p "代理端口 (外部通过此端口连接) [${SERVER_PROXY_PORT}]: " input
SERVER_PROXY_PORT="${input:-$SERVER_PROXY_PORT}"

read -p "本地转发目标 [${CLIENT_LOCAL_TARGET}]: " input
CLIENT_LOCAL_TARGET="${input:-$CLIENT_LOCAL_TARGET}"

echo ""
info "配置确认:"
echo "  服务器: ${SERVER_USER}@${SERVER_HOST}"
echo "  控制端口: ${SERVER_CONTROL_PORT}"
echo "  代理端口: ${SERVER_PROXY_PORT}"
echo "  本地转发: ${CLIENT_LOCAL_TARGET}"
echo ""
read -p "确认部署? [y/N] " confirm
if [[ ! "$confirm" =~ ^[Yy]$ ]]; then
    echo "已取消"
    exit 0
fi

# ---- 第一步: 编译 ----
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

info "检查 Rust 交叉编译工具链..."
if ! rustup target list --installed | grep -q "$RUST_TARGET"; then
    info "安装编译目标 ${RUST_TARGET}..."
    rustup target add "$RUST_TARGET"
fi

info "编译 release 版本 (target: ${RUST_TARGET})..."
cargo build --release --target "$RUST_TARGET"

SERVER_BIN="target/${RUST_TARGET}/release/proxy-server"
CLIENT_BIN="target/${RUST_TARGET}/release/proxy-client"

if [ ! -f "$SERVER_BIN" ] || [ ! -f "$CLIENT_BIN" ]; then
    error "编译产物不存在"
fi

info "编译完成"
ls -lh "$SERVER_BIN" "$CLIENT_BIN"

# ---- 第二步: 部署 Server ----
echo ""
info "========== 部署 Server 到 ${SERVER_USER}@${SERVER_HOST} =========="

info "上传 proxy-server..."
scp "$SERVER_BIN" "${SERVER_USER}@${SERVER_HOST}:${INSTALL_PATH}/proxy-server"

info "配置 systemd 服务..."
ssh "${SERVER_USER}@${SERVER_HOST}" bash -s <<REMOTE_SERVER
set -e

chmod +x ${INSTALL_PATH}/proxy-server

cat > /etc/systemd/system/proxy-server.service <<'UNIT'
[Unit]
Description=Proxy SSH Server (reverse tunnel relay)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_PATH}/proxy-server --control-port ${SERVER_CONTROL_PORT} --proxy-port ${SERVER_PROXY_PORT} --secret "${SECRET}" --heartbeat-timeout ${HEARTBEAT_TIMEOUT}
Restart=always
RestartSec=3
Environment=RUST_LOG=info
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable proxy-server
systemctl restart proxy-server
sleep 1
systemctl is-active proxy-server && echo "proxy-server 运行正常" || echo "proxy-server 启动失败"
REMOTE_SERVER

info "Server 部署完成"

# ---- 第三步: 部署 Client (本地) ----
echo ""
info "========== 部署 Client 到本机 =========="

# 检测本机是否是 Linux
if [ "$(uname)" != "Linux" ]; then
    warn "当前系统不是 Linux ($(uname))，Client 需要部署到家里的 Linux 主机"
    echo ""
    info "请手动部署 Client:"
    echo ""
    echo "  1. 复制 binary 到家里 Linux 主机:"
    echo "     scp ${CLIENT_BIN} user@HOME_HOST:${INSTALL_PATH}/proxy-client"
    echo ""
    echo "  2. 在家里 Linux 主机上执行:"
    echo "     chmod +x ${INSTALL_PATH}/proxy-client"
    echo ""
    echo "  3. 创建 systemd 服务文件 /etc/systemd/system/proxy-client.service:"
    cat <<EOF

[Unit]
Description=Proxy SSH Client (reverse tunnel)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_PATH}/proxy-client --server ${SERVER_HOST} --control-port ${SERVER_CONTROL_PORT} --local-target ${CLIENT_LOCAL_TARGET} --secret "${SECRET}" --heartbeat-interval ${HEARTBEAT_INTERVAL} --max-reconnect-delay ${MAX_RECONNECT_DELAY}
Restart=always
RestartSec=3
Environment=RUST_LOG=info
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF
    echo ""
    echo "  4. 启用并启动:"
    echo "     sudo systemctl daemon-reload"
    echo "     sudo systemctl enable --now proxy-client"
    echo ""

    # 提供可选的远程部署
    echo ""
    read -p "是否要通过 SSH 直接部署到家里 Linux 主机? [y/N] " deploy_remote
    if [[ "$deploy_remote" =~ ^[Yy]$ ]]; then
        read -p "家里 Linux 主机 SSH 地址 (user@host): " HOME_SSH
        if [ -z "$HOME_SSH" ]; then
            error "SSH 地址不能为空"
        fi

        info "上传 proxy-client 到 ${HOME_SSH}..."
        scp "$CLIENT_BIN" "${HOME_SSH}:${INSTALL_PATH}/proxy-client"

        info "配置 systemd 服务..."
        ssh "$HOME_SSH" bash -s <<REMOTE_CLIENT
set -e

chmod +x ${INSTALL_PATH}/proxy-client

cat > /etc/systemd/system/proxy-client.service <<'UNIT'
[Unit]
Description=Proxy SSH Client (reverse tunnel)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_PATH}/proxy-client --server ${SERVER_HOST} --control-port ${SERVER_CONTROL_PORT} --local-target ${CLIENT_LOCAL_TARGET} --secret "${SECRET}" --heartbeat-interval ${HEARTBEAT_INTERVAL} --max-reconnect-delay ${MAX_RECONNECT_DELAY}
Restart=always
RestartSec=3
Environment=RUST_LOG=info
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable proxy-client
systemctl restart proxy-client
sleep 1
systemctl is-active proxy-client && echo "proxy-client 运行正常" || echo "proxy-client 启动失败"
REMOTE_CLIENT

        info "Client 远程部署完成"
    fi
else
    # 本机就是 Linux，直接部署
    info "安装 proxy-client 到 ${INSTALL_PATH}..."
    sudo cp "$CLIENT_BIN" "${INSTALL_PATH}/proxy-client"
    sudo chmod +x "${INSTALL_PATH}/proxy-client"

    info "配置 systemd 服务..."
    sudo tee /etc/systemd/system/proxy-client.service > /dev/null <<UNIT
[Unit]
Description=Proxy SSH Client (reverse tunnel)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=${INSTALL_PATH}/proxy-client --server ${SERVER_HOST} --control-port ${SERVER_CONTROL_PORT} --local-target ${CLIENT_LOCAL_TARGET} --secret "${SECRET}" --heartbeat-interval ${HEARTBEAT_INTERVAL} --max-reconnect-delay ${MAX_RECONNECT_DELAY}
Restart=always
RestartSec=3
Environment=RUST_LOG=info
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT

    sudo systemctl daemon-reload
    sudo systemctl enable proxy-client
    sudo systemctl restart proxy-client
    sleep 1

    if systemctl is-active --quiet proxy-client; then
        info "proxy-client 运行正常"
    else
        warn "proxy-client 启动可能有问题，检查: journalctl -u proxy-client -f"
    fi

    info "Client 部署完成"
fi

# ---- 完成 ----
echo ""
echo "========================================"
echo -e "${GREEN}  部署完成!${NC}"
echo "========================================"
echo ""
echo "  从任意网络连接家里主机:"
echo "    ssh user@${SERVER_HOST} -p ${SERVER_PROXY_PORT}"
echo ""
echo "  查看日志:"
echo "    Server: ssh ${SERVER_USER}@${SERVER_HOST} journalctl -u proxy-server -f"
echo "    Client: journalctl -u proxy-client -f"
echo ""
echo "  管理服务:"
echo "    systemctl {start|stop|restart|status} proxy-server"
echo "    systemctl {start|stop|restart|status} proxy-client"
echo ""
