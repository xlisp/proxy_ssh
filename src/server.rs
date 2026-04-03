/// proxy-server: high-performance reverse proxy relay on public server
///
/// Architecture (v2 - separate data connections for zero-copy):
///   - Control port (7000): heartbeat + session signaling only
///   - Data port (7002): per-session dedicated TCP connections from home client
///   - Proxy port (7001): external users connect here
///
/// Flow:
///   1. Home client connects control port, authenticates
///   2. External user connects proxy port
///   3. Server sends NewConnection(session_id) via control channel
///   4. Home client opens NEW TCP to data port, sends DataConnect(session_id)
///   5. Server bridges external<->data connections with tokio::io::copy_bidirectional (splice)
///   6. Zero framing overhead on data path — raw TCP relay
mod protocol;

use protocol::*;

use clap::Parser;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "proxy-server", about = "High-performance reverse proxy server")]
struct Args {
    #[arg(long, default_value_t = 7000)]
    control_port: u16,
    #[arg(long, default_value_t = 7001)]
    proxy_port: u16,
    #[arg(long, default_value_t = 7002)]
    data_port: u16,
    #[arg(long, default_value = "change-me-secret")]
    secret: String,
    #[arg(long, default_value_t = 30)]
    heartbeat_timeout: u64,
}

struct ServerState {
    /// Control channel writer (send frames to home client)
    control_tx: Mutex<Option<mpsc::Sender<(FrameType, u32, Vec<u8>)>>>,
    /// Pending sessions waiting for data connection
    pending: RwLock<HashMap<u32, oneshot::Sender<TcpStream>>>,
    next_session_id: Mutex<u32>,
    last_heartbeat: Mutex<Instant>,
    #[allow(dead_code)]
    secret: String,
}

impl ServerState {
    fn new(secret: String) -> Self {
        Self {
            control_tx: Mutex::new(None),
            pending: RwLock::new(HashMap::new()),
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

    // Heartbeat watchdog
    let state_hb = state.clone();
    let hb_timeout = args.heartbeat_timeout;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let elapsed = state_hb.last_heartbeat.lock().await.elapsed();
            if elapsed > Duration::from_secs(hb_timeout) {
                if state_hb.control_tx.lock().await.is_some() {
                    warn!("Heartbeat timeout ({hb_timeout}s), dropping control");
                    *state_hb.control_tx.lock().await = None;
                    state_hb.pending.write().await.clear();
                }
            }
        }
    });

    let control_listener = TcpListener::bind(format!("0.0.0.0:{}", args.control_port)).await?;
    let proxy_listener = TcpListener::bind(format!("0.0.0.0:{}", args.proxy_port)).await?;
    let data_listener = TcpListener::bind(format!("0.0.0.0:{}", args.data_port)).await?;
    info!(
        "Listening: control={}, proxy={}, data={}",
        args.control_port, args.proxy_port, args.data_port
    );

    // Accept control connections
    let state_c = state.clone();
    let secret = args.secret.clone();
    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = control_listener.accept().await {
                info!("Control from {addr}");
                let st = state_c.clone();
                let sec = secret.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_control(stream, st, &sec).await {
                        error!("Control error: {e}");
                    }
                });
            }
        }
    });

    // Accept data connections (from home client, per-session)
    let state_d = state.clone();
    tokio::spawn(async move {
        loop {
            if let Ok((stream, addr)) = data_listener.accept().await {
                let st = state_d.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_data_connect(stream, st).await {
                        error!("Data connect from {addr} error: {e}");
                    }
                });
            }
        }
    });

    // Accept proxy connections (from external users)
    loop {
        if let Ok((stream, addr)) = proxy_listener.accept().await {
            info!("Proxy from {addr}");
            let st = state.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_proxy(stream, st).await {
                    error!("Proxy error: {e}");
                }
            });
        }
    }
}

