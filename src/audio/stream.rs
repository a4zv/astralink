use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::time::{timeout, Duration};
use tracing::{debug, error, info};

use crate::config::AstralinkConfig;
use crate::model::{GuildId, StateResponse};
use crate::state::AstralinkState;

#[derive(Debug, Deserialize)]
struct AudioHandshake {
    token: String,
    guild_id: GuildId,
}

pub async fn start_audio_socket(
    config: Arc<AstralinkConfig>,
    state: Arc<AstralinkState>,
    ws_broadcasters: Arc<DashMap<GuildId, broadcast::Sender<StateResponse>>>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.audio_addr).await?;
    info!("ASTRALINK_AUDIO_LISTENING addr={}", config.audio_addr);

    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            continue;
        };

        let cfg = config.clone();
        let shared_state = state.clone();
        let ws_map = ws_broadcasters.clone();

        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, peer, cfg, shared_state, ws_map).await {
                error!("ASTRALINK_AUDIO_CONN_ERROR: {err}");
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    peer: SocketAddr,
    config: Arc<AstralinkConfig>,
    state: Arc<AstralinkState>,
    ws_broadcasters: Arc<DashMap<GuildId, broadcast::Sender<StateResponse>>>,
) -> anyhow::Result<()> {
    info!("ASTRALINK_AUDIO_ACCEPTED peer={}", peer);

    let (reader_half, mut writer_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(reader_half);

    let mut line = String::new();
    info!("ASTRALINK_WAITING_FOR_HANDSHAKE peer={}", peer);

    let n = timeout(Duration::from_secs(3), reader.read_line(&mut line)).await??;

    info!("ASTRALINK_HANDSHAKE_RECEIVED bytes={}", n);
    debug!("ASTRALINK_HANDSHAKE_RAW bytes={} data={:?}", n, line);

    if n == 0 {
        info!("ASTRALINK_AUDIO_NO_HANDSHAKE peer={}", peer);
        return Ok(());
    }

    let handshake: AudioHandshake = serde_json::from_str(&line)?;
    debug!("ASTRALINK_HANDSHAKE_PARSED {:?}", handshake);

    if handshake.token != config.token {
        info!("ASTRALINK_AUDIO_BAD_TOKEN peer={}", peer);
        return Ok(());
    }

    let guild_id = handshake.guild_id;

    info!(
        "ASTRALINK_AUDIO_HANDSHAKE_OK peer={} guild_id={}",
        peer, guild_id
    );

    loop {
        let mut wait_ms = 0u64;
        let track = loop {
            if let Some(t) = state.current_for(guild_id).await {
                break t;
            }
            if wait_ms >= 30_000 {
                info!(
                    "ASTRALINK_AUDIO_NO_CURRENT_TRACK_TIMEOUT guild_id={} waited_ms={}",
                    guild_id, wait_ms
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            wait_ms += 100;
        };

        let current_track_id = track.id;
        let stream_started_at = Instant::now();

        info!(
            "ASTRALINK_AUDIO_START_STREAM guild_id={} title=\"{}\" url={}",
            guild_id, track.title, track.url
        );

        let resolve_playback_started = Instant::now();
        let playback_url = match crate::resolve::ytdl::resolve_playback_url(&track.url).await {
            Ok(url) => {
                info!(
                    "ASTRALINK_AUDIO_PLAYBACK_URL_READY guild_id={} track_id={} elapsed_ms={}",
                    guild_id,
                    current_track_id,
                    resolve_playback_started.elapsed().as_millis()
                );
                url
            }
            Err(err) => {
                error!(
                    "ASTRALINK_AUDIO_PLAYBACK_URL_FAILED guild_id={} track_id={} error={} elapsed_ms={}",
                    guild_id,
                    current_track_id,
                    err,
                    resolve_playback_started.elapsed().as_millis()
                );
                track.url.clone()
            }
        };

        let ytdlp_started = Instant::now();
        let mut ytdlp_cmd = Command::new("yt-dlp");
        ytdlp_cmd
            .arg("-f")
            .arg("bestaudio[abr<=128]/bestaudio/best")
            .arg("--no-playlist")
            .arg("--socket-timeout")
            .arg("8")
            .arg("--retries")
            .arg("2")
            .arg("--fragment-retries")
            .arg("2")
            .arg("--no-part")
            .arg("-o")
            .arg("-")
            .arg(&playback_url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cookies) = crate::config::yt_cookies_file() {
            ytdlp_cmd.arg("--cookies").arg(cookies);
        }
        let session_id = current_track_id.to_string().replace('-', "");
        if let Some(proxy) = crate::config::sticky_proxy_url(&session_id) {
            ytdlp_cmd.arg("--proxy").arg(proxy);
        } else if let Some(addr) = crate::net::ipv6::random_source_addr() {
            ytdlp_cmd
                .arg("--force-ipv6")
                .arg("--source-address")
                .arg(addr);
        }
        let mut ytdlp_child = ytdlp_cmd.spawn()?;
        info!(
            "ASTRALINK_AUDIO_YTDLP_SPAWNED guild_id={} track_id={} elapsed_ms={}",
            guild_id,
            current_track_id,
            ytdlp_started.elapsed().as_millis()
        );

        let mut ytdlp_stdout = ytdlp_child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing yt-dlp stdout"))?;

        let mut ytdlp_stderr = ytdlp_child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing yt-dlp stderr"))?;

        let ytdlp_stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut ytdlp_stderr, &mut buf).await;
            buf
        });

        let ffmpeg_started = Instant::now();
        let mut ffmpeg_child = Command::new("ffmpeg")
            .arg("-loglevel")
            .arg("error")
            .arg("-hide_banner")
            .arg("-fflags")
            .arg("nobuffer")
            .arg("-flags")
            .arg("low_delay")
            .arg("-analyzeduration")
            .arg("0")
            .arg("-probesize")
            .arg("32768")
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
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        info!(
            "ASTRALINK_AUDIO_FFMPEG_SPAWNED guild_id={} track_id={} elapsed_ms={}",
            guild_id,
            current_track_id,
            ffmpeg_started.elapsed().as_millis()
        );

        let mut ffmpeg_stdin = ffmpeg_child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing ffmpeg stdin"))?;

        let mut ffmpeg_stdout = ffmpeg_child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing ffmpeg stdout"))?;

        let pump = tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                let n = ytdlp_stdout.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                ffmpeg_stdin.write_all(&buf[..n]).await?;
            }
            ffmpeg_stdin.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        });

        let mut pcm_buffer = [0u8; 3840];
        let mut frames = 0u64;
        let mut first_frame_logged = false;

        let mut track_finished_naturally = false;

        loop {
            if let Some(cur) = state.current_for(guild_id).await {
                if cur.id != current_track_id {
                    info!(
                        "ASTRALINK_AUDIO_TRACK_CHANGED guild_id={} old_id={} new_id={}",
                        guild_id, current_track_id, cur.id
                    );
                    break;
                }
            } else {
                info!(
                    "ASTRALINK_AUDIO_NO_CURRENT guild_id={} current_id={}",
                    guild_id, current_track_id
                );
                break;
            }

            match ffmpeg_stdout.read_exact(&mut pcm_buffer).await {
                Ok(_) => {
                    frames += 1;
                    if !first_frame_logged {
                        first_frame_logged = true;
                        info!(
                            "ASTRALINK_AUDIO_FIRST_FRAME guild_id={} track_id={} startup_ms={}",
                            guild_id,
                            current_track_id,
                            stream_started_at.elapsed().as_millis()
                        );
                    }
                    writer_half.write_all(&pcm_buffer).await?;
                    let _ = state.increment_position_ms(guild_id, 20).await;
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    info!("ASTRALINK_FFMPEG_EOF frames={}", frames);
                    track_finished_naturally = true;
                    break;
                }
                Err(e) => {
                    error!("ASTRALINK_PCM_READ_ERROR {}", e);
                    return Err(e.into());
                }
            }
        }

        if !track_finished_naturally {
            let _ = ffmpeg_child.kill().await;
            let _ = ytdlp_child.kill().await;
        }

        if let Err(e) = pump.await {
            error!("ASTRALINK_PUMP_TASK_FAILED {:?}", e);
        }

        let ffmpeg_status = ffmpeg_child.wait().await?;
        info!("ASTRALINK_FFMPEG_EXIT status={}", ffmpeg_status);

        let ytdlp_status = ytdlp_child.wait().await?;
        let ytdlp_stderr_output = ytdlp_stderr_task.await.unwrap_or_default();
        if !ytdlp_status.success() {
            error!(
                "ASTRALINK_YTDLP_EXIT status={} stderr={}",
                ytdlp_status,
                ytdlp_stderr_output.trim()
            );
        }

        if frames == 0 && playback_url != track.url {
            crate::resolve::ytdl::cache_playback_url(&track.url, &track.url);
            info!(
                "ASTRALINK_AUDIO_PLAYBACK_URL_BYPASS guild_id={} track_id={} reason=no_frames",
                guild_id, current_track_id
            );
            continue;
        }

        if track_finished_naturally {
            let _ = state.advance_after_track_end(guild_id).await;
            let response_state = state.as_response(guild_id).await;
            let ws_tx = ws_broadcasters
                .entry(guild_id)
                .or_insert_with(|| broadcast::channel::<StateResponse>(100).0)
                .clone();
            let _ = ws_tx.send(response_state);

            crate::resolve::preload::trigger_preload(state.clone(), guild_id, config.clone());
        }

        info!(
            "ASTRALINK_AUDIO_STREAM_DONE guild_id={} track_id={} frames={} elapsed_ms={}",
            guild_id,
            current_track_id,
            frames,
            stream_started_at.elapsed().as_millis()
        );
    }
}
