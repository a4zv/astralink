use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::OnceLock;

use std::error::Error as StdError;
use std::process::Command;
use thiserror::Error;

static PROXY_URL: OnceLock<String> = OnceLock::new();
static YT_COOKIES_FILE: OnceLock<String> = OnceLock::new();

pub fn init_proxy(url: String) {
    PROXY_URL.set(url).ok();
}

pub fn init_yt_cookies(path: String) {
    YT_COOKIES_FILE.set(path).ok();
}

pub fn proxy_url() -> Option<&'static str> {
    PROXY_URL.get().map(|s| s.as_str())
}

pub fn yt_cookies_file() -> Option<&'static str> {
    YT_COOKIES_FILE.get().map(|s| s.as_str())
}

pub fn sticky_proxy_url(session_id: &str) -> Option<String> {
    let base = PROXY_URL.get()?;
    let scheme_end = base.find("://").map(|i| i + 3).unwrap_or(0);
    let rest = &base[scheme_end..];
    if let Some(colon_pos) = rest.find(':') {
        let split_at = scheme_end + colon_pos;
        Some(format!(
            "{}-session-{}-sessionduration-10{}",
            &base[..split_at],
            session_id,
            &base[split_at..]
        ))
    } else {
        Some(base.clone())
    }
}

#[derive(Debug, Clone)]
pub struct AstralinkConfig {
    pub http_addr: SocketAddr,
    pub audio_addr: SocketAddr,
    pub token: String,
    pub spotify_client_id: Option<String>,
    pub spotify_client_secret: Option<String>,
    pub soundcloud_client_id: Option<String>,
    pub proxies: Vec<String>,
    pub redis_url: String,
    pub node_id: String,
    pub max_guilds_per_node: usize,
    pub node_host: Option<String>,
    pub ipv6_detection: bool,
    pub proxy_url: Option<String>,
    pub yt_cookies_file: Option<String>,
}

fn hostname() -> Result<String, Box<dyn StdError>> {
    let output = Command::new("hostname").output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid or missing ASTRALINK_TOKEN")]
    MissingToken,
    #[error("invalid HTTP listen addr: {0}")]
    InvalidHttpAddr(String),
    #[error("invalid audio listen addr: {0}")]
    InvalidAudioAddr(String),
}

impl AstralinkConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let http_addr =
            std::env::var("ASTRALINK_HTTP_ADDR").unwrap_or_else(|_| "0.0.0.0:9000".to_string());
        let audio_addr =
            std::env::var("ASTRALINK_AUDIO_ADDR").unwrap_or_else(|_| "0.0.0.0:9001".to_string());

        let http_addr = SocketAddr::from_str(&http_addr)
            .map_err(|_| ConfigError::InvalidHttpAddr(http_addr))?;
        let audio_addr = SocketAddr::from_str(&audio_addr)
            .map_err(|_| ConfigError::InvalidAudioAddr(audio_addr))?;

        let token = std::env::var("ASTRALINK_TOKEN").map_err(|_| ConfigError::MissingToken)?;

        let spotify_client_id = std::env::var("ASTRALINK_SPOTIFY_CLIENT_ID").ok();
        let spotify_client_secret = std::env::var("ASTRALINK_SPOTIFY_CLIENT_SECRET").ok();
        let soundcloud_client_id = std::env::var("ASTRALINK_SOUNDCLOUD_CLIENT_ID").ok();

        let proxies = std::env::var("ASTRALINK_PROXIES")
            .ok()
            .map(|value| {
                value
                    .split(',')
                    .filter_map(|raw| {
                        let trimmed = raw.trim();
                        if trimmed.is_empty() {
                            return None;
                        }
                        Some(trimmed.to_string())
                    })
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();

        let redis_url =
            std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string());

        let node_id = std::env::var("NODE_ID")
            .unwrap_or_else(|_| hostname().unwrap_or_else(|_| "default".to_string()));

        let max_guilds_per_node = std::env::var("MAX_GUILDS_PER_NODE")
            .unwrap_or_else(|_| "100".to_string())
            .parse()
            .unwrap_or(100);

        let node_host = std::env::var("NODE_HOST").ok();

        let ipv6_detection = std::env::var("ASTRALINK_IPV6_DETECTION")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);

        let proxy_url = std::env::var("ASTRALINK_PROXY_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());

        let yt_cookies_file = std::env::var("ASTRALINK_YT_COOKIES_FILE")
            .ok()
            .filter(|s| !s.trim().is_empty());

        Ok(Self {
            http_addr,
            audio_addr,
            token,
            spotify_client_id,
            spotify_client_secret,
            soundcloud_client_id,
            proxies,
            redis_url,
            node_id,
            max_guilds_per_node,
            node_host,
            ipv6_detection,
            proxy_url,
            yt_cookies_file,
        })
    }
}
