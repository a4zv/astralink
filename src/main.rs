mod api;
mod audio;
mod config;
mod model;
mod net;
mod resolve;
mod state;

use std::sync::Arc;

use axum::Router;
use dashmap::DashMap;
use tokio::net::TcpListener;
use tokio::signal;
use tracing_subscriber::EnvFilter;

use crate::api::{router, AppState};
use crate::config::AstralinkConfig;
use crate::model::NodeInfo;
use crate::state::AstralinkState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = AstralinkConfig::from_env()?;

    if let Some(url) = &config.proxy_url {
        config::init_proxy(url.clone());
        tracing::info!("ASTRALINK_PROXY_ENABLED");
    }

    if let Some(path) = &config.yt_cookies_file {
        config::init_yt_cookies(path.clone());
        tracing::info!("ASTRALINK_YT_COOKIES_ENABLED path={}", path);
    }

    if config.ipv6_detection {
        match net::ipv6::detect_and_setup() {
            Ok(()) => tracing::info!("ASTRALINK_IPV6_ENABLED"),
            Err(err) => tracing::warn!("ASTRALINK_IPV6_SETUP_FAILED error={err}"),
        }
    }

    let shared_config = Arc::new(config);

    let node_host = shared_config
        .node_host
        .clone()
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let node_info = NodeInfo {
        id: shared_config.node_id.clone(),
        host: node_host,
        http_port: shared_config.http_addr.port(),
        audio_port: shared_config.audio_addr.port(),
        available_slots: shared_config.max_guilds_per_node,
        total_slots: shared_config.max_guilds_per_node,
    };
    let _connected_guilds = Arc::new(DashMap::<crate::model::GuildId, ()>::new());
    let ws_broadcasters = Arc::new(DashMap::new());
    let state = Arc::new(AstralinkState::new(shared_config.node_id.clone()));

    let client = reqwest::Client::builder().build()?;

    let app_state = AppState {
        config: shared_config.clone(),
        http_client: client,
        state: state.clone(),
        ws_broadcasters: ws_broadcasters.clone(),
        node_info: node_info.clone(),
    };

    let app: Router = router().with_state(app_state);

    let http_addr = shared_config.http_addr;
    let audio_config = shared_config.clone();

    let http_task = tokio::spawn(async move {
        let listener = TcpListener::bind(http_addr).await?;
        tracing::info!("ASTRALINK_HTTP_LISTENING addr={}", http_addr);
        axum::serve(listener, app.into_make_service()).await?;
        Ok::<(), anyhow::Error>(())
    });

    let audio_task = tokio::spawn(async move {
        if let Err(err) = audio::start_audio_socket(audio_config, state, ws_broadcasters).await {
            tracing::error!("ASTRALINK_AUDIO_ERROR: {err}");
        }
    });

    let shutdown = async {
        let _ = signal::ctrl_c().await;
        tracing::info!("ASTRALINK_SHUTDOWN_SIGNAL");
    };

    tokio::select! {
        _ = http_task => {}
        _ = audio_task => {}
        _ = shutdown => {}
    }

    Ok(())
}
