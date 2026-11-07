use std::net::SocketAddr;
use std::time::Duration;

use mm_core::ThreadConfig;
use mm_proto::ipc::ShardId;
use mm_script::Limits;
use serde::Deserialize;

use crate::script::ScriptConfig;

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
    #[serde(default)]
    pub scripts: ScriptSettings,
    /// Public origin of the lobby's web editor, e.g. `https://edit.example.net`.
    /// `/script edit` links point here; unset disables the command.
    pub editor_url: Option<String>,
}

/// Per-VM script limits. Memory will come from the owner's rank later.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScriptSettings {
    pub memory_mb: usize,
    /// CPU time per tick before running script threads are yielded.
    pub slice_ms: f64,
    /// Ticks in a row a VM may use its whole slice before it's stopped.
    pub max_overrun_ticks: u32,
    pub max_fill_volume: u64,
    /// Script block writes applied per instance per tick.
    pub blocks_per_tick: usize,
}

impl Default for ScriptSettings {
    fn default() -> Self {
        let limits = Limits::default();
        Self {
            memory_mb: limits.memory >> 20,
            slice_ms: limits.slice.as_secs_f64() * 1000.0,
            max_overrun_ticks: limits.max_overrun_steps,
            max_fill_volume: limits.max_fill_volume,
            blocks_per_tick: 8192,
        }
    }
}

impl ScriptSettings {
    pub fn script_config(&self, editor_url: Option<String>) -> ScriptConfig {
        ScriptConfig {
            editor_url: editor_url.map(|u| u.trim_end_matches('/').to_owned()),
            limits: Limits {
                memory: self.memory_mb << 20,
                slice: Duration::from_secs_f64(self.slice_ms.max(0.1) / 1000.0),
                max_overrun_steps: self.max_overrun_ticks,
                max_fill_volume: self.max_fill_volume,
                ..Limits::default()
            },
            blocks_per_tick: self.blocks_per_tick.max(1),
        }
    }
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
