/// proxy-client: runs on home Linux machine
///
/// Connects to the public server's control port, authenticates,
/// and waits for NewConnection frames. For each new session,
/// opens a local TCP connection to the target service and relays data.
///
/// Features:
///   - Heartbeat ping every N seconds
///   - Auto-reconnect on disconnect with exponential backoff
mod protocol;

use protocol::*;

use clap::Parser;
use log::{error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "proxy-client", about = "Reverse proxy client on home machine")]
struct Args {
    /// Public server address
    #[arg(long, default_value = "104.244.95.160")]
    server: String,

    /// Public server control port
    #[arg(long, default_value_t = 7000)]
    control_port: u16,

    /// Local target address to forward to (e.g. 127.0.0.1:22 for SSH)
    #[arg(long, default_value = "127.0.0.1:22")]
    local_target: String,

    /// Shared secret for authentication
    #[arg(long, default_value = "change-me-secret")]
    secret: String,

    /// Heartbeat interval in seconds
    #[arg(long, default_value_t = 10)]
    heartbeat_interval: u64,

    /// Max reconnect delay in seconds
    #[arg(long, default_value_t = 60)]
    max_reconnect_delay: u64,
}

/// Per-session: channel to send data from local service back to server
struct LocalSession {
    tx: mpsc::Sender<Vec<u8>>,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    let mut backoff = 1u64;

    loop {
        info!(
            "Connecting to server {}:{}...",
            args.server, args.control_port
        );

        match run_client(&args).await {
            Ok(()) => {
                info!("Connection closed normally");
                backoff = 1;
            }
            Err(e) => {
                error!("Connection error: {e}");
            }
        }

        let delay = backoff.min(args.max_reconnect_delay);
        warn!("Reconnecting in {delay}s...");
        tokio::time::sleep(Duration::from_secs(delay)).await;
        backoff = (backoff * 2).min(args.max_reconnect_delay);
    }
}

async fn run_client(args: &Args) -> std::io::Result<()> {
    let addr = format!("{}:{}", args.server, args.control_port);
    let mut stream = TcpStream::connect(&addr).await?;
    stream.set_nodelay(true)?;
    info!("Connected to server");

    // Authenticate
    write_frame(&mut stream, FrameType::Auth, 0, args.secret.as_bytes()).await?;

    let (ft, _, payload) = read_frame(&mut stream).await?;
    if ft != FrameType::AuthOk {
        let msg = String::from_utf8_lossy(&payload);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("Auth failed: {msg}"),
        ));
    }
    info!("Authenticated with server");

    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let sessions: Arc<RwLock<HashMap<u32, LocalSession>>> = Arc::new(RwLock::new(HashMap::new()));
    let last_pong = Arc::new(Mutex::new(Instant::now()));

    // Heartbeat sender
    let writer_hb = writer.clone();
    let heartbeat_interval = args.heartbeat_interval;
    let last_pong_hb = last_pong.clone();
    let hb_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(heartbeat_interval)).await;

            // Check if we got a pong recently
            let elapsed = last_pong_hb.lock().await.elapsed();
            if elapsed > Duration::from_secs(heartbeat_interval * 3) {
                error!("No pong received for {}s, connection likely dead", elapsed.as_secs());
                return;
            }

            let mut w = writer_hb.lock().await;
            if write_frame(&mut *w, FrameType::Ping, 0, b"").await.is_err() {
                error!("Failed to send heartbeat");
                return;
            }
        }
    });

    let local_target = args.local_target.clone();

    // Read frames from server
    let writer_r = writer.clone();
    let sessions_r = sessions.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok((FrameType::Pong, _, _)) => {
                    *last_pong.lock().await = Instant::now();
                }
                Ok((FrameType::NewConnection, session_id, _)) => {
                    info!("New session {session_id}, connecting to {local_target}");
                    let local_target = local_target.clone();
                    let writer = writer_r.clone();
                    let sessions = sessions_r.clone();

                    // Connect to local target
                    match TcpStream::connect(&local_target).await {
                        Ok(local_stream) => {
                            let (mut local_reader, local_writer) = local_stream.into_split();
                            let local_writer = Arc::new(Mutex::new(local_writer));

                            // Channel for data from server -> local
                            let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
                            sessions
                                .write()
                                .await
                                .insert(session_id, LocalSession { tx });

                            // Server -> Local: write received data to local service
                            let lw = local_writer.clone();
                            let sessions2 = sessions.clone();
                            tokio::spawn(async move {
                                while let Some(data) = rx.recv().await {
                                    let mut w = lw.lock().await;
                                    if w.write_all(&data).await.is_err() {
                                        break;
                                    }
                                    if w.flush().await.is_err() {
                                        break;
                                    }
                                }
                                sessions2.write().await.remove(&session_id);
                            });

                            // Local -> Server: read from local, send Data frames
                            let writer2 = writer.clone();
                            let sessions3 = sessions.clone();
                            tokio::spawn(async move {
                                let mut buf = vec![0u8; 32 * 1024];
                                loop {
                                    match local_reader.read(&mut buf).await {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            let mut w = writer2.lock().await;
                                            if write_frame(
                                                &mut *w,
                                                FrameType::Data,
                                                session_id,
                                                &buf[..n],
                                            )
                                            .await
                                            .is_err()
                                            {
                                                break;
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                                // Send close
                                let mut w = writer2.lock().await;
                                let _ =
                                    write_frame(&mut *w, FrameType::Close, session_id, b"").await;
                                sessions3.write().await.remove(&session_id);
                                info!("Session {session_id} local connection closed");
                            });
                        }
                        Err(e) => {
                            error!(
                                "Failed to connect to local target {local_target}: {e}"
                            );
                            // Send close back to server
                            let mut w = writer.lock().await;
                            let _ =
                                write_frame(&mut *w, FrameType::Close, session_id, b"").await;
                        }
                    }
                }
                Ok((FrameType::Data, session_id, payload)) => {
                    let sessions = sessions_r.read().await;
                    if let Some(session) = sessions.get(&session_id) {
                        let _ = session.tx.send(payload).await;
                    }
                }
                Ok((FrameType::Close, session_id, _)) => {
                    sessions_r.write().await.remove(&session_id);
                    info!("Session {session_id} closed by server");
                }
                Ok((ft, _, _)) => {
                    warn!("Unexpected frame: {ft:?}");
                }
                Err(e) => {
                    error!("Read error: {e}");
                    return;
                }
            }
        }
    });

    tokio::select! {
        _ = hb_task => {
            warn!("Heartbeat task ended");
        },
        _ = reader_task => {
            warn!("Reader task ended");
        },
    }

    // Clean up sessions
    sessions.write().await.clear();
    Ok(())
}
