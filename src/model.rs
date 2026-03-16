use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type GuildId = u64;
pub type UserId = u64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackSource {
    Youtube,
    Spotify,
    Soundcloud,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Track {
    pub id: Uuid,
    pub title: String,
    pub url: String,
    pub thumbnail: Option<String>,
    pub duration_ms: Option<u64>,
    pub source: TrackSource,
    pub original_url: Option<String>,
    pub artist: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub track: Track,
    pub requested_by: UserId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildSessionState {
    pub guild_id: GuildId,
    pub queue: Vec<QueueItem>,
    pub current: Option<Track>,
    pub paused: bool,
    pub volume: u8,
    pub position_ms: u64,
    pub is_finished: bool,
    pub node: Option<NodeInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveRequest {
    pub query: String,
    #[serde(default)]
    pub source: Option<ResolveSourceHint>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolveSourceHint {
    Youtube,
    Spotify,
    Soundcloud,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnqueueRequest {
    pub query: String,
    pub user_id: UserId,
    #[serde(default)]
    pub position: Option<usize>,
    #[serde(default)]
    pub source: Option<ResolveSourceHint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveResponse {
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateResponse {
    pub queue: Vec<QueueItem>,
    pub current: Option<Track>,
    pub paused: bool,
    pub volume: u8,
    pub position_ms: u64,
    pub is_finished: bool,
    pub node: Option<NodeInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionRequest {
    Play,
    Pause,
    Resume,
    Skip,
    Stop,
    Clear,
    SetVolume { volume: u8 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub state: Option<StateResponse>,
    pub node: Option<NodeInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueResponse {
    pub state: StateResponse,
    pub success: bool,
    pub message: Option<String>,
    pub node: Option<NodeInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: String,
    pub host: String,
    pub http_port: u16,
    pub audio_port: u16,
    pub available_slots: usize,
    pub total_slots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodesResponse {
    pub nodes: Vec<NodeInfo>,
}

impl From<&GuildSessionState> for StateResponse {
    fn from(state: &GuildSessionState) -> Self {
        Self {
            queue: state.queue.clone(),
            current: state.current.clone(),
            paused: state.paused,
            volume: state.volume,
            position_ms: state.position_ms,
            is_finished: state.is_finished,
            node: state.node.clone(),
        }
    }
}
