use crate::model::{GuildId, NodeInfo};
use dashmap::DashMap;
use redis::{AsyncCommands, Client};
use serde_json;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
const NODES_KEY: &str = "astralink:nodes";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const EXPIRE_SECS: i64 = 30;

pub async fn register_node(redis: Arc<Client>, node_info: NodeInfo) {
    let mut con = redis.get_multiplexed_tokio_connection().await.unwrap();
    let json = serde_json::to_string(&node_info).unwrap();
    let score = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as f64;
    let _: redis::RedisResult<()> = con.zadd(NODES_KEY, score, &json).await;
    let _: redis::RedisResult<()> = con.expire(NODES_KEY, EXPIRE_SECS).await;
}

pub async fn heartbeat_loop(
    redis: Arc<Client>,
    node_info: NodeInfo,
    connected: Arc<DashMap<GuildId, ()>>,
) {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    loop {
        interval.tick().await;
        let connected_count = connected.len();
        let available = node_info.total_slots.saturating_sub(connected_count);
        let n = NodeInfo {
            available_slots: available,
            ..node_info.clone()
        };
        let mut con = redis.get_multiplexed_tokio_connection().await.unwrap();
        let json = serde_json::to_string(&n).unwrap();
        let score = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as f64;
        let _: redis::RedisResult<()> = con.zadd(NODES_KEY, score, &json).await;
        let old_score = score - 60.0;
        let _: redis::RedisResult<()> = con.zrembyscore(NODES_KEY, 0.0, old_score).await;
        let _: redis::RedisResult<()> = con.expire(NODES_KEY, EXPIRE_SECS).await;
    }
}

pub async fn get_active_nodes(redis: Arc<Client>) -> Vec<NodeInfo> {
    let mut con = redis.get_multiplexed_tokio_connection().await.unwrap();
    let members: Vec<String> = con.zrange(NODES_KEY, 0, -1).await.unwrap_or_default();
    let mut nodes = Vec::new();
    for json_str in members {
        if let Ok(node) = serde_json::from_str::<NodeInfo>(&json_str) {
            nodes.push(node);
        }
    }
    nodes.retain(|n| n.available_slots > 0);
    nodes
}

pub async fn pick_least_loaded(redis: Arc<Client>) -> Option<NodeInfo> {
    let nodes = get_active_nodes(redis.clone()).await;
    if nodes.is_empty() {
        return None;
    }
    nodes
        .into_iter()
        .filter(|n| n.available_slots > 0)
        .max_by_key(|n| n.available_slots)
}

pub fn connected_guilds_count(connected: &DashMap<GuildId, ()>) -> usize {
    connected.len()
}
