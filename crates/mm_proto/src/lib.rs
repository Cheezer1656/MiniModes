//! Types shared between the lobby and game-host shards, and the IPC protocol
//! they speak over loopback TCP (see [`ipc`]).

pub mod ipc;

use serde::{Deserialize, Serialize};

/// Primary key of a game (Postgres `bigint`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct GameId(pub i64);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum GameKind {
    /// Round-based: platform queue, countdown and `EndMatch`; worlds reset per match.
    Match,
    /// Drop-in, long-lived, never ends (prisons, survival, RPGs).
    OpenWorld {
        /// One canonical instance whose world changes are saved, versus
        /// parallel servers that each start from the published map.
        persistent_world: bool,
    },
}

/// Listing data for one published game.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GameSummary {
    pub id: GameId,
    pub name: String,
    pub creator: String,
    pub description: String,
    /// Item id used as the menu icon, e.g. `diamond_sword`.
    pub icon: String,
    pub kind: GameKind,
    /// Cap per instance.
    pub max_players: u32,
    pub featured: bool,
    /// Unix seconds of the first publish.
    pub published_at: i64,
    /// Mean rating out of 5, if anyone rated it.
    pub rating: Option<f32>,
}

/// Live numbers for a game, reported by shards.
#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug)]
pub struct GameLiveStats {
    pub players: u32,
    /// Match instances still accepting players.
    pub waiting: u32,
    /// Match instances that have started.
    pub in_progress: u32,
}
