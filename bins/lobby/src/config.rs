use std::net::SocketAddr;
use std::path::PathBuf;

use chunkedge::prelude::*;
use std::collections::HashMap;

use mm_core::ThreadConfig;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LobbyConfig {
    pub bind: SocketAddr,
    pub max_players: usize,
    /// Loopback address shards connect to.
    pub coordinator_bind: SocketAddr,
    /// Gate's API origin, e.g. `http://127.0.0.1:8080`.
    pub gate_api: String,
    /// Vanilla world save containing a `region` directory. When unset, a flat
    /// platform is generated instead (local development only).
    pub world: Option<PathBuf>,
    /// Chunks within this radius of spawn stay loaded permanently.
    pub keep_loaded_radius: i32,
    pub spawn: Spawn,
    #[serde(default)]
    pub threads: ThreadConfig,
    #[serde(default, rename = "npc")]
    pub npcs: Vec<NpcConfig>,
    #[serde(default = "default_hotbar")]
    pub hotbar: Vec<HotbarConfig>,
    /// Limits and staff powers per rank; players without a rank row are `member`.
    #[serde(default = "crate::account::default_ranks")]
    pub ranks: HashMap<String, crate::account::RankLimits>,
    /// The web script editor; unset disables it.
    pub web: Option<crate::web::WebConfig>,
    #[serde(default)]
    pub moderation: crate::moderation::ModerationConfig,
    /// Loopback address serving Prometheus metrics at `/metrics`; unset disables them.
    pub metrics_bind: Option<SocketAddr>,
}

#[derive(Resource, Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    #[serde(default)]
    pub yaw: f32,
    #[serde(default)]
    pub pitch: f32,
    /// Players below this Y are sent back to spawn.
    pub void_y: f64,
}

impl Spawn {
    pub fn pos(self) -> DVec3 {
        DVec3::new(self.x, self.y, self.z)
    }
}

/// Lobby menus that NPCs and hotbar items can open.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MenuName {
    Browser,
    MyGames,
    Favorites,
    Search,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NpcConfig {
    /// Label lines above the NPC; `§` color codes are allowed.
    pub label: Vec<String>,
    pub pos: [f64; 3],
    #[serde(default)]
    pub yaw: f32,
    #[serde(default)]
    pub pitch: f32,
    pub opens: MenuName,
    pub skin: Option<SkinConfig>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SkinConfig {
    pub value: String,
    pub signature: String,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct HotbarConfig {
    /// Hotbar index, 0..=8.
    pub slot: u8,
    /// Item id, e.g. `compass`.
    pub item: String,
    pub name: String,
    #[serde(default)]
    pub lore: Vec<String>,
    pub opens: MenuName,
}

fn default_hotbar() -> Vec<HotbarConfig> {
    let entry = |slot, item: &str, name: &str, opens| HotbarConfig {
        slot,
        item: item.to_owned(),
        name: name.to_owned(),
        lore: vec!["§7Right-click to open".to_owned()],
        opens,
    };
    vec![
        entry(0, "compass", "§aGame Browser", MenuName::Browser),
        entry(1, "writable_book", "§bMy Games", MenuName::MyGames),
        entry(4, "nether_star", "§eFavorites & Recent", MenuName::Favorites),
        entry(8, "name_tag", "§dSearch Games", MenuName::Search),
    ]
}
