use std::time::{Duration, Instant};

use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::Deserialize;
use thiserror::Error;
use tokio::process::Command;
use tracing::info;
use uuid::Uuid;
use youtube_dl::YoutubeDl;

use crate::model::{Track, TrackSource};

#[derive(Debug, Error)]
pub enum YtdlError {
    #[error("yt-dlp not available in PATH")]
    MissingBinary,
    #[error("yt-dlp invocation failed: {0}")]
    InvocationFailed(String),
    #[error("failed to parse yt-dlp json: {0}")]
    ParseError(String),
}

#[derive(Clone)]
struct CachedTrack {
    track: Track,
    resolved_at: Instant,
}

struct TrackCache {
    entries: DashMap<String, CachedTrack>,
    ttl: Duration,
}

impl TrackCache {
    fn get(&self, key: &str) -> Option<Track> {
        let entry = self.entries.get(key)?;
        if entry.resolved_at.elapsed() > self.ttl {
            self.entries.remove(key);
            return None;
        }
        Some(entry.track.clone())
    }

    fn insert(&self, key: String, track: Track) {
        let cached = CachedTrack {
            track,
            resolved_at: Instant::now(),
        };
        self.entries.insert(key, cached);
    }
}

static TRACK_CACHE: Lazy<TrackCache> = Lazy::new(|| TrackCache {
    entries: DashMap::new(),
    ttl: Duration::from_secs(600),
});

fn normalize_for_match(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
}

fn score_title_for_audio(title: &str, original_query: &str) -> i32 {
    let lower = title.to_lowercase();
    let normalized_title = normalize_for_match(title);
    let normalized_query = normalize_for_match(original_query);

    let query_tokens: Vec<&str> = normalized_query
        .split_whitespace()
        .filter(|token| token.len() >= 3)
        .collect();

    let mut score = 0;

    if !normalized_query.trim().is_empty() {
        if normalized_title.contains(normalized_query.trim()) {
            score += 140;
        }

        let matched_tokens = query_tokens
            .iter()
            .filter(|token| normalized_title.contains(**token))
            .count() as i32;

        score += matched_tokens * 14;

        let missing_tokens = query_tokens.len() as i32 - matched_tokens;
        score -= missing_tokens * 9;
    }

    if lower.contains("official audio") {
        score += 60;
    } else if lower.contains("audio") {
        score += 30;
    }

    if lower.contains("lyrics") || lower.contains("lyric") {
        score -= 30;
    }

    if lower.contains("visualizer") {
        score -= 30;
    }

    if lower.contains("music video") || lower.contains("official video") || lower.contains("mv") {
        score -= 40;
    }

    if lower.contains("live") {
        score -= 40;
    }

    if lower.contains("fan video") || lower.contains("fan-made") {
        score -= 20;
    }

    score
}

fn video_to_track(video: youtube_dl::SingleVideo, source: TrackSource) -> Track {
    let url = video
        .url
        .clone()
        .unwrap_or_else(|| video.webpage_url.clone().unwrap_or_default());

    Track {
        id: Uuid::new_v4(),
        title: video.title.unwrap_or_default(),
        url,
        thumbnail: video.thumbnail,
        duration_ms: video.duration.and_then(|v| v.as_u64()).map(|d| d * 1000),
        source,
        original_url: video.webpage_url,
        artist: video.uploader,
    }
}

fn extract_tracks(output: youtube_dl::YoutubeDlOutput, source: TrackSource) -> Vec<Track> {
    match output {
        youtube_dl::YoutubeDlOutput::SingleVideo(video) => {
            vec![video_to_track(*video, source)]
        }
        youtube_dl::YoutubeDlOutput::Playlist(playlist) => playlist
            .entries
            .into_iter()
            .flatten()
            .map(|item| video_to_track(item, source))
            .collect(),
    }
}

#[derive(Debug, Deserialize)]
struct FlatVideo {
    id: Option<String>,
    title: Option<String>,
    url: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    uploader: Option<String>,
    webpage_url: Option<String>,
}

