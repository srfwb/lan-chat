use serde::{Deserialize, Serialize};

pub const APP_TAG: &str = "lan-chat-v1";

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub sender_name: String,
    pub sender_id: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileOffer {
    pub id: String,
    pub sender_name: String,
    pub sender_id: String,
    pub timestamp: u64,
    pub filename: String,
    pub size: u64,
    pub mime: String,
    pub hash: String,
}

/// Wrapper unifié pour tout ce qui transite via gossipsub : chat + annonces fichiers.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WireMessage {
    Chat(ChatMessage),
    FileOffer(FileOffer),
}

/// Enveloppe chiffrée publiée sur gossipsub : jamais de clair sur le réseau.
/// `n` = nonce 12 octets (base64), `c` = ciphertext + tag AEAD (base64).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EncryptedEnvelope {
    pub n: String,
    pub c: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum NodeStatus {
    Initializing,
    AwaitingRoom,
    Ready { peer_id: String, room_name: String },
    Error { message: String },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RoomConfig {
    pub version: u32,
    pub code: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomStatusResp {
    pub configured: bool,
    pub room_name: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NodeReadyPayload {
    pub peer_id: String,
    pub room_name: String,
}

/// Pilotage du cycle de vie du swarm : permet le hot re-key (abort + respawn).
pub enum ManagerCmd {
    StartRoom(RoomConfig),
    LeaveRoom,
}
