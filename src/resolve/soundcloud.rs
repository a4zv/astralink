use serde_json::Value;
use thiserror::Error;
use tokio::process::Command;
use tracing::info;
use uuid::Uuid;

use crate::model::{Track, TrackSource};

#[derive(Debug, Error)]
pub enum SoundcloudError {
    #[error("soundcloud request failed: {0}")]
    RequestFailed(String),
}

fn is_url(value: &str) -> bool {
    let trimmed = value.trim().to_lowercase();
    trimmed.starts_with("http://") || trimmed.starts_with("https://")
}

fn is_soundcloud_url(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower.contains("soundcloud.com") || lower.contains("snd.sc")
}

fn is_go_premium_payload(payload: &Value) -> bool {
    let availability = payload
        .get("availability")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let duration = payload.get("duration").and_then(Value::as_u64);

    if availability.contains("premium") || availability.contains("subscriber_only") {
        return true;
    }
    if title.contains("go+") || title.contains("soundcloud go") {
        return true;
    }
    if description.contains("go+") || description.contains("go premium") {
        return true;
    }
    if duration == Some(30) {
        return true;
    }

    payload
        .get("formats")
        .and_then(Value::as_array)
        .map(|formats| {
            formats.iter().any(|fmt| {
                let format_note = fmt
                    .get("format_note")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase();
                let acodec = fmt
                    .get("acodec")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase();

                format_note.contains("preview") || acodec.contains("preview")
            })
        })
        .unwrap_or(false)
}

fn payload_to_track(payload: &Value, source: TrackSource) -> Option<Track> {
    if is_go_premium_payload(payload) {
        return None;
    }

    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let original_url = payload
        .get("webpage_url")
        .and_then(Value::as_str)
        .or_else(|| payload.get("original_url").and_then(Value::as_str))
        .map(|s| s.to_string());

    let url = original_url
        .clone()
        .or_else(|| {
            payload
                .get("url")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .unwrap_or_default();

    if url.is_empty() {
        return None;
    }

    let artist = payload
        .get("uploader")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let thumbnail = payload
        .get("thumbnail")
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    let duration_ms = payload
        .get("duration")
        .and_then(Value::as_f64)
        .filter(|d| *d > 0.0)
        .map(|d| (d * 1000.0) as u64);

    Some(Track {
        id: Uuid::new_v4(),
        title,
        url,
        thumbnail,
        duration_ms,
        source,
        original_url,
        artist,
    })
}

fn fallback_query(payload: Option<&Value>, original_query: &str) -> String {
    let artist = payload
        .and_then(|p| p.get("uploader"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = payload
        .and_then(|p| p.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    let combined = format!("{artist} {title}").trim().to_string();
    if combined.is_empty() {
        original_query.to_string()
    } else {
        combined
    }
}

async fn run_ytdlp_json(target: &str) -> Result<Value, SoundcloudError> {
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--dump-single-json")
        .arg("--no-warnings")
        .arg(target)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(cookies) = crate::config::yt_cookies_file() {
        cmd.arg("--cookies").arg(cookies);
    }
    if let Some(proxy) = crate::config::proxy_url() {
        cmd.arg("--proxy").arg(proxy);
    } else if let Some(addr) = crate::net::ipv6::random_source_addr() {
        cmd.arg("--force-ipv6").arg("--source-address").arg(addr);
    }

    let output = cmd
        .output()
        .await
        .map_err(|err| SoundcloudError::RequestFailed(err.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(SoundcloudError::RequestFailed(stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<Value>(&stdout)
        .map_err(|err| SoundcloudError::RequestFailed(err.to_string()))
}

async fn fallback_to_youtube(query: &str) -> Result<Vec<Track>, SoundcloudError> {
    info!(
        "ASTRALINK_SOUNDCLOUD_FALLBACK_TO_YOUTUBE query=\"{}\"",
        query
    );

    super::ytdl::resolve_query_to_tracks(query, TrackSource::Youtube)
        .await
        .map_err(|err| SoundcloudError::RequestFailed(err.to_string()))
}

pub async fn resolve_soundcloud_query(
    query: &str,
    _client_id: Option<&str>,
) -> Result<Vec<Track>, SoundcloudError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let target = if is_url(trimmed) {
        if !is_soundcloud_url(trimmed) {
            return fallback_to_youtube(trimmed).await;
        }
        trimmed.to_string()
    } else {
        format!("scsearch1:{trimmed}")
    };

    let payload = run_ytdlp_json(&target).await;

    let Ok(payload) = payload else {
        return fallback_to_youtube(trimmed).await;
    };

    let primary = payload
        .get("entries")
        .and_then(Value::as_array)
        .and_then(|entries| entries.first())
        .unwrap_or(&payload);

    if let Some(track) = payload_to_track(primary, TrackSource::Soundcloud) {
        info!(
            "ASTRALINK_TRACK_RESOLVED source={:?} id={} title=\"{}\" url={} original_url={:?} duration_ms={:?}",
            track.source, track.id, track.title, track.url, track.original_url, track.duration_ms
        );
        return Ok(vec![track]);
    }

    let yt_query = fallback_query(Some(primary), trimmed);
    fallback_to_youtube(&yt_query).await
}
