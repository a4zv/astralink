use dashmap::DashMap;
use std::error::Error as StdError;
use tracing::info;

use crate::model::{
    ActionRequest, GuildId, GuildSessionState, NodeInfo, QueueItem, StateResponse, Track,
};

#[derive(Debug)]
pub struct AstralinkState {
    guilds: DashMap<GuildId, GuildSessionState>,
    node_id: String,
}

impl AstralinkState {
    pub fn new(node_id: String) -> Self {
        Self {
            guilds: DashMap::new(),
            node_id,
        }
    }

    pub async fn get_or_init(&self, guild_id: GuildId) -> GuildSessionState {
        if let Some(state) = self.guilds.get(&guild_id) {
            return state.clone();
        }

        let state = GuildSessionState {
            guild_id,
            queue: Vec::new(),
            current: None,
            paused: false,
            volume: 100,
            position_ms: 0,
            is_finished: false,
            node: None,
        };

        let _ = self.set_state_async(&state).await;
        state
    }

    pub async fn set_state_async(
        &self,
        state: &GuildSessionState,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        if let Some(mut existing) = self.guilds.get_mut(&state.guild_id) {
            *existing = state.clone();
        } else {
            self.guilds.insert(state.guild_id, state.clone());
        }
        Ok(())
    }

    pub fn assign_node(&self, guild_id: GuildId, node: NodeInfo) {
        if let Some(mut state) = self.guilds.get_mut(&guild_id) {
            state.node = Some(node);
        }
    }

    pub fn is_assigned_to_me(&self, guild_id: GuildId) -> bool {
        if let Some(state) = self.guilds.get(&guild_id) {
            state.node.as_ref().map_or(false, |n| n.id == self.node_id)
        } else {
            false
        }
    }

    pub async fn unassign_node(
        &self,
        guild_id: GuildId,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        if let Some(mut state) = self.guilds.get_mut(&guild_id) {
            state.node = None;
            self.set_state_async(&state).await
        } else {
            Ok(())
        }
    }

    pub async fn enqueue(
        &self,
        guild_id: GuildId,
        mut tracks: Vec<QueueItem>,
        position: Option<usize>,
    ) -> GuildSessionState {
        let mut state = self.get_or_init(guild_id).await;

        if tracks.is_empty() {
            let _ = self.set_state_async(&state).await;
            return state;
        }

        let added = tracks.len();

        match position {
            Some(pos) if pos < state.queue.len() => {
                for (offset, item) in tracks.drain(..).enumerate() {
                    state.queue.insert(pos + offset, item);
                }
            }
            _ => {
                state.queue.extend(tracks.drain(..));
            }
        };

        if state.current.is_none() {
            if let Some(next) = state.queue.first() {
                state.current = Some(next.track.clone());
            }
        }

        state.is_finished = false;
        let _ = self.set_state_async(&state).await;

        info!(
            "ASTRALINK_QUEUE_ENQUEUE guild_id={} added={} queue_len={} current_title={}",
            guild_id,
            added,
            state.queue.len(),
            state
                .current
                .as_ref()
                .map(|t| t.title.as_str())
                .unwrap_or_default()
        );

        state
    }

    pub async fn apply_action(&self, guild_id: GuildId, action: ActionRequest) -> StateResponse {
        let mut state = self.get_or_init(guild_id).await;

        info!(
            "ASTRALINK_ACTION guild_id={} action={:?} queue_len={} paused={} volume={}",
            guild_id,
            action,
            state.queue.len(),
            state.paused,
            state.volume
        );

        match action {
            ActionRequest::Play => {
                if state.current.is_none() {
                    if let Some(item) = state.queue.first() {
                        state.current = Some(item.track.clone());
                        state.position_ms = 0;
                        state.paused = false;
                    }
                } else {
                    state.paused = false;
                }
            }
            ActionRequest::Pause => {
                state.paused = true;
            }
            ActionRequest::Resume => {
                state.paused = false;
            }
            ActionRequest::Skip => {
                if !state.queue.is_empty() {
                    state.queue.remove(0);
                }
                state.current = state.queue.first().map(|item| item.track.clone());
                state.position_ms = 0;
            }
            ActionRequest::Stop => {
                state.current = None;
                state.queue.clear();
                state.position_ms = 0;
                state.paused = false;
            }
            ActionRequest::Clear => {
                state.queue.clear();
            }
            ActionRequest::SetVolume { volume } => {
                state.volume = volume.clamp(0, 150);
            }
        }

        let is_finished = match action {
            ActionRequest::Stop => true,
            ActionRequest::Clear => false,
            _ => state.current.is_none() && state.queue.is_empty(),
        };
        state.is_finished = is_finished;

        let _ = self.set_state_async(&state).await;

        StateResponse::from(&state)
    }

    pub async fn as_response(&self, guild_id: GuildId) -> StateResponse {
        let state = self.get_or_init(guild_id).await;
        StateResponse::from(&state)
    }

    pub async fn increment_position_ms(&self, guild_id: GuildId, delta_ms: u64) {
        let mut state = self.get_or_init(guild_id).await;
        state.position_ms = state.position_ms.saturating_add(delta_ms);
        let _ = self.set_state_async(&state).await;
    }

    pub async fn current_for(&self, guild_id: GuildId) -> Option<Track> {
        let state = self.get_or_init(guild_id).await;
        state.current.clone()
    }

    pub async fn advance_after_track_end(&self, guild_id: GuildId) -> Option<Track> {
        let mut state = self.get_or_init(guild_id).await;

        let ended = state.current.as_ref().map(|t| t.title.clone());

        if !state.queue.is_empty() {
            state.queue.remove(0);
        }

        state.current = state.queue.first().map(|item| item.track.clone());

        if state.current.is_none() {
            state.is_finished = true;
        }

        let _ = self.set_state_async(&state).await;

        info!(
            "ASTRALINK_QUEUE_ADVANCE guild_id={} ended_title={} next_title={}",
            guild_id,
            ended.as_deref().unwrap_or(""),
            state
                .current
                .as_ref()
                .map(|t| t.title.as_str())
                .unwrap_or_default()
        );

        state.current.clone()
    }
}
