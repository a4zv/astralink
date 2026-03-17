use soundcloud_rs::{query::TracksQuery, Client};
use thiserror::Error;

use crate::model::{Track, TrackSource};

#[derive(Debug, Error)]
pub enum SoundcloudError {
    #[error("soundcloud is not configured")]
    NotConfigured,
    #[error("soundcloud request failed: {0}")]
    RequestFailed(String),
}

pub async fn resolve_soundcloud_query(
    query: &str,
    client_id: Option<&str>,
) -> Result<Vec<Track>, SoundcloudError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let _client_id = client_id.ok_or(SoundcloudError::NotConfigured)?;

    let client = Client::new()
        .await
        .map_err(|err| SoundcloudError::RequestFailed(err.to_string()))?;

    let search_query = TracksQuery {
        q: Some(query.to_string()),
        limit: Some(5),
        ..Default::default()
    };

    let search = client
        .search_tracks(Some(&search_query))
        .await
        .map_err(|err| SoundcloudError::RequestFailed(err.to_string()))?;

    let mut join_set = tokio::task::JoinSet::new();

    for item in search.collection.into_iter() {
        join_set.spawn(async move {
            let artist = item
                .user
                .as_ref()
                .and_then(|u| u.username.as_deref())
                .unwrap_or_default();
            let title = item.title.as_deref().unwrap_or_default();
            let query_string = format!("{artist} - {title}");

            super::ytdl::resolve_query_to_tracks(&query_string, TrackSource::Soundcloud)
                .await
                .map_err(|err| SoundcloudError::RequestFailed(err.to_string()))
        });
    }

    let mut tracks = Vec::new();

    while let Some(result) = join_set.join_next().await {
        if let Ok(Ok(mut resolved)) = result {
            tracks.append(&mut resolved);
        }
    }

    Ok(tracks)
}