async fn flat_search(
    search_query: &str,
    count: u32,
    use_proxy: bool,
) -> Result<Vec<FlatVideo>, YtdlError> {
    let query = format!("ytsearch{count}:{search_query}");
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--flat-playlist")
        .arg("--dump-json")
        .arg("--no-warnings")
        .arg(&query)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(cookies) = crate::config::yt_cookies_file() {
        cmd.arg("--cookies").arg(cookies);
    }

    if use_proxy {
        if let Some(proxy) = crate::config::proxy_url() {
            cmd.arg("--proxy").arg(proxy);
        } else if let Some(addr) = crate::net::ipv6::random_source_addr() {
            cmd.arg("--force-ipv6").arg("--source-address").arg(addr);
        }
    }

    let output = cmd.output().await.map_err(|_| YtdlError::MissingBinary)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(YtdlError::InvocationFailed(stderr.trim().to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<FlatVideo>(line) {
            Ok(v) => results.push(v),
            Err(_) => continue,
        }
    }

    Ok(results)
}

fn flat_video_to_track(video: FlatVideo, source: TrackSource) -> Track {
    let video_id = video.id.unwrap_or_default();
    let webpage_url = video.webpage_url.or_else(|| {
        if video_id.is_empty() {
            None
        } else {
            Some(format!("https://www.youtube.com/watch?v={video_id}"))
        }
    });
    let url = video
        .url
        .unwrap_or_else(|| webpage_url.clone().unwrap_or_default());

    Track {
        id: Uuid::new_v4(),
        title: video.title.unwrap_or_default(),
        url,
        thumbnail: video.thumbnail,
        duration_ms: video.duration.map(|d| (d * 1000.0) as u64),
        source,
        original_url: webpage_url,
        artist: video.uploader,
    }
}

pub async fn resolve_query_to_tracks(
    query: &str,
    source: TrackSource,
) -> Result<Vec<Track>, YtdlError> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let trimmed = query.trim();
    let search_query = match source {
        TrackSource::Spotify => format!("{trimmed} (official audio)"),
        _ => format!("{trimmed} audio"),
    };

    info!(
        "ASTRALINK_YTDL_SEARCH source={:?} original_query=\"{}\" search_query=\"{}\"",
        source, trimmed, search_query
    );

    let cache_key = format!("{:?}:{}", source, search_query);
    if let Some(track) = TRACK_CACHE.get(&cache_key) {
        info!(
            "ASTRALINK_TRACK_CACHE_HIT source={:?} title=\"{}\" url={}",
            source, track.title, track.url
        );
        return Ok(vec![track]);
    }

    let count = match source {
        TrackSource::Youtube => 3,
        _ => 1,
    };

    let use_proxy = !matches!(source, TrackSource::Spotify);
    let videos = flat_search(&search_query, count, use_proxy).await?;

    if videos.is_empty() {
        return Ok(Vec::new());
    }

    let tracks: Vec<Track> = videos
        .into_iter()
        .map(|v| flat_video_to_track(v, source))
        .collect();

    for track in &tracks {
        info!(
            "ASTRALINK_TRACK_RESOLVED source={:?} id={} title=\"{}\" url={} original_url={:?} duration_ms={:?}",
            source, track.id, track.title, track.url, track.original_url, track.duration_ms
        );
    }

    let mut best = &tracks[0];
    let mut best_score = score_title_for_audio(&best.title, trimmed);

    for candidate in &tracks[1..] {
        let score = score_title_for_audio(&candidate.title, trimmed);
        if score > best_score {
            best = candidate;
            best_score = score;
        }
    }

    TRACK_CACHE.insert(cache_key, best.clone());

    Ok(vec![best.clone()])
}

#[cfg(test)]
mod tests {
    use super::score_title_for_audio;

    #[test]
    fn exact_title_match_beats_generic_audio_candidate() {
        let query = "juice wrld never understand me";
        let exact = "Juice WRLD - Never Understand Me ft. XXXTENTACION (Remix)";
        let generic = "Random Chill Mix (Official Audio)";

        assert!(score_title_for_audio(exact, query) > score_title_for_audio(generic, query));
    }

    #[test]
    fn title_with_missing_query_tokens_is_penalized() {
        let query = "juice wrld never understand me";
        let partial = "Juice WRLD - New Song";
        let closer = "Juice WRLD - Never Understand Me (Audio)";

        assert!(score_title_for_audio(closer, query) > score_title_for_audio(partial, query));
    }
}

pub async fn resolve_url_to_track(url: &str, source: TrackSource) -> Result<Vec<Track>, YtdlError> {
    if url.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut dl = YoutubeDl::new(url);
    if let Some(cookies) = crate::config::yt_cookies_file() {
        dl.extra_arg("--cookies");
        dl.extra_arg(cookies);
    }
    if let Some(proxy) = crate::config::proxy_url() {
        dl.extra_arg("--proxy");
        dl.extra_arg(proxy);
    } else if let Some(addr) = crate::net::ipv6::random_source_addr() {
        dl.extra_arg("--force-ipv6");
        dl.extra_arg("--source-address");
        dl.extra_arg(addr);
    }

    let output = dl.run_async().await.map_err(|err| match err {
        youtube_dl::Error::Io(_) => YtdlError::MissingBinary,
        other => YtdlError::InvocationFailed(other.to_string()),
    })?;

    let tracks = extract_tracks(output, source);

    for track in &tracks {
        info!(
            "ASTRALINK_TRACK_URL_RESOLVED source={:?} id={} title=\"{}\" url={} original_url={:?} duration_ms={:?}",
            source, track.id, track.title, track.url, track.original_url, track.duration_ms
        );
    }

    Ok(tracks)
}
