use std::io;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::config::AstralinkConfig;
use crate::model::{GuildId, StateResponse};
use crate::state::AstralinkState;

pub type Frame = Vec<u8>;

#[derive(Debug)]
struct PlaybackHandle {
    join: JoinHandle<()>,
    frames: broadcast::Sender<Frame>,
}

#[derive(Debug)]
pub struct PlaybackRegistry {
    guilds: DashMap<GuildId, PlaybackHandle>,
}

impl PlaybackRegistry {
    pub fn new() -> Self {
        Self {
            guilds: DashMap::new(),
        }
    }

    pub fn subscribe(&self, guild_id: GuildId) -> Option<broadcast::Receiver<Frame>> {
        self.guilds.get(&guild_id).map(|h| h.frames.subscribe())
    }

    pub fn ensure_guild_playback(
        &self,
        guild_id: GuildId,
        state: Arc<AstralinkState>,
        config: Arc<AstralinkConfig>,
        ws_broadcasters: Arc<DashMap<GuildId, broadcast::Sender<StateResponse>>>,
        connected: Arc<DashMap<GuildId, ()>>,
    ) {
        if self.guilds.contains_key(&guild_id) {
            return;
        }

        let (tx, _rx) = broadcast::channel::<Frame>(512);
        let state_clone = state.clone();

        let tx_for_task = tx.clone();

        let ws_broadcasters_for_task = ws_broadcasters.clone();

        let connected_for_task = connected.clone();

        let join = tokio::spawn(async move {
            loop {
                let Some(track) = state_clone.current_for(guild_id).await else {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                };

                info!(
                    "ASTRALINK_PLAYBACK_START guild_id={} title=\"{}\" url={}",
                    guild_id, track.title, track.url
                );

                let mut retries = 0;
                let mut ytdlp_child = None;
                loop {
                    retries += 1;
                    if retries > 3 {
                        error!(
                            "ASTRALINK_YTDLP_FAILED_AFTER_RETRIES guild_id={} url={}",
                            guild_id, track.url
                        );
                        let next_opt = state_clone.advance_after_track_end(guild_id).await;
                        if next_opt.is_none() {
                            connected_for_task.remove(&guild_id);
                            let _ = state_clone.unassign_node(guild_id).await;
                            info!("ASTRALINK_UNASSIGN_DUE_TO_SKIP guild_id={}", guild_id);
                        }
                        break;
                    }
                    let mut ytdlp = Command::new("yt-dlp");
                    ytdlp
                        .arg("--no-playlist")
                        .arg("-f")
                        .arg("bestaudio")
                        .arg("-o")
                        .arg("-")
                        .arg(&track.url)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null());
                    if let Some(proxy) = crate::config::proxy_url() {
                        ytdlp.arg("--proxy").arg(proxy);
                    } else if let Some(addr) = crate::net::ipv6::random_source_addr() {
                        ytdlp.arg("--force-ipv6").arg("--source-address").arg(addr);
                    } else if !config.proxies.is_empty() && retries < 3 {
                        let proxy_idx = (retries - 1) % config.proxies.len();
                        ytdlp.arg("--proxy").arg(&config.proxies[proxy_idx]);
                    }
                    match ytdlp.spawn() {
                        Ok(child) => {
                            ytdlp_child = Some(child);
                            break;
                        }
                        Err(err) => {
                            error!(
                                "ASTRALINK_YTDLP_SPAWN_ERROR guild_id={} error={} retry={}",
                                guild_id, err, retries
                            );
                            continue;
                        }
                    }
                }
                let mut ytdlp_child = match ytdlp_child {
                    Some(child) => child,
                    None => continue,
                };

                let mut ytdlp_stdout = match ytdlp_child.stdout.take() {
                    Some(stdout) => stdout,
                    None => {
                        error!("ASTRALINK_YTDLP_MISSING_STDOUT guild_id={}", guild_id);
                        break;
                    }
                };

                let mut ffmpeg_retries = 0;
                let mut ffmpeg_child = None;
                loop {
                    ffmpeg_retries += 1;
                    if ffmpeg_retries > 3 {
                        error!(
                            "ASTRALINK_FFMPEG_FAILED_AFTER_RETRIES guild_id={} url={}",
                            guild_id, track.url
                        );
                        let next_opt = state_clone.advance_after_track_end(guild_id).await;
                        if next_opt.is_none() {
                            connected_for_task.remove(&guild_id);
                            let _ = state_clone.unassign_node(guild_id).await;
                            info!("ASTRALINK_UNASSIGN_DUE_TO_SKIP guild_id={}", guild_id);
                        }
                        break;
                    }
                    let mut ffmpeg = Command::new("ffmpeg");
                    ffmpeg
                        .arg("-re")
                        .arg("-i")
                        .arg("pipe:0")
                        .arg("-f")
                        .arg("s16le")
                        .arg("-ac")
                        .arg("2")
                        .arg("-ar")
                        .arg("48000")
                        .arg("pipe:1")
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null());
                    match ffmpeg.spawn() {
                        Ok(child) => {
                            ffmpeg_child = Some(child);
                            break;
                        }
                        Err(err) => {
                            error!(
                                "ASTRALINK_FFMPEG_SPAWN_ERROR guild_id={} error={} retry={}",
                                guild_id, err, ffmpeg_retries
                            );
                            continue;
                        }
                    }
                }
                let mut ffmpeg_child = match ffmpeg_child {
                    Some(child) => child,
                    None => continue,
                };

                let mut ffmpeg_stdin = match ffmpeg_child.stdin.take() {
                    Some(stdin) => stdin,
                    None => {
                        error!("ASTRALINK_FFMPEG_MISSING_STDIN guild_id={}", guild_id);
                        break;
                    }
                };

                let mut ffmpeg_stdout = match ffmpeg_child.stdout.take() {
                    Some(stdout) => stdout,
                    None => {
                        error!("ASTRALINK_FFMPEG_MISSING_STDOUT guild_id={}", guild_id);
                        break;
                    }
                };

                let pump = tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    loop {
                        match ytdlp_stdout.read(&mut buf).await {
                            Ok(n) if n == 0 => break,
                            Ok(n) => {
                                if let Err(e) = ffmpeg_stdin.write_all(&buf[..n]).await {
                                    error!(
                                        "ASTRALINK_PUMP_WRITE_ERROR guild_id={} error={}",
                                        guild_id, e
                                    );
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(
                                    "ASTRALINK_YTDLP_READ_ERROR guild_id={} error={}",
                                    guild_id, e
                                );
                                break;
                            }
                        }
                    }
                    let _ = ffmpeg_stdin.shutdown().await;
                    match ytdlp_child.wait().await {
                        Ok(status) if !status.success() => {
                            error!(
                                "ASTRALINK_YTDLP_EXITED guild_id={} status={}",
                                guild_id, status
                            );
                        }
                        Ok(_) => {}
                        Err(e) => error!(
                            "ASTRALINK_YTDLP_WAIT_ERROR guild_id={} error={}",
                            guild_id, e
                        ),
                    }
                    Ok::<(), anyhow::Error>(())
                });

                let mut frame_buf = vec![0u8; 3840];

                loop {
                    match ffmpeg_stdout.read_exact(&mut frame_buf).await {
                        Ok(_) => {
                            let _ = tx_for_task.send(frame_buf.clone());
                        }
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            break;
                        }
                        Err(e) => {
                            error!(
                                "ASTRALINK_FFMPEG_READ_ERROR guild_id={} error={}",
                                guild_id, e
                            );
                            let mut read_retries = 0;
                            loop {
                                read_retries += 1;
                                if read_retries > 3 {
                                    error!(
                                        "ASTRALINK_FFMPEG_READ_FAILED_AFTER_RETRIES guild_id={}",
                                        guild_id
                                    );
                                    break;
                                }
                                match ffmpeg_stdout.read_exact(&mut frame_buf).await {
                                    Ok(_) => {
                                        let _ = tx_for_task.send(frame_buf.clone());
                                        break;
                                    }
                                    Err(e2) if e2.kind() == io::ErrorKind::UnexpectedEof => {
                                        break;
                                    }
                                    Err(_) => {
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                        continue;
                                    }
                                }
                            }
                            if read_retries > 3 {
                                let next_opt = state_clone.advance_after_track_end(guild_id).await;
                                if next_opt.is_none() {
                                    connected_for_task.remove(&guild_id);
                                    let _ = state_clone.unassign_node(guild_id).await;
                                    info!("ASTRALINK_UNASSIGN_DUE_TO_SKIP guild_id={}", guild_id);
                                    break;
                                }
                                break;
                            } else {
                                continue;
                            }
                        }
                    }
                }

