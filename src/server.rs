/// proxy-server: runs on public server (e.g. 104.244.95.160)
///
/// Listens on two ports:
///   - Control port (default 7000): for the home client to connect
///   - Proxy port (default 7001): for external users to connect (e.g. SSH)
///
/// Flow:
///   1. Home client connects to control port, authenticates
///   2. External user connects to proxy port
///   3. Server assigns session_id, sends NewConnection to home client
///   4. Home client opens local connection to target service, relays data
///   5. Heartbeat ping/pong keeps control channel alive
mod protocol;

use protocol::*;

use clap::Parser;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "proxy-server", about = "Reverse proxy server on public node")]
struct Args {
    /// Control port for home client
    #[arg(long, default_value_t = 7000)]
    control_port: u16,

    /// Proxy port for external connections
    #[arg(long, default_value_t = 7001)]
    proxy_port: u16,

    /// Shared secret for authentication
    #[arg(long, default_value = "change-me-secret")]
    secret: String,

    /// Heartbeat timeout in seconds
    #[arg(long, default_value_t = 30)]
    heartbeat_timeout: u64,
}

/// Per-session state: a channel to send data back to the external client
struct Session {
    tx: mpsc::Sender<Vec<u8>>,
}

/// Shared server state
struct ServerState {
    /// Active sessions: session_id -> Session
    sessions: RwLock<HashMap<u32, Session>>,
    /// Channel to send frames to home client via the control connection
    control_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Next session ID
    next_session_id: Mutex<u32>,
    /// Last heartbeat from home client
    last_heartbeat: Mutex<Instant>,
    /// Secret (stored for potential re-auth)
    #[allow(dead_code)]
    secret: String,
}

impl ServerState {
    fn new(secret: String) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            control_tx: Mutex::new(None),
            next_session_id: Mutex::new(1),
            last_heartbeat: Mutex::new(Instant::now()),
            secret,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();
    let state = Arc::new(ServerState::new(args.secret.clone()));

    let heartbeat_timeout = args.heartbeat_timeout;

    // Spawn heartbeat checker
    let state_hb = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let last = *state_hb.last_heartbeat.lock().await;
            if last.elapsed() > Duration::from_secs(heartbeat_timeout) {
                let has_control = state_hb.control_tx.lock().await.is_some();
                if has_control {
                    warn!(
                        "Heartbeat timeout ({heartbeat_timeout}s), dropping control connection"
                    );
                    *state_hb.control_tx.lock().await = None;
                    // Close all sessions
                    state_hb.sessions.write().await.clear();
                }
            }
        }
    });

    // Listen for control connections
    let control_listener = TcpListener::bind(format!("0.0.0.0:{}", args.control_port)).await?;
    info!("Control port listening on 0.0.0.0:{}", args.control_port);

    // Listen for proxy connections
    let proxy_listener = TcpListener::bind(format!("0.0.0.0:{}", args.proxy_port)).await?;
    info!("Proxy port listening on 0.0.0.0:{}", args.proxy_port);

    let state_ctrl = state.clone();
    let secret = args.secret.clone();
    // Accept control connections
    tokio::spawn(async move {
        loop {
            match control_listener.accept().await {
                Ok((stream, addr)) => {
                    info!("Control connection from {addr}");
                    let state = state_ctrl.clone();
                    let secret = secret.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_control(stream, state, &secret).await {
                            error!("Control connection error: {e}");
                        }
                    });
                }
                Err(e) => error!("Control accept error: {e}"),
            }
        }
    });

    // Accept proxy connections
    let state_proxy = state.clone();
    loop {
        match proxy_listener.accept().await {
            Ok((stream, addr)) => {
                info!("Proxy connection from {addr}");
                let state = state_proxy.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_proxy(stream, state).await {
                        error!("Proxy session error: {e}");
                    }
                });
            }
            Err(e) => error!("Proxy accept error: {e}"),
        }
    }
}

