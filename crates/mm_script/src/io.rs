//! What crosses between the main thread and a VM step: events and a snapshot
//! in, commands and diagnostics out. None of it borrows from the ECS world.

use std::time::Duration;

use chunkedge::prelude::*;
use mm_proto::GameKind;
use mm_proto::ipc::DiagnosticLevel;
use mm_world::BlockMirror;

use crate::rules::Rules;

/// A player's id inside one VM, assigned by the host on join and never reused
/// while the VM lives.
pub type PlayerId = u32;
pub type RegionId = u32;

/// What scripts know about the game they run in (`game.GameId`, ...).
#[derive(Clone, Debug)]
pub struct GameInfo {
    pub game_id: i64,
    /// `None` in a draft.
    pub version: Option<u32>,
    pub kind: GameKind,
    pub max_players: u32,
    /// Persistent worlds: Unix seconds of the last unload.
    pub last_shutdown: Option<i64>,
}

impl Default for GameInfo {
    fn default() -> Self {
        Self {
            game_id: 0,
            version: None,
            kind: GameKind::Match,
            max_players: 8,
            last_shutdown: None,
        }
    }
}

/// Where a match is, as the host runs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchPhase {
    Waiting,
    Starting,
    Started,
    Ended,
}

impl MatchPhase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Waiting => "Waiting",
            Self::Starting => "Starting",
            Self::Started => "Started",
            Self::Ended => "Ended",
        }
    }
}

/// Read-only player state as of the tick the step was spawned.
#[derive(Clone, Debug)]
pub struct PlayerSnap {
    pub id: PlayerId,
    pub pos: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub health: f32,
    pub food: i32,
    pub game_mode: GameMode,
    pub held: ItemKind,
}

#[derive(Clone, Default)]
pub struct Snapshot {
    pub tick: u64,
    pub players: Vec<PlayerSnap>,
    /// A fresh copy of the instance's blocks, when the host changed them in
    /// ways the VM wasn't told about (map load, WorldEdit).
    pub blocks: Option<BlockMirror>,
}

/// Something that happened on the main thread since the last step.
#[derive(Clone, Debug)]
pub enum Event {
    PlayerAdded { id: PlayerId, name: String, uuid: Uuid },
    PlayerRemoving { id: PlayerId },
    Chatted { id: PlayerId, message: String },
    /// Already applied; `Cancel()` puts `old` back.
    BlockBroken { player: PlayerId, pos: [i32; 3], old: BlockState },
    BlockPlaced { player: PlayerId, pos: [i32; 3], old: BlockState, new: BlockState },
    Died { id: PlayerId },
    Respawned { id: PlayerId },
    /// The match moved to `phase`. `seconds` is the countdown for `Starting`;
    /// `winners` are set for `Ended`.
    Match { phase: MatchPhase, seconds: u32, winners: Vec<PlayerId> },
    /// The instance is shutting down: run `game:BindToClose` callbacks.
    Closing,
}

#[derive(Clone, Debug)]
pub enum Command {
    Message { player: PlayerId, text: String },
    Title {
        player: PlayerId,
        title: String,
        subtitle: String,
        /// Fade in, stay, fade out, in ticks.
        times: [i32; 3],
    },
    ActionBar { player: PlayerId, text: String },
    Teleport { player: PlayerId, pos: [f64; 3], look: Option<[f32; 2]> },
    SetGameMode { player: PlayerId, mode: GameMode },
    SetHealth { player: PlayerId, health: f32 },
    SetFood { player: PlayerId, food: i32 },
    Give { player: PlayerId, item: ItemStack },
    SetSlot { player: PlayerId, slot: u16, item: ItemStack },
    ClearInventory { player: PlayerId },
    PlaySound { player: PlayerId, sound: String, volume: f32, pitch: f32 },
    SendToLobby { player: PlayerId },
    /// Already applied to the VM's mirror. The host may spread them over
    /// several ticks.
    SetBlocks(Vec<([i32; 3], BlockState)>),
    /// A late `Cancel()`: restore `restore` at `pos` if it still holds `expect`.
    Revert { pos: [i32; 3], expect: BlockState, restore: BlockState },
    /// `region: None` sets the game-wide rules.
    SetRules { region: Option<(RegionId, mm_world::Bounds)>, rules: Rules },
    RemoveRegion(RegionId),
    /// `game.Match.MinPlayers` / `CountdownSeconds` changed.
    MatchSettings { min_players: u32, countdown: u32 },
    /// `game.Match:End(winners)`.
    EndMatch { winners: Vec<PlayerId> },
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
}

/// Caps for one VM. The host sets them from config and, later, the owner's rank.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Luau heap.
    pub memory: usize,
    /// CPU time per step before running threads are yielded.
    pub slice: Duration,
    /// Steps in a row that may use their whole slice before the VM is killed.
    pub max_overrun_steps: u32,
    /// Blocks one `Fill`/`Refill` may touch.
    pub max_fill_volume: u64,
    /// Commands one step may emit; the rest are dropped with a warning.
    pub max_commands: usize,
    /// Diagnostics one step may emit.
    pub max_diagnostics: usize,
    /// Modules that may opt into native codegen with `--!native`.
    pub max_native_modules: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory: 64 << 20,
            slice: Duration::from_millis(2),
            max_overrun_steps: 100,
            max_fill_volume: 1 << 18,
            max_commands: 10_000,
            max_diagnostics: 50,
            max_native_modules: 4,
        }
    }
}

#[derive(Default)]
pub struct StepOutput {
    pub commands: Vec<Command>,
    pub diagnostics: Vec<Diagnostic>,
    /// The slice ran out with threads still runnable.
    pub overran: bool,
    /// Threads are ready to run (budget-yielded or deferred).
    pub runnable: bool,
    /// The earliest tick a sleeping thread wakes.
    pub next_timer: Option<u64>,
    /// The VM must be torn down (sustained overrun); the reason.
    pub fatal: Option<String>,
    /// After [`Event::Closing`]: every `BindToClose` callback has returned.
    pub closed: bool,
}
