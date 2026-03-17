use std::sync::Mutex;
use std::time::Instant;

use serde::Deserialize;
use thiserror::Error;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

use crate::model::{Track, TrackSource};

struct CachedToken {
    token: String,
    expires_at: Instant,
}

static TOKEN_CACHE: Mutex<Option<CachedToken>> = Mutex::new(None);

#[derive(Debug, Error)]
pub enum SpotifyError {
    #[error("spotify is not configured")]
    NotConfigured,
    #[error("spotify request failed: {0}")]
    RequestFailed(String),
    #[error("spotify response parse error: {0}")]
    ParseError(String),
}

#[derive(Debug, Deserialize)]
struct SpotifyTrack {
    name: String,
    #[serde(default)]
    artists: Vec<SpotifyArtist>,
    #[serde(default)]
    album: Option<SpotifyAlbum>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    external_urls: Option<SpotifyExternalUrls>,
}

#[derive(Debug, Deserialize)]
struct SpotifyArtist {
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyAlbum {
    #[serde(default)]
    images: Vec<SpotifyImage>,
}

#[derive(Debug, Deserialize)]
struct SpotifyImage {
    url: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyExternalUrls {
    #[serde(default)]
    spotify: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpotifySearchResponse {
    tracks: Option<SpotifyTracksPage>,
}

#[derive(Debug, Deserialize)]
struct SpotifyTracksPage {
    items: Vec<SpotifyTrack>,
}

async fn get_spotify_token(client_id: &str, client_secret: &str) -> Result<String, SpotifyError> {
    if let Ok(guard) = TOKEN_CACHE.lock() {
        if let Some(cached) = guard.as_ref() {
            if cached.expires_at > Instant::now() {
                return Ok(cached.token.clone());
            }
        }
    }

    let credentials = format!("{client_id}:{client_secret}");
    let encoded = STANDARD.encode(credentials.as_bytes());
    let auth = format!("Basic {encoded}");

    let client = reqwest::Client::new();

    let token_resp = client
        .post("https://accounts.spotify.com/api/token")
        .header("Authorization", auth)
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .map_err(|err| SpotifyError::RequestFailed(err.to_string()))?;

    if !token_resp.status().is_success() {
        return Err(SpotifyError::RequestFailed(token_resp.status().to_string()));
    }

    #[derive(Deserialize)]
    struct TokenBody {
        access_token: String,
        expires_in: Option<u64>,
    }

    let token_body: TokenBody = token_resp
        .json()
        .await
        .map_err(|err| SpotifyError::ParseError(err.to_string()))?;

    let ttl = token_body.expires_in.unwrap_or(3600).saturating_sub(60);
    if let Ok(mut guard) = TOKEN_CACHE.lock() {
        *guard = Some(CachedToken {
            token: token_body.access_token.clone(),
            expires_at: Instant::now() + std::time::Duration::from_secs(ttl),
        });
    }

    Ok(token_body.access_token)
}

fn apply_spotify_metadata(
    track: &mut Track,
    name: &str,
    artist: &str,
    thumbnail: Option<&str>,
    duration_ms: Option<u64>,
    spotify_url: Option<&str>,
) {
    track.title = name.to_string();
    if !artist.is_empty() {
        track.artist = Some(artist.to_string());
    }
    if let Some(orig) = track.original_url.clone() {
        track.url = orig;
    }
    if let Some(thumb) = thumbnail {
        track.thumbnail = Some(thumb.to_string());
    }
    if let Some(dur) = duration_ms {
        track.duration_ms = Some(dur);
    }
    if let Some(url) = spotify_url {
        track.original_url = Some(url.to_string());
    }
    track.source = TrackSource::Spotify;
}

fn extract_spotify_track_id(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("spotify:track:") {
        let id = rest.split(|c| c == '?' || c == '&' || c == '/').next()?;
        if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        }
    } else if let Some(idx) = url.find("track/") {
        let rest = &url[idx + "track/".len()..];
        let id = rest
            .split(|c| c == '?' || c == '&' || c == '/')
            .next()
            .unwrap_or_default();
        if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        }
    } else {
        None
    }
}

pub async fn resolve_spotify_query(
    query: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<Vec<Track>, SpotifyError> {
    let client_id = client_id.ok_or(SpotifyError::NotConfigured)?;
    let client_secret = client_secret.ok_or(SpotifyError::NotConfigured)?;

    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    let client = reqwest::Client::new();

    let access_token = get_spotify_token(client_id, client_secret).await?;

    let search_resp = client
        .get("https://api.spotify.com/v1/search")
        .bearer_auth(access_token)
        .query(&[("q", query), ("type", "track"), ("limit", "5")])
        .send()
        .await
        .map_err(|err| SpotifyError::RequestFailed(err.to_string()))?;

    if !search_resp.status().is_success() {
        return Err(SpotifyError::RequestFailed(
            search_resp.status().to_string(),
        ));
    }

    let body: SpotifySearchResponse = search_resp
        .json()
        .await
        .map_err(|err| SpotifyError::ParseError(err.to_string()))?;

    let Some(page) = body.tracks else {
        return Ok(Vec::new());
    };

    let mut join_set = tokio::task::JoinSet::new();

    for spotify_track in page.items.into_iter() {
        join_set.spawn(async move {
            let artist_name = spotify_track
                .artists
                .first()
                .map(|a| a.name.as_str())
                .unwrap_or_default()
                .to_string();
            let query_string = format!("{artist_name} - {}", spotify_track.name);

            let resolved =
                super::ytdl::resolve_query_to_tracks(&query_string, TrackSource::Spotify)
                    .await
                    .map_err(|err| SpotifyError::RequestFailed(err.to_string()))?;

            Ok::<(SpotifyTrack, String, Vec<Track>), SpotifyError>((
                spotify_track,
                artist_name,
                resolved,
            ))
        });
    }

    let mut tracks = Vec::new();

    while let Some(result) = join_set.join_next().await {
        let Ok(Ok((spotify_track, artist_name, mut resolved))) = result else {
            continue;
        };
        let spotify_url = spotify_track
            .external_urls
            .as_ref()
            .and_then(|e| e.spotify.as_ref())
            .map(|s| s.as_str());
        let spotify_thumbnail = spotify_track
            .album
            .as_ref()
            .and_then(|album| album.images.first())
            .map(|img| img.url.as_str());

        for mut yt_track in resolved.drain(..) {
            apply_spotify_metadata(
                &mut yt_track,
                &spotify_track.name,
                &artist_name,
                spotify_thumbnail,
                spotify_track.duration_ms,
                spotify_url,
            );
            tracks.push(yt_track);
        }
    }

    Ok(tracks)
}

pub async fn resolve_spotify_url(
    url: &str,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<Vec<Track>, SpotifyError> {
    let client_id = client_id.ok_or(SpotifyError::NotConfigured)?;
    let client_secret = client_secret.ok_or(SpotifyError::NotConfigured)?;

    if url.trim().is_empty() {
        return Ok(Vec::new());
    }

    let track_id = extract_spotify_track_id(url)
        .ok_or_else(|| SpotifyError::RequestFailed("invalid spotify track url".to_string()))?;

    let client = reqwest::Client::new();
    let access_token = get_spotify_token(client_id, client_secret).await?;

    let track_resp = client
        .get(&format!("https://api.spotify.com/v1/tracks/{}", track_id))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|err| SpotifyError::RequestFailed(err.to_string()))?;

    if !track_resp.status().is_success() {
        return Err(SpotifyError::RequestFailed(track_resp.status().to_string()));
    }

    let track: SpotifyTrack = track_resp
        .json()
        .await
        .map_err(|err| SpotifyError::ParseError(err.to_string()))?;

    let artist_name = track
        .artists
        .first()
        .map(|a| a.name.as_str())
        .unwrap_or_default();
    let query_string = format!("{artist_name} - {}", track.name);
    let spotify_url = track
        .external_urls
        .as_ref()
        .and_then(|e| e.spotify.as_ref())
        .map(|s| s.as_str())
        .or(Some(url));
    let spotify_thumbnail = track
        .album
        .as_ref()
        .and_then(|album| album.images.first())
        .map(|img| img.url.as_str());

    let mut resolved = super::ytdl::resolve_query_to_tracks(&query_string, TrackSource::Spotify)
        .await
        .map_err(|err| SpotifyError::RequestFailed(err.to_string()))?;

    let mut tracks = Vec::new();

    for mut yt_track in resolved.drain(..) {
        apply_spotify_metadata(
            &mut yt_track,
            &track.name,
            artist_name,
            spotify_thumbnail,
            track.duration_ms,
            spotify_url,
        );
        tracks.push(yt_track);
    }

    Ok(tracks)
}
