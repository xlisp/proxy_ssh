# proxy_ssh

高性能 Rust 反向代理隧道工具。通过公网服务器中转，让你从任意网络访问 NAT/防火墙后面的家庭主机。

## 架构

```
任意网络的用户                       公网服务器                        家里 Linux 主机
 (SSH 客户端)                    (104.244.95.160)                  (NAT/防火墙后)
      │                                │                                │
      │   ssh user@server -p 7001      │                                │
      ├───────────────────────────────►│ :7001 (proxy port)             │
      │                                │                                │
      │                                │ :7000 (control port) ◄─────────┤ 反向连接 (持久)
      │                                │        控制通道                 │
      │                                │ ── NewConnection ─────────────►│
      │                                │                                │ connect 127.0.0.1:22
      │◄──────── 双向数据转发 ──────────►│◄──────── 双向数据转发 ────────►│
      │                                │                                │
```

**核心流程：**

1. 家里的 `proxy-client` 主动连接公网服务器的控制端口 (7000)，建立持久控制通道
2. 外部用户连接公网服务器的代理端口 (7001)
3. 服务器通过控制通道通知 client 有新连接
4. Client 在本地连接目标服务 (如 SSH 22 端口)，开始双向转发

## 特性

- **心跳保活** — Client 每 10s 发送 Ping，Server 回复 Pong；超时 30s 无心跳自动判定断连
- **死连接检测** — Client 连续 3 倍心跳间隔未收到 Pong，主动断开触发重连
- **断线自动重连** — 指数退避策略 (1s → 2s → 4s → ... → 60s 上限)，成功连接后重置
- **共享密钥认证** — 防止未授权客户端连接控制通道
- **多会话复用** — 单条控制连接承载多个并发会话，每个会话独立 session_id
- **高性能异步 I/O** — 基于 tokio，TCP_NODELAY，32KB 读缓冲区
- **轻量二进制协议** — 9 字节帧头 (1B type + 4B session_id + 4B length)，零序列化开销

## 构建

```bash
cargo build --release
```

产出两个二进制：
- `target/release/proxy-server` — 部署在公网服务器
- `target/release/proxy-client` — 部署在家里 Linux 主机

### 交叉编译 (macOS 编译 Linux 目标)

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## 部署

### 1. 公网服务器

```bash
# 复制 binary 到服务器
scp target/release/proxy-server root@104.244.95.160:/usr/local/bin/

# 运行
proxy-server --secret "your-strong-secret"
```

完整参数：

```
Options:
    --control-port <PORT>           控制端口 [默认: 7000]
    --proxy-port <PORT>             代理端口 [默认: 7001]
    --secret <SECRET>               共享密钥 [默认: change-me-secret]
    --heartbeat-timeout <SECONDS>   心跳超时 [默认: 30]
```

### 2. 家里 Linux 主机

```bash
# 运行 (转发到本地 SSH)
proxy-client --server 104.244.95.160 --secret "your-strong-secret"
```

完整参数：

```
Options:
    --server <ADDR>                 服务器地址 [默认: 104.244.95.160]
    --control-port <PORT>           控制端口 [默认: 7000]
    --local-target <ADDR:PORT>      本地转发目标 [默认: 127.0.0.1:22]
    --secret <SECRET>               共享密钥 [默认: change-me-secret]
    --heartbeat-interval <SECONDS>  心跳间隔 [默认: 10]
    --max-reconnect-delay <SECONDS> 最大重连延迟 [默认: 60]
```

### 3. 从任意网络连接

```bash
ssh user@104.244.95.160 -p 7001
```

通过公网服务器 7001 端口即可 SSH 到家里主机。

## 转发其他服务

`--local-target` 支持任意 TCP 服务：

```bash
# 转发 Web 服务
proxy-client --server 104.244.95.160 --secret "xxx" --local-target 127.0.0.1:8080

# 转发数据库
proxy-client --server 104.244.95.160 --secret "xxx" --local-target 127.0.0.1:5432
```

## Systemd 服务 (可选)

### Server (/etc/systemd/system/proxy-server.service)

```ini
[Unit]
Description=Proxy SSH Server
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/proxy-server --secret "your-strong-secret"
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

### Client (/etc/systemd/system/proxy-client.service)

```ini
[Unit]
Description=Proxy SSH Client
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/proxy-client --server 104.244.95.160 --secret "your-strong-secret"
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

```bash
systemctl enable --now proxy-server   # 服务器端
systemctl enable --now proxy-client   # 客户端
```

## 协议

自定义轻量二进制协议，帧格式：

```
+--------+------------+--------+---------+
| type   | session_id | length | payload |
| 1 byte | 4 bytes    | 4 bytes| N bytes |
+--------+------------+--------+---------+
```

帧类型：

| Type | 值 | 方向 | 说明 |
|------|---|------|------|
| Ping | 1 | Client → Server | 心跳探测 |
| Pong | 2 | Server → Client | 心跳回复 |
| NewConnection | 3 | Server → Client | 新外部连接通知 |
| Data | 4 | 双向 | 数据传输 |
| Close | 5 | 双向 | 关闭会话 |
| Auth | 6 | Client → Server | 认证请求 |
| AuthOk | 7 | Server → Client | 认证成功 |

## 日志

通过 `RUST_LOG` 环境变量控制日志级别：

```bash
RUST_LOG=debug proxy-client --server 104.244.95.160 --secret "xxx"
RUST_LOG=info proxy-server --secret "xxx"      # 默认级别
RUST_LOG=warn proxy-server --secret "xxx"      # 只显示警告
```

## License

MIT
