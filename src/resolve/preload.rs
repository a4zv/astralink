use std::sync::Arc;

use dashmap::DashSet;
use tracing::info;
use uuid::Uuid;

use crate::model::{GuildId, QueueItem};
use crate::state::AstralinkState;

static PRELOADING: once_cell::sync::Lazy<DashSet<Uuid>> =
    once_cell::sync::Lazy::new(DashSet::new);

pub fn trigger_preload(
    state: Arc<AstralinkState>,
    guild_id: GuildId,
    _config: Arc<crate::config::AstralinkConfig>,
) {
    tokio::spawn(async move {
        let session = state.get_or_init(guild_id).await;

        let next_items: Vec<&QueueItem> = session
            .queue
            .iter()
            .skip(1)
            .take(2)
            .filter(|item| !PRELOADING.contains(&item.track.id))
            .collect();

        for item in next_items {
            let track = item.track.clone();

            if PRELOADING.contains(&track.id) {
                continue;
            }
            PRELOADING.insert(track.id);

            tokio::spawn(async move {
                info!(
                    "ASTRALINK_PRELOAD_START guild_id={} track_id={} title=\"{}\"",
                    guild_id, track.id, track.title
                );

                let result = super::ytdl::resolve_url_to_track(
                    &track.url,
                    track.source,
                )
                .await;

                match &result {
                    Ok(tracks) => info!(
                        "ASTRALINK_PRELOAD_DONE guild_id={} track_id={} resolved={}",
                        guild_id,
                        track.id,
                        tracks.len()
                    ),
                    Err(err) => info!(
                        "ASTRALINK_PRELOAD_FAILED guild_id={} track_id={} error={}",
                        guild_id, track.id, err
                    ),
                }

                PRELOADING.remove(&track.id);
            });
        }
    });
}
