/// proxy-client: high-performance reverse proxy client on home machine
///
/// Architecture (v2 - separate data connections for zero-copy):
///   1. Connects control channel to server, authenticates
///   2. On NewConnection(session_id): opens BOTH a local connection AND
///      a new TCP to server's data port
///   3. Sends DataConnect(session_id) handshake on the data connection
///   4. Bridges local<->data with tokio::io::copy (splice on Linux)
///   5. Control channel only carries heartbeat + signaling
mod protocol;

use protocol::*;

use clap::Parser;
use log::{error, info, warn};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "proxy-client", about = "High-performance reverse proxy client")]
struct Args {
    #[arg(long, default_value = "104.244.95.160")]
    server: String,
    #[arg(long, default_value_t = 7000)]
    control_port: u16,
    #[arg(long, default_value_t = 7002)]
    data_port: u16,
    #[arg(long, default_value = "127.0.0.1:22")]
    local_target: String,
    #[arg(long, default_value = "change-me-secret")]
    secret: String,
    #[arg(long, default_value_t = 10)]
    heartbeat_interval: u64,
    #[arg(long, default_value_t = 60)]
    max_reconnect_delay: u64,
}

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let mut backoff = 1u64;

    loop {
        info!("Connecting to {}:{}...", args.server, args.control_port);
        match run_client(&args).await {
            Ok(()) => { backoff = 1; }
            Err(e) => { error!("Connection error: {e}"); }
        }
        let delay = backoff.min(args.max_reconnect_delay);
        warn!("Reconnecting in {delay}s...");
        tokio::time::sleep(Duration::from_secs(delay)).await;
        backoff = (backoff * 2).min(args.max_reconnect_delay);
    }
}

async fn run_client(args: &Args) -> std::io::Result<()> {
    let stream = TcpStream::connect(format!("{}:{}", args.server, args.control_port)).await?;
    tune_socket(&stream);
    info!("Connected to server");

    let (rd, wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let mut writer = BufWriter::new(wr);

    // Auth
    write_frame(&mut writer, FrameType::Auth, 0, args.secret.as_bytes()).await?;
    let (ft, _, payload) = read_frame(&mut reader).await?;
    if ft != FrameType::AuthOk {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("Auth failed: {}", String::from_utf8_lossy(&payload)),
        ));
    }
    info!("Authenticated");

    // Shared writer for heartbeat (control channel is low-traffic, mutex is fine)
    let writer = Arc::new(Mutex::new(writer));
    let last_pong = Arc::new(Mutex::new(Instant::now()));

    // Heartbeat sender
    let writer_hb = writer.clone();
    let last_pong_hb = last_pong.clone();
    let hb_interval = args.heartbeat_interval;
    let hb_task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(hb_interval)).await;
            let elapsed = last_pong_hb.lock().await.elapsed();
            if elapsed > Duration::from_secs(hb_interval * 3) {
                error!("No pong for {}s, connection dead", elapsed.as_secs());
                return;
            }
            let mut w = writer_hb.lock().await;
            if write_frame(&mut *w, FrameType::Ping, 0, b"").await.is_err() {
                error!("Heartbeat send failed");
                return;
            }
        }
    });

    let server_addr = args.server.clone();
    let data_port = args.data_port;
    let local_target = args.local_target.clone();

    // Control channel reader: only heartbeat pongs + NewConnection signals
    let reader_task = tokio::spawn(async move {
        loop {
            match read_frame(&mut reader).await {
                Ok((FrameType::Pong, _, _)) => {
                    *last_pong.lock().await = Instant::now();
                }
                Ok((FrameType::NewConnection, session_id, _)) => {
                    let server = server_addr.clone();
                    let target = local_target.clone();
                    // Spawn independent relay task — no locks, no channels on data path
                    tokio::spawn(async move {
                        if let Err(e) = relay_session(session_id, &server, data_port, &target).await
                        {
                            error!("Session {session_id} error: {e}");
                        }
                    });
                }
                Ok((ft, _, _)) => {
                    warn!("Unexpected: {ft:?}");
                }
                Err(e) => {
                    error!("Control read: {e}");
                    return;
                }
            }
        }
    });

    tokio::select! {
        _ = hb_task => { warn!("Heartbeat ended"); }
        _ = reader_task => { warn!("Reader ended"); }
    }
    Ok(())
}

/// Per-session relay: connect to server data port + local target,
/// then zero-copy bidirectional copy.
async fn relay_session(
    session_id: u32,
    server: &str,
    data_port: u16,
    local_target: &str,
) -> std::io::Result<()> {
    // Connect to server data port
    let data_stream = TcpStream::connect(format!("{server}:{data_port}")).await?;
    tune_socket(&data_stream);

    // Send DataConnect handshake
    {
        let (rd, wr) = data_stream.into_split();
        let mut writer = BufWriter::new(wr);
        write_frame(&mut writer, FrameType::DataConnect, session_id, b"").await?;

        // Reunite after handshake
        let data_stream = rd.reunite(writer.into_inner())
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "reunite"))?;

        // Connect to local target
        let local_stream = TcpStream::connect(local_target).await?;
        tune_socket(&local_stream);

        info!("Session {session_id}: bridging {local_target} (zero-copy)");

        // Zero-copy bidirectional relay
        let (mut data_rd, mut data_wr) = data_stream.into_split();
        let (mut local_rd, mut local_wr) = local_stream.into_split();

        let c1 = tokio::io::copy(&mut data_rd, &mut local_wr);
        let c2 = tokio::io::copy(&mut local_rd, &mut data_wr);

        tokio::select! {
            r = c1 => { if let Ok(n) = r { info!("Session {session_id}: server→local {n}B"); } }
            r = c2 => { if let Ok(n) = r { info!("Session {session_id}: local→server {n}B"); } }
        }

        let _ = local_wr.shutdown().await;
        let _ = data_wr.shutdown().await;
    }

    info!("Session {session_id}: closed");
    Ok(())
}
