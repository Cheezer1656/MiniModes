use std::net::SocketAddr;

use mm_core::ThreadConfig;
use mm_proto::ipc::ShardId;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamehostConfig {
    /// `N` in `gamehost@N`.
    pub shard: u16,
    /// Loopback address Gate forwards players to. Also registered with Gate.
    pub bind: SocketAddr,
    pub max_players: usize,
    /// Server name to register with Gate. Defaults to `gh-<shard>`.
    pub gate_name: Option<String>,
    /// The lobby's coordinator address.
    pub lobby_coordinator: SocketAddr,
    /// Gate's API origin, e.g. `http://127.0.0.1:8080`.
    pub gate_api: String,
    /// The lobby's server name in Gate's config.
    #[serde(default = "default_lobby_server")]
    pub lobby_server: String,
    /// Unload an instance this long after its last player leaves.
    #[serde(default = "default_idle_unload_secs")]
    pub idle_unload_secs: u64,
    /// Holds `worlds/` (content-addressed `.mmw` maps).
    #[serde(default = "default_data_dir")]
    pub data_dir: std::path::PathBuf,
    #[serde(default = "default_autosave_secs")]
    pub autosave_secs: u64,
    #[serde(default = "default_max_edit_volume")]
    pub max_edit_volume: u64,
    #[serde(default = "default_edit_blocks_per_tick")]
    pub edit_blocks_per_tick: usize,
    #[serde(default)]
    pub threads: ThreadConfig,
}

fn default_data_dir() -> std::path::PathBuf {
    "data".into()
}
fn default_autosave_secs() -> u64 {
    60
}
fn default_max_edit_volume() -> u64 {
    1_000_000
}
fn default_edit_blocks_per_tick() -> usize {
    20_000
}

impl GamehostConfig {
    pub fn shard_id(&self) -> ShardId {
        ShardId(self.shard)
    }

    pub fn gate_name(&self) -> String {
        self.gate_name.clone().unwrap_or_else(|| format!("gh-{}", self.shard))
    }
}

fn default_lobby_server() -> String {
    "lobby".to_owned()
}

fn default_idle_unload_secs() -> u64 {
    300
}
