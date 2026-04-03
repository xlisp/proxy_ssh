use serde::{Deserialize, Serialize};

/// Frame header: 1 byte type + 4 bytes session_id + 4 bytes payload length
/// Total header: 9 bytes
pub const HEADER_SIZE: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum FrameType {
    /// Heartbeat ping from client
    Ping = 1,
    /// Heartbeat pong from server
    Pong = 2,
    /// Server tells client: new external connection arrived, open a data channel
    NewConnection = 3,
    /// Data frame
    Data = 4,
    /// Connection closed
    Close = 5,
    /// Authentication
    Auth = 6,
    /// Auth OK
    AuthOk = 7,
}

impl TryFrom<u8> for FrameType {
    type Error = &'static str;
    fn try_from(v: u8) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::Ping),
            2 => Ok(Self::Pong),
            3 => Ok(Self::NewConnection),
            4 => Ok(Self::Data),
            5 => Ok(Self::Close),
            6 => Ok(Self::Auth),
            7 => Ok(Self::AuthOk),
            _ => Err("unknown frame type"),
        }
    }
}

/// Encode a frame into bytes: [type:1][session_id:4][len:4][payload:len]
pub fn encode_frame(frame_type: FrameType, session_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());
    buf.push(frame_type as u8);
    buf.extend_from_slice(&session_id.to_be_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read one frame from a stream. Returns (frame_type, session_id, payload).
pub async fn read_frame<R: AsyncReadExt + Unpin>(
    reader: &mut R,
) -> std::io::Result<(FrameType, u32, Vec<u8>)> {
    let mut header = [0u8; HEADER_SIZE];
    reader.read_exact(&mut header).await?;

    let frame_type = FrameType::try_from(header[0])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let session_id = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let payload_len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;

    // Limit payload to 16MB to prevent memory abuse
    if payload_len > 16 * 1024 * 1024 {
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

/// Write one frame to a stream.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    frame_type: FrameType,
    session_id: u32,
    payload: &[u8],
) -> std::io::Result<()> {
    let buf = encode_frame(frame_type, session_id, payload);
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}