/// Control channel: auth + heartbeat + session signaling (no data!)
async fn handle_control(
    stream: TcpStream,
    state: Arc<ServerState>,
    secret: &str,
) -> std::io::Result<()> {
    tune_socket(&stream);
    let (rd, wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut writer = BufWriter::new(wr);

    // Auth
    let (ft, _, payload) = read_frame(&mut reader).await?;
    if ft != FrameType::Auth || String::from_utf8_lossy(&payload) != secret {
        warn!("Auth failed");
        write_frame(&mut writer, FrameType::Close, 0, b"auth failed").await?;
        return Ok(());
    }
    write_frame(&mut writer, FrameType::AuthOk, 0, b"").await?;
    info!("Home client authenticated");
    *state.last_heartbeat.lock().await = Instant::now();

    // Writer task via channel (no mutex needed)
    let (tx, mut rx) = mpsc::channel::<(FrameType, u32, Vec<u8>)>(512);
    *state.control_tx.lock().await = Some(tx);

    let state_w = state.clone();
    let writer_task = tokio::spawn(async move {
        while let Some((ft, sid, payload)) = rx.recv().await {
            if write_frame(&mut writer, ft, sid, &payload).await.is_err() {
                break;
            }
        }
        *state_w.control_tx.lock().await = None;
    });

    let state_r = state.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok((FrameType::Ping, _, _)) => {
                    *state_r.last_heartbeat.lock().await = Instant::now();
                    if let Some(tx) = state_r.control_tx.lock().await.as_ref() {
                        let _ = tx.send((FrameType::Pong, 0, vec![])).await;
                    }
                }
                Ok((ft, sid, _)) => {
                    warn!("Unexpected control frame: {ft:?} sid={sid}");
                }
                Err(e) => {
                    error!("Control read: {e}");
                    break;
                }
            }
        }
        *state_r.control_tx.lock().await = None;
        state_r.pending.write().await.clear();
    });

    tokio::select! {
        _ = writer_task => {},
        _ = reader_task => {},
    }
    info!("Control closed");
    Ok(())
}

/// Home client opens a data connection for a specific session.
/// Reads DataConnect frame with session_id, then hands off the stream.
async fn handle_data_connect(
    stream: TcpStream,
    state: Arc<ServerState>,
) -> std::io::Result<()> {
    tune_socket(&stream);
    let (rd, wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let writer = BufWriter::new(wr);

    let (ft, session_id, _) = read_frame(&mut reader).await?;
    if ft != FrameType::DataConnect {
        return Ok(());
    }

    // Reunite the split stream
    let stream = reader.into_inner().reunite(writer.into_inner())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "reunite failed"))?;

    // Find pending session and deliver this stream
    if let Some(tx) = state.pending.write().await.remove(&session_id) {
        let _ = tx.send(stream);
    }

    Ok(())
}

/// External user connects. We ask the home client to open a data connection,
/// then bridge the two streams with zero-copy relay.
async fn handle_proxy(
    ext_stream: TcpStream,
    state: Arc<ServerState>,
) -> std::io::Result<()> {
    tune_socket(&ext_stream);

    let control_tx = state.control_tx.lock().await.clone();
    let control_tx = match control_tx {
        Some(tx) => tx,
        None => {
            warn!("No home client, rejecting");
            return Ok(());
        }
    };

    // Assign session
    let session_id = {
        let mut id = state.next_session_id.lock().await;
        let sid = *id;
        *id = id.wrapping_add(1);
        if *id == 0 { *id = 1; }
        sid
    };

    // Create oneshot for data connection delivery
    let (tx, rx) = oneshot::channel::<TcpStream>();
    state.pending.write().await.insert(session_id, tx);

    // Tell home client to open a data connection
    if control_tx
        .send((FrameType::NewConnection, session_id, vec![]))
        .await
        .is_err()
    {
        state.pending.write().await.remove(&session_id);
        return Ok(());
    }

    info!("Session {session_id}: waiting for data connection");

    // Wait for home client to connect (timeout 10s)
    let data_stream = match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(stream)) => stream,
        _ => {
            warn!("Session {session_id}: data connection timeout");
            state.pending.write().await.remove(&session_id);
            return Ok(());
        }
    };

    info!("Session {session_id}: bridging (zero-copy relay)");

    // Zero-copy bidirectional relay — this uses splice() on Linux
    let (mut ext_rd, mut ext_wr) = ext_stream.into_split();
    let (mut data_rd, mut data_wr) = data_stream.into_split();

    let c1 = tokio::io::copy(&mut ext_rd, &mut data_wr);
    let c2 = tokio::io::copy(&mut data_rd, &mut ext_wr);

    tokio::select! {
        r = c1 => { if let Ok(n) = r { info!("Session {session_id}: ext→home {n} bytes"); } }
        r = c2 => { if let Ok(n) = r { info!("Session {session_id}: home→ext {n} bytes"); } }
    }

    // Shutdown both directions
    let _ = data_wr.shutdown().await;
    let _ = ext_wr.shutdown().await;

    info!("Session {session_id}: done");
    Ok(())
}
