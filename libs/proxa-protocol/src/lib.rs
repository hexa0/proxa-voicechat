use serde::{Deserialize, Serialize};

/// control messages sent typically over a reliable stream (e.g. QUIC bi-directional streams)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// request to join a room.
    JoinRoom(String),
    /// graceful disconnect from the current room.
    LeaveRoom,
    ReportPeerLoss {
        peer_id: u32,
        loss_rate: f32,
    },
    /// signal that we are entering/leaving a silent state to avoid misreporting loss.
    SetSilence(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    RoomJoined { peer_id: u32 },
    PeerJoined { peer_id: u32 },
    PeerLeft { peer_id: u32 },
    PeerSilence { peer_id: u32, silenced: bool },
    TargetLossRate(f32),
    Error(String),
}

/// helper struct for serializing / deserializing Client datagrams
pub struct ClientAudioPacket<'a> {
    pub sequence: u32,
    /// opus encoded payload
    pub payload: &'a [u8],
}

impl<'a> ClientAudioPacket<'a> {
    pub fn serialize(sequence: u32, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&sequence.to_le_bytes());
        buf.extend_from_slice(payload);
        buf
    }

    pub fn deserialize(data: &'a [u8]) -> Option<Self> {
        if data.len() < 4 {
            return None;
        }
        let mut seq_bytes = [0u8; 4];
        seq_bytes.copy_from_slice(&data[0..4]);
        let sequence = u32::from_le_bytes(seq_bytes);
        let payload = &data[4..];
        Some(Self { sequence, payload })
    }
}

/// helper struct for serializing / deserializing Server datagrams
#[derive(Debug, Clone)]
pub struct ServerAudioPacket {
    pub peer_id: u32,
    pub sequence: u32,
    /// opus encoded payload
    pub payload: Vec<u8>,
}

impl ServerAudioPacket {
    /// serialization: [peer_id (4 bytes)][sequence (4 bytes)][payload]
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(8 + self.payload.len());
        buf.extend_from_slice(&self.peer_id.to_le_bytes());
        buf.extend_from_slice(&self.sequence.to_le_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn deserialize(data: &[u8]) -> Option<Self> {
        if data.len() < 8 {
            return None;
        }
        let mut id_bytes = [0u8; 4];
        id_bytes.copy_from_slice(&data[0..4]);
        let peer_id = u32::from_le_bytes(id_bytes);

        let mut seq_bytes = [0u8; 4];
        seq_bytes.copy_from_slice(&data[4..8]);
        let sequence = u32::from_le_bytes(seq_bytes);

        let payload = data[8..].to_vec();
        Some(Self {
            peer_id,
            sequence,
            payload,
        })
    }
}
