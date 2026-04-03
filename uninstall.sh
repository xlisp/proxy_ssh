#!/bin/bash
set -e

# ============================================================
#  proxy_ssh 一键卸载脚本
#  - 停止并删除 systemd 服务
#  - 删除二进制文件
#  - 支持卸载服务端、本地端、或远程客户端
# ============================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

SERVER_HOST="104.244.95.160"
SERVER_USER="root"
INSTALL_PATH="/usr/local/bin"

echo "========================================"
echo "  proxy_ssh 一键卸载"
echo "========================================"
echo ""
echo "  1) 卸载服务端 (公网服务器)"
echo "  2) 卸载本地客户端"
echo "  3) 卸载远程客户端 (通过 SSH)"
echo "  4) 全部卸载 (服务端 + 本地客户端)"
echo ""
read -p "请选择 [1-4]: " choice

# ---- 卸载服务端 ----
uninstall_server() {
    read -p "公网服务器地址 [${SERVER_HOST}]: " input
    SERVER_HOST="${input:-$SERVER_HOST}"
    read -p "SSH 用户 [${SERVER_USER}]: " input
    SERVER_USER="${input:-$SERVER_USER}"

    info "卸载 ${SERVER_USER}@${SERVER_HOST} 上的 proxy-server..."

    ssh "${SERVER_USER}@${SERVER_HOST}" bash -s <<'REMOTE'
set -e
echo "[INFO] 停止服务..."
systemctl stop proxy-server 2>/dev/null || true
systemctl disable proxy-server 2>/dev/null || true
echo "[INFO] 删除 systemd 配置..."
rm -f /etc/systemd/system/proxy-server.service
systemctl daemon-reload
echo "[INFO] 删除二进制..."
rm -f /usr/local/bin/proxy-server
echo "[INFO] 服务端卸载完成"
REMOTE

    info "服务端卸载完成"
}

# ---- 卸载本地客户端 ----
uninstall_local_client() {
    if [ "$(uname)" != "Linux" ]; then
        warn "当前系统不是 Linux，本地可能没有安装 systemd 服务"
        read -p "仍然继续? [y/N] " confirm
        if [[ ! "$confirm" =~ ^[Yy]$ ]]; then
            return
        fi
    fi

    info "卸载本地 proxy-client..."

    sudo systemctl stop proxy-client 2>/dev/null || true
    sudo systemctl disable proxy-client 2>/dev/null || true
    sudo rm -f /etc/systemd/system/proxy-client.service
    sudo systemctl daemon-reload
    sudo rm -f "${INSTALL_PATH}/proxy-client"

    info "本地客户端卸载完成"
}

# ---- 卸载远程客户端 ----
uninstall_remote_client() {
    read -p "家里 Linux 主机 SSH 地址 (user@host): " HOME_SSH
    if [ -z "$HOME_SSH" ]; then
        error "SSH 地址不能为空"
    fi

    info "卸载 ${HOME_SSH} 上的 proxy-client..."

    ssh "$HOME_SSH" bash -s <<'REMOTE'
set -e
echo "[INFO] 停止服务..."
systemctl stop proxy-client 2>/dev/null || true
systemctl disable proxy-client 2>/dev/null || true
echo "[INFO] 删除 systemd 配置..."
rm -f /etc/systemd/system/proxy-client.service
systemctl daemon-reload
echo "[INFO] 删除二进制..."
rm -f /usr/local/bin/proxy-client
echo "[INFO] 客户端卸载完成"
REMOTE

    info "远程客户端卸载完成"
}

case "$choice" in
    1)
        uninstall_server
        ;;
    2)
        uninstall_local_client
        ;;
    3)
        uninstall_remote_client
        ;;
    4)
        uninstall_server
        echo ""
        uninstall_local_client
        ;;
    *)
        error "无效选择"
        ;;
esac

echo ""
echo "========================================"
echo -e "${GREEN}  卸载完成${NC}"
echo "========================================"
