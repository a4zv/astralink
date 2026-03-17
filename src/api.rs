use std::sync::Arc;

use axum::extract::{
    ws::{Message, WebSocket, WebSocketUpgrade},
    Path, State,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde_json;
use tokio::sync::broadcast;
use tracing::error;

use crate::config::AstralinkConfig;
use crate::model::NodesResponse;
use crate::model::{
    ActionRequest, ActionResponse, EnqueueRequest, EnqueueResponse, GuildId, NodeInfo, QueueItem,
    ResolveRequest, ResolveResponse, StateResponse, TrackSource,
};
use crate::resolve::soundcloud::resolve_soundcloud_query;
use crate::resolve::spotify::{resolve_spotify_query, resolve_spotify_url};
use crate::resolve::ytdl::{resolve_query_to_tracks, resolve_url_to_track};
use crate::state::AstralinkState;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AstralinkConfig>,
    pub http_client: reqwest::Client,
    pub state: Arc<AstralinkState>,
    pub ws_broadcasters: Arc<DashMap<GuildId, broadcast::Sender<StateResponse>>>,
    pub node_info: NodeInfo,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/sessions/:guild_id/queue/resolve",
            post(handle_resolve_queue),
        )
        .route(
            "/sessions/:guild_id/queue/enqueue",
            post(handle_enqueue_queue),
        )
        .route("/sessions/:guild_id/state", get(handle_get_state))
        .route("/sessions/:guild_id/actions", post(handle_actions))
        .route("/sessions/:guild_id/ws", get(ws_handler))
        .route("/nodes", get(handle_nodes))
}

async fn handle_resolve_queue(
    Path(guild_id): Path<GuildId>,
    State(app): State<AppState>,
    Json(body): Json<ResolveRequest>,
) -> impl IntoResponse {
    let query = body.query.trim();

    if query.is_empty() {
        let response = ResolveResponse { tracks: Vec::new() };
        return (StatusCode::OK, Json(response));
    }

    let source_hint = body
        .source
        .unwrap_or(crate::model::ResolveSourceHint::Youtube);

    let is_http_url = query.starts_with("http://") || query.starts_with("https://");
    let is_spotify_url = is_http_url && query.contains("open.spotify.com");

    let tracks = match source_hint {
        crate::model::ResolveSourceHint::Youtube if is_http_url => {
            resolve_url_to_track(query, TrackSource::Youtube).await
        }
        crate::model::ResolveSourceHint::Youtube => {
            resolve_query_to_tracks(query, TrackSource::Youtube).await
        }
        crate::model::ResolveSourceHint::Spotify if is_spotify_url => resolve_spotify_url(
            query,
            app.config.spotify_client_id.as_deref(),
            app.config.spotify_client_secret.as_deref(),
        )
        .await
        .map_err(|err| crate::resolve::ytdl::YtdlError::InvocationFailed(err.to_string())),
        crate::model::ResolveSourceHint::Spotify => resolve_spotify_query(
            query,
            app.config.spotify_client_id.as_deref(),
            app.config.spotify_client_secret.as_deref(),
        )
        .await
        .map_err(|err| crate::resolve::ytdl::YtdlError::InvocationFailed(err.to_string())),
        crate::model::ResolveSourceHint::Soundcloud => {
            resolve_soundcloud_query(query, app.config.soundcloud_client_id.as_deref())
                .await
                .map_err(|err| crate::resolve::ytdl::YtdlError::InvocationFailed(err.to_string()))
        }
    };

    let tracks = match tracks {
        Ok(tracks) => tracks,
        Err(err) => {
            error!(
                "ASTRALINK_RESOLVE_FAILED guild_id={} source={:?} query=\"{}\" error=\"{}\"",
                guild_id, source_hint, query, err
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ResolveResponse { tracks: Vec::new() }),
            );
        }
    };

    let response = ResolveResponse { tracks };
    (StatusCode::OK, Json(response))
}

