use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};

/// Frame header: 1 byte type + 4 bytes session_id + 4 bytes payload length
pub const HEADER_SIZE: usize = 9;

/// Socket buffer size hint (256KB)
pub const SOCK_BUF_SIZE: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Ping = 1,
    Pong = 2,
    /// Server → Client: new session, open a data connection
    NewConnection = 3,
    /// Session closed
    Close = 5,
    /// Client → Server: auth request (payload = secret)
    Auth = 6,
    /// Server → Client: auth ok
    AuthOk = 7,
    /// Client → Server: data channel handshake (payload = session_id as 4 bytes)
    DataConnect = 8,
}

impl TryFrom<u8> for FrameType {
    type Error = &'static str;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Ping),
            2 => Ok(Self::Pong),
            3 => Ok(Self::NewConnection),
            5 => Ok(Self::Close),
            6 => Ok(Self::Auth),
            7 => Ok(Self::AuthOk),
            8 => Ok(Self::DataConnect),
            _ => Err("unknown frame type"),
        }
    }
}

/// Read one frame from a buffered stream.
pub async fn read_frame<R: AsyncReadExt + Unpin>(
    reader: &mut BufReader<R>,
) -> std::io::Result<(FrameType, u32, Vec<u8>)> {
    let mut header = [0u8; HEADER_SIZE];
    reader.read_exact(&mut header).await?;

    let frame_type = FrameType::try_from(header[0])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let session_id = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let payload_len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;

    if payload_len > 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "payload too large",
        ));
    }

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload).await?;
    }

    Ok((frame_type, session_id, payload))
}

/// Write one frame to a buffered stream.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut BufWriter<W>,
    frame_type: FrameType,
    session_id: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut header = [0u8; HEADER_SIZE];
    header[0] = frame_type as u8;
    header[1..5].copy_from_slice(&session_id.to_be_bytes());
    header[5..9].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    if !payload.is_empty() {
        writer.write_all(payload).await?;
    }
    writer.flush().await?;
    Ok(())
}

/// Set TCP socket options for low latency.
pub fn tune_socket(stream: &tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    // Try to set larger socket buffers via socket2
    if let Ok(std_stream) = stream.as_ref().try_clone_to_owned() {
        let sock = socket2::Socket::from(std_stream);
        let _ = sock.set_send_buffer_size(SOCK_BUF_SIZE);
        let _ = sock.set_recv_buffer_size(SOCK_BUF_SIZE);
        // Don't drop — we don't own it, leak to avoid closing
        std::mem::forget(sock);
    }
}

trait TryCloneToOwned {
    fn try_clone_to_owned(&self) -> std::io::Result<std::net::TcpStream>;
}

impl TryCloneToOwned for std::os::fd::RawFd {
    fn try_clone_to_owned(&self) -> std::io::Result<std::net::TcpStream> {
        use std::os::fd::FromRawFd;
        // Dup the fd so we don't steal ownership
        let new_fd = unsafe { libc::dup(*self) };
        if new_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { std::net::TcpStream::from_raw_fd(new_fd) })
    }
}

impl TryCloneToOwned for tokio::net::TcpStream {
    fn try_clone_to_owned(&self) -> std::io::Result<std::net::TcpStream> {
        use std::os::unix::io::AsRawFd;
        self.as_raw_fd().try_clone_to_owned()
    }
}