/// Handle the control connection from the home client.
async fn handle_control(
    mut stream: TcpStream,
    state: Arc<ServerState>,
    secret: &str,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;

    // Wait for Auth frame
    let (ft, _, payload) = read_frame(&mut stream).await?;
    if ft != FrameType::Auth {
        warn!("Expected Auth frame, got {ft:?}");
        return Ok(());
    }
    let received_secret = String::from_utf8_lossy(&payload);
    if received_secret != secret {
        warn!("Auth failed: wrong secret");
        write_frame(&mut stream, FrameType::Close, 0, b"auth failed").await?;
        return Ok(());
    }
    write_frame(&mut stream, FrameType::AuthOk, 0, b"").await?;
    info!("Home client authenticated");

    *state.last_heartbeat.lock().await = Instant::now();

    // Split the control connection
    let (mut reader, mut writer) = stream.into_split();

    // Channel for sending frames to the home client
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1024);
    *state.control_tx.lock().await = Some(tx);

    // Writer task: forward queued frames to home client
    let state_w = state.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if writer.write_all(&data).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
        *state_w.control_tx.lock().await = None;
        info!("Control writer stopped");
    });

    // Reader task: read frames from home client
    let state_r = state.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok((FrameType::Ping, _, _)) => {
                    *state_r.last_heartbeat.lock().await = Instant::now();
                    // Send pong via control_tx
                    let pong = encode_frame(FrameType::Pong, 0, b"");
                    if let Some(tx) = state_r.control_tx.lock().await.as_ref() {
                        let _ = tx.send(pong).await;
                    }
                }
                Ok((FrameType::Data, session_id, payload)) => {
                    let sessions = state_r.sessions.read().await;
                    if let Some(session) = sessions.get(&session_id) {
                        let _ = session.tx.send(payload).await;
                    }
                }
                Ok((FrameType::Close, session_id, _)) => {
                    state_r.sessions.write().await.remove(&session_id);
                }
                Ok((ft, _, _)) => {
                    warn!("Unexpected frame from home client: {ft:?}");
                }
                Err(e) => {
                    error!("Control read error: {e}");
                    break;
                }
            }
        }
        *state_r.control_tx.lock().await = None;
        state_r.sessions.write().await.clear();
        info!("Control reader stopped");
    });

    tokio::select! {
        _ = writer_task => {},
        _ = reader_task => {},
    }

    info!("Control connection closed");
    Ok(())
}

/// Handle an external proxy connection.
async fn handle_proxy(stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    stream.set_nodelay(true)?;

    // Check if home client is connected
    let control_tx = {
        let guard = state.control_tx.lock().await;
        guard.clone()
    };
    let control_tx = match control_tx {
        Some(tx) => tx,
        None => {
            warn!("No home client connected, rejecting proxy connection");
            return Ok(());
        }
    };

    // Assign session ID
    let session_id = {
        let mut id = state.next_session_id.lock().await;
        let sid = *id;
        *id = id.wrapping_add(1);
        if *id == 0 {
            *id = 1;
        }
        sid
    };

    // Create session channel
    let (session_tx, mut session_rx) = mpsc::channel::<Vec<u8>>(256);
    state
        .sessions
        .write()
        .await
        .insert(session_id, Session { tx: session_tx });

    // Notify home client of new connection
    let new_conn_frame = encode_frame(FrameType::NewConnection, session_id, b"");
    if control_tx.send(new_conn_frame).await.is_err() {
        state.sessions.write().await.remove(&session_id);
        return Ok(());
    }

    info!("Session {session_id} created");

    let (mut ext_reader, mut ext_writer) = stream.into_split();

    // External -> Home: read from external client, send Data frames via control
    let control_tx2 = control_tx.clone();
    let state2 = state.clone();
    let ext_to_home = tokio::spawn(async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match ext_reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let frame = encode_frame(FrameType::Data, session_id, &buf[..n]);
                    if control_tx2.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Send close
        let close = encode_frame(FrameType::Close, session_id, b"");
        let _ = control_tx2.send(close).await;
        state2.sessions.write().await.remove(&session_id);
    });

    // Home -> External: read from session channel, write to external client
    let home_to_ext = tokio::spawn(async move {
        while let Some(data) = session_rx.recv().await {
            if ext_writer.write_all(&data).await.is_err() {
                break;
            }
            if ext_writer.flush().await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = ext_to_home => {},
        _ = home_to_ext => {},
    }

    info!("Session {session_id} closed");
    Ok(())
}
