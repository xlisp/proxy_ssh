# proxy_ssh

高性能 Rust 反向代理隧道工具。通过公网服务器中转，让你从任意网络访问 NAT/防火墙后面的家庭主机。

## 架构 (v2 — 独立数据连接，零拷贝转发)

```
任意网络的用户                         公网服务器                          家里 Linux 主机
 (SSH 客户端)                      (104.244.95.160)                    (NAT/防火墙后)
      │                                  │                                  │
      │   ssh user@server -p 7001        │                                  │
      ├─────────────────────────────────►│ :7001 (proxy port)               │
      │                                  │                                  │
      │                                  │ :7000 (control) ◄────────────────┤ 持久控制通道
      │                                  │  NewConnection(sid) ────────────►│  (仅心跳+信令)
      │                                  │                                  │
      │                                  │ :7002 (data) ◄──────────────────┤ 新建数据连接
      │                                  │  DataConnect(sid)                │  + 连本地 SSH
      │                                  │                                  │
      │◄═══════ 零拷贝双向转发 (splice) ═══►│◄═══════ 零拷贝双向转发 ═══════►│
      │           tokio::io::copy         │          tokio::io::copy         │
```

**核心流程：**

1. `proxy-client` 连接服务器控制端口 (7000)，认证后保持心跳
2. 外部用户连接代理端口 (7001)
3. Server 通过控制通道发送 `NewConnection(session_id)`
4. Client 同时连接本地服务 (22) 和服务器数据端口 (7002)，发送 `DataConnect(session_id)`
5. Server 将外部连接与数据连接桥接 — **`tokio::io::copy` 零拷贝直通**，数据路径无帧编解码

## 为什么快

| 优化点 | 说明 |
|--------|------|
| 独立数据连接 | 每个会话独立 TCP，不走控制通道复用，消除帧编解码和锁竞争 |
| splice 零拷贝 | `tokio::io::copy` 在 Linux 上使用 `splice()`，数据不经过用户态 |
| TCP_NODELAY | 所有连接禁用 Nagle，交互式零延迟 |
| 256KB socket buffer | 增大收发缓冲区，提升吞吐 |
| BufReader/BufWriter | 控制通道使用缓冲 I/O，减少 syscall |
| 无锁数据路径 | 数据转发路径完全无 Mutex、无 channel，直接 copy |

## 特性

- **心跳保活** — 每 10s Ping/Pong，30s 超时断连判定
- **死连接检测** — 3 倍心跳间隔无 Pong 主动断开
- **断线自动重连** — 指数退避 (1s → 2s → ... → 60s)
- **共享密钥认证**
- **零拷贝数据转发** — splice on Linux

## 构建

```bash
cargo build --release
```

### 交叉编译 (macOS → Linux)

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## 一键部署

```bash
./install.sh
```

交互式输入密钥，自动编译、上传、配置 systemd 开机启动。

## 手动部署

### 1. 公网服务器

```bash
scp target/release/proxy-server root@104.244.95.160:/usr/local/bin/
proxy-server --secret "your-strong-secret"
```

参数：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--control-port` | 7000 | 控制端口 |
| `--proxy-port` | 7001 | 外部连接端口 |
| `--data-port` | 7002 | 数据通道端口 |
| `--secret` | change-me-secret | 认证密钥 |
| `--heartbeat-timeout` | 30 | 心跳超时 (秒) |

### 2. 家里 Linux 主机

```bash
proxy-client --server 104.244.95.160 --secret "your-strong-secret"
```

参数：

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--server` | 104.244.95.160 | 服务器地址 |
| `--control-port` | 7000 | 控制端口 |
| `--data-port` | 7002 | 数据通道端口 |
| `--local-target` | 127.0.0.1:22 | 本地转发目标 |
| `--secret` | change-me-secret | 认证密钥 |
| `--heartbeat-interval` | 10 | 心跳间隔 (秒) |
| `--max-reconnect-delay` | 60 | 最大重连延迟 (秒) |

### 3. 连接

```bash
ssh user@104.244.95.160 -p 7001
```

## 转发其他服务

```bash
# Web
proxy-client --server 104.244.95.160 --secret "xxx" --local-target 127.0.0.1:8080

# 数据库
proxy-client --server 104.244.95.160 --secret "xxx" --local-target 127.0.0.1:5432
```

## 协议

控制通道帧格式 (仅心跳和信令)：

```
+--------+------------+--------+---------+
| type   | session_id | length | payload |
| 1 byte | 4 bytes    | 4 bytes| N bytes |
+--------+------------+--------+---------+
```

| Type | 值 | 方向 | 说明 |
|------|---|------|------|
| Ping | 1 | Client → Server | 心跳 |
| Pong | 2 | Server → Client | 心跳回复 |
| NewConnection | 3 | Server → Client | 新连接通知 |
| Close | 5 | 双向 | 关闭会话 |
| Auth | 6 | Client → Server | 认证 |
| AuthOk | 7 | Server → Client | 认证成功 |
| DataConnect | 8 | Client → Server | 数据通道握手 |

数据通道：DataConnect 握手后为**裸 TCP 流**，无任何帧开销。

## 日志

```bash
RUST_LOG=debug proxy-client --server 104.244.95.160 --secret "xxx"
RUST_LOG=info  proxy-server --secret "xxx"
```

## License

MIT
