//! Bincode framing over QUIC streams: 4-byte LE length prefix + payload.

use aeon_types::AeonError;

/// Maximum frame size (64 MiB — large enough for Raft snapshots).
pub const MAX_FRAME_SIZE: u32 = 64 * 1024 * 1024;

/// Message type discriminant (first byte after length prefix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    AppendEntries = 1,
    AppendEntriesResponse = 2,
    Vote = 3,
    VoteResponse = 4,
    InstallSnapshot = 5,
    InstallSnapshotResponse = 6,
    FullSnapshot = 7,
    FullSnapshotResponse = 8,
    Discovery = 9,
    DiscoveryResponse = 10,
    HealthPing = 11,
    HealthPong = 12,
    /// Request leader to add this node to the cluster (join protocol).
    AddNodeRequest = 13,
    AddNodeResponse = 14,
    /// Request leader to remove a node from the cluster.
    RemoveNodeRequest = 15,
    RemoveNodeResponse = 16,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::AppendEntries),
            2 => Some(Self::AppendEntriesResponse),
            3 => Some(Self::Vote),
            4 => Some(Self::VoteResponse),
            5 => Some(Self::InstallSnapshot),
            6 => Some(Self::InstallSnapshotResponse),
            7 => Some(Self::FullSnapshot),
            8 => Some(Self::FullSnapshotResponse),
            9 => Some(Self::Discovery),
            10 => Some(Self::DiscoveryResponse),
            11 => Some(Self::HealthPing),
            12 => Some(Self::HealthPong),
            13 => Some(Self::AddNodeRequest),
            14 => Some(Self::AddNodeResponse),
            15 => Some(Self::RemoveNodeRequest),
            16 => Some(Self::RemoveNodeResponse),
            _ => None,
        }
    }
}

/// Write a framed message: [msg_type: u8][length: u32 LE][payload].
pub async fn write_frame(
    stream: &mut quinn::SendStream,
    msg_type: MessageType,
    payload: &[u8],
) -> Result<(), AeonError> {
    use quinn::WriteError;

    let len = payload.len() as u32;
    if len > MAX_FRAME_SIZE {
        return Err(AeonError::Connection {
            message: format!("frame too large: {len} > {MAX_FRAME_SIZE}"),
            source: None,
            retryable: false,
        });
    }

    let mut header = [0u8; 5];
    header[0] = msg_type as u8;
    header[1..5].copy_from_slice(&len.to_le_bytes());

    stream
        .write_all(&header)
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("failed to write frame header: {e}"),
            source: None,
            retryable: matches!(e, WriteError::ConnectionLost(_)),
        })?;

    if !payload.is_empty() {
        stream
            .write_all(payload)
            .await
            .map_err(|e| AeonError::Connection {
                message: format!("failed to write frame payload: {e}"),
                source: None,
                retryable: matches!(e, WriteError::ConnectionLost(_)),
            })?;
    }

    Ok(())
}

/// Read a framed message: returns (msg_type, payload).
pub async fn read_frame(
    stream: &mut quinn::RecvStream,
) -> Result<(MessageType, Vec<u8>), AeonError> {
    use quinn::ReadExactError;

    let mut header = [0u8; 5];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| AeonError::Connection {
            message: format!("failed to read frame header: {e}"),
            source: None,
            retryable: matches!(e, ReadExactError::ReadError(_)),
        })?;

    let msg_type = MessageType::from_u8(header[0]).ok_or_else(|| AeonError::Connection {
        message: format!("unknown message type: {}", header[0]),
        source: None,
        retryable: false,
    })?;

    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    if len > MAX_FRAME_SIZE {
        return Err(AeonError::Connection {
            message: format!("frame too large: {len} > {MAX_FRAME_SIZE}"),
            source: None,
            retryable: false,
        });
    }

    let mut payload = vec![0u8; len as usize];
    if len > 0 {
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|e| AeonError::Connection {
                message: format!("failed to read frame payload: {e}"),
                source: None,
                retryable: matches!(e, ReadExactError::ReadError(_)),
            })?;
    }

    Ok((msg_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_type_roundtrip() {
        for i in 1..=16u8 {
            let mt = MessageType::from_u8(i).unwrap();
            assert_eq!(mt as u8, i);
        }
        assert!(MessageType::from_u8(0).is_none());
        assert!(MessageType::from_u8(17).is_none());
    }

    #[test]
    fn max_frame_size_is_64mib() {
        assert_eq!(MAX_FRAME_SIZE, 64 * 1024 * 1024);
    }
}