                match pump.await {
                    Ok(_) => {}
                    Err(e) => {
                        error!("ASTRALINK_PUMP_PANIC guild_id={} error={:?}", guild_id, e);
                        let next_opt = state_clone.advance_after_track_end(guild_id).await;
                        if next_opt.is_none() {
                            connected_for_task.remove(&guild_id);
                            let _ = state_clone.unassign_node(guild_id).await;
                            info!("ASTRALINK_UNASSIGN_DUE_TO_SKIP guild_id={}", guild_id);
                            break;
                        }
                        continue;
                    }
                }

                match ffmpeg_child.wait().await {
                    Ok(status) if !status.success() => {
                        error!(
                            "ASTRALINK_FFMPEG_EXITED guild_id={} status={}",
                            guild_id, status
                        );
                    }
                    Ok(_) => {}
                    Err(e) => error!(
                        "ASTRALINK_FFMPEG_WAIT_ERROR guild_id={} error={}",
                        guild_id, e
                    ),
                }

                let next_opt = state_clone.advance_after_track_end(guild_id).await;
                let after_state = state_clone.as_response(guild_id).await;
                let ws_tx = ws_broadcasters_for_task
                    .entry(guild_id)
                    .or_insert_with(|| broadcast::channel::<StateResponse>(100).0)
                    .clone();
                let _ = ws_tx.send(after_state);

                if next_opt.is_none() {
                    connected_for_task.remove(&guild_id);
                    let _ = state_clone.unassign_node(guild_id).await;
                    info!("ASTRALINK_UNASSIGN_DUE_TO_END guild_id={}", guild_id);
                    break;
                }
            }
        });

        self.guilds
            .insert(guild_id, PlaybackHandle { join, frames: tx });
    }
}