async fn handle_enqueue_queue(
    Path(guild_id): Path<GuildId>,
    State(app): State<AppState>,
    Json(body): Json<EnqueueRequest>,
) -> Response {
    if body.query.trim().is_empty() {
        let current_state = app.state.as_response(guild_id).await;
        return (
            StatusCode::OK,
            Json(EnqueueResponse {
                state: current_state,
                success: true,
                message: None,
                node: Some(app.node_info.clone()),
            }),
        )
        .into_response();
    }

    let full_state = app.state.get_or_init(guild_id).await;

    let mut state = full_state.clone();
    if state.is_finished {
        state.is_finished = false;
        state.current = None;
        state.queue.clear();
        state.position_ms = 0;
        let _ = app.state.set_state_async(&state).await;
    }
    if state.node.is_none() {
        let node_info = app.node_info.clone();
        state.node = Some(node_info.clone());
        app.state.assign_node(guild_id, node_info);
        let _ = app.state.set_state_async(&state).await;
    }

    let source_hint = body
        .source
        .unwrap_or(crate::model::ResolveSourceHint::Youtube);
    let query = body.query.trim();

    let is_http_url = query.starts_with("http://") || query.starts_with("https://");
    let is_spotify_url = is_http_url && query.contains("open.spotify.com");

    let tracks = match source_hint {
        crate::model::ResolveSourceHint::Youtube if is_http_url => {
            resolve_url_to_track(query, TrackSource::Youtube).await
        }
        crate::model::ResolveSourceHint::Youtube => {
            resolve_query_to_tracks(query, TrackSource::Youtube).await
        }
        crate::model::ResolveSourceHint::Spotify if is_spotify_url => resolve_spotify_url(
            query,
            app.config.spotify_client_id.as_deref(),
            app.config.spotify_client_secret.as_deref(),
        )
        .await
        .map_err(|err| crate::resolve::ytdl::YtdlError::InvocationFailed(err.to_string())),
        crate::model::ResolveSourceHint::Spotify => resolve_spotify_query(
            query,
            app.config.spotify_client_id.as_deref(),
            app.config.spotify_client_secret.as_deref(),
        )
        .await
        .map_err(|err| crate::resolve::ytdl::YtdlError::InvocationFailed(err.to_string())),
        crate::model::ResolveSourceHint::Soundcloud => {
            resolve_soundcloud_query(query, app.config.soundcloud_client_id.as_deref())
                .await
                .map_err(|err| crate::resolve::ytdl::YtdlError::InvocationFailed(err.to_string()))
        }
    };

    let tracks = match tracks {
        Ok(tracks) => tracks,
        Err(err) => {
            error!(
                "ASTRALINK_ENQUEUE_FAILED guild_id={} source={:?} query=\"{}\" error=\"{}\"",
                guild_id, source_hint, query, err
            );
            let state = app.state.as_response(guild_id).await;
            return (
                StatusCode::BAD_GATEWAY,
                Json(EnqueueResponse {
                    state,
                    success: false,
                    message: Some(err.to_string()),
                    node: full_state.node,
                }),
            )
            .into_response();
        }
    };

    let queue_items: Vec<QueueItem> = tracks
        .into_iter()
        .map(|track| QueueItem {
            track,
            requested_by: body.user_id,
        })
        .collect();

    let position = body.position;
    let _ = app.state.enqueue(guild_id, queue_items, position).await;

    let response_state = app.state.as_response(guild_id).await;
    let ws_tx = app
        .ws_broadcasters
        .entry(guild_id)
        .or_insert_with(|| broadcast::channel::<StateResponse>(100).0)
        .clone();
    let _ = ws_tx.send(response_state.clone());

    crate::resolve::preload::trigger_preload(
        app.state.clone(),
        guild_id,
        app.config.clone(),
    );

    let node_for_response = response_state
        .node
        .clone()
        .or_else(|| Some(app.node_info.clone()));

    (
        StatusCode::OK,
        Json(EnqueueResponse {
            state: response_state,
            success: true,
            message: None,
            node: node_for_response,
        }),
    )
    .into_response()
}

async fn handle_nodes(State(app): State<AppState>) -> impl IntoResponse {
    let nodes = vec![app.node_info.clone()];
    (StatusCode::OK, Json(NodesResponse { nodes }))
}

async fn handle_get_state(
    Path(guild_id): Path<GuildId>,
    State(app): State<AppState>,
) -> Response {
    let current_state = app.state.as_response(guild_id).await;
    (StatusCode::OK, Json(current_state)).into_response()
}

async fn ws_handler(
    Path(guild_id): Path<GuildId>,
    State(app): State<AppState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_ws_socket(
            socket,
            app.state.clone(),
            app.ws_broadcasters.clone(),
            app.node_info.clone(),
            guild_id,
        )
    })
}

async fn handle_ws_socket(
    socket: WebSocket,
    state: Arc<AstralinkState>,
    ws_broadcasters: Arc<DashMap<GuildId, broadcast::Sender<StateResponse>>>,
    node_info: NodeInfo,
    guild_id: GuildId,
) {
    let full_state = state.as_response(guild_id).await;
    if full_state.node.as_ref().map_or(false, |n| n.id != node_info.id) {
        let error_json = r#"{"error": "Wrong node"}"#;
        let (mut sender, _) = socket.split();
        let _ = sender.send(Message::Text(error_json.to_string())).await;
        return;
    }

    let (mut sender, mut receiver) = socket.split();

    if let Ok(json) = serde_json::to_string(&full_state) {
        let _ = sender.send(Message::Text(json)).await;
    }

    let ws_tx = ws_broadcasters
        .entry(guild_id)
        .or_insert_with(|| broadcast::channel::<StateResponse>(100).0)
        .clone();
    let mut rx = ws_tx.subscribe();

    let sender_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(s) => {
                    if let Ok(json) = serde_json::to_string(&s) {
                        if sender.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });

    let receiver_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let _ = tokio::join!(sender_task, receiver_task);
}

async fn handle_actions(
    Path(guild_id): Path<GuildId>,
    State(app): State<AppState>,
    Json(action): Json<ActionRequest>,
) -> Response {
    let full_state = app.state.get_or_init(guild_id).await;
    let state = app.state.apply_action(guild_id, action).await;
    let ws_tx = app
        .ws_broadcasters
        .entry(guild_id)
        .or_insert_with(|| broadcast::channel::<StateResponse>(100).0)
        .clone();
    let _ = ws_tx.send(state.clone());
    let response = ActionResponse {
        ok: true,
        error: None,
        state: Some(state.clone()),
        node: full_state.node,
    };
    (StatusCode::OK, Json(response)).into_response()
}
