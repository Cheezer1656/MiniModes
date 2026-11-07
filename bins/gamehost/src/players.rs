//! When a player counts as present in an instance ([`Ready`]), and saving
//! and restoring player state in open worlds that keep it: inventory,
//! position, health, food and game mode per (game, player). State loads
//! before scripts see the player, and saves on leave, every few minutes,
//! and at shutdown.

use std::time::{Duration, Instant};

use chunkedge::UniqueId;
use chunkedge::entity::living::Health;
use chunkedge::entity::player::Food;
use chunkedge::prelude::bevy_ecs::system::SystemParam;
use chunkedge::prelude::*;
use chunkedge_binary::{Decode, Encode};
use mm_core::Tokio;
use mm_proto::GameId;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::instance::InInstance;
use crate::world::{GameWorld, Storage};

/// Give up waiting for saved state after this; the visit then isn't saved,
/// so a slow database can't wipe anyone's progress.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(5);
const SAVE_EVERY_TICKS: i64 = 20 * 60 * 5;
/// Bumped when [`SavedPlayer`] changes shape.
const FORMAT: u8 = 1;

/// The player is placed (and restored) and scripts may see them.
#[derive(Component)]
pub struct Ready;

#[derive(Component)]
struct Restoring {
    since: Instant,
}

/// This player's state is saved to `game` when they leave.
#[derive(Component)]
struct SavesState {
    game: GameId,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
struct SavedPlayer {
    format: u8,
    pos: [f64; 3],
    yaw: f32,
    pitch: f32,
    health: f32,
    food: i32,
    mode: u8,
    /// Non-empty slots, items in protocol encoding.
    slots: Vec<(u16, Vec<u8>)>,
}

fn mode_byte(mode: GameMode) -> u8 {
    match mode {
        GameMode::Survival => 0,
        GameMode::Creative => 1,
        GameMode::Adventure => 2,
        GameMode::Spectator => 3,
    }
}

fn byte_mode(b: u8) -> GameMode {
    match b {
        0 => GameMode::Survival,
        1 => GameMode::Creative,
        3 => GameMode::Spectator,
        _ => GameMode::Adventure,
    }
}

type StateParts<'a> = (&'a Position, &'a Look, &'a Health, &'a Food, &'a GameMode, &'a Inventory);

fn capture((pos, look, health, food, mode, inventory): StateParts) -> Vec<u8> {
    let slots = (0..inventory.slot_count())
        .filter_map(|slot| {
            let stack = inventory.slot(slot);
            if stack.is_empty() {
                return None;
            }
            let mut bytes = Vec::new();
            stack.encode(&mut bytes).ok()?;
            Some((slot, bytes))
        })
        .collect();
    let saved = SavedPlayer {
        format: FORMAT,
        pos: pos.0.to_array(),
        yaw: look.yaw,
        pitch: look.pitch,
        health: health.0,
        food: food.0,
        mode: mode_byte(*mode),
        slots,
    };
    postcard::to_stdvec(&saved).expect("serializable")
}

enum StateEvent {
    Loaded {
        player: Entity,
        layer: Entity,
        result: Result<Option<Vec<u8>>, String>,
    },
}

#[derive(Resource)]
struct StateEvents {
    tx: UnboundedSender<StateEvent>,
    rx: UnboundedReceiver<StateEvent>,
}

/// Runs after the world systems (worlds must be loaded to place players)
/// and before scripts look at who is present.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlayerSet;

pub struct PlayersPlugin;

impl Plugin for PlayersPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(StateEvents { tx, rx }).add_systems(
            Update,
            (save_leavers, mark_ready, apply_restores, save_periodically)
                .chain()
                .in_set(PlayerSet),
        );
    }
}

fn spawn_save(storage: &Storage, tokio: &Tokio, game: GameId, player: Uuid, state: Vec<u8>) -> JoinHandle<()> {
    let db = storage.db.clone();
    tokio.0.spawn(async move {
        if let Err(e) = mm_db::publish::save_player_state(&db, game.0, player, &state).await {
            tracing::error!("saving {player} in game {}: {e}", game.0);
        }
    })
}

/// Saves players as they disconnect (their components outlive `Client` for
/// this tick).
fn save_leavers(
    mut gone: RemovedComponents<Client>,
    players: Query<(&SavesState, &UniqueId, StateParts)>,
    storage: Res<Storage>,
    tokio: Res<Tokio>,
) {
    for entity in gone.read() {
        if let Ok((saves, uuid, parts)) = players.get(entity) {
            spawn_save(&storage, &tokio, saves.game, uuid.0, capture(parts));
        }
    }
}

fn mark_ready(
    mut commands: Commands,
    players: Query<(Entity, &InInstance, &UniqueId), (With<Client>, Without<Ready>, Without<Restoring>)>,
    layers: Query<&GameWorld>,
    storage: Res<Storage>,
    events: Res<StateEvents>,
    tokio: Res<Tokio>,
) {
    for (player, in_instance, uuid) in &players {
        let Ok(world) = layers.get(in_instance.0) else {
            continue; // still loading
        };
        if !world.save_players {
            commands.entity(player).insert(Ready);
            continue;
        }
        commands.entity(player).insert(Restoring { since: Instant::now() });
        let (db, tx, game, uuid, layer) = (storage.db.clone(), events.tx.clone(), world.game, uuid.0, in_instance.0);
        tokio.0.spawn(async move {
            let result = mm_db::publish::player_state(&db, game.0, uuid).await.map_err(|e| e.to_string());
            let _ = tx.send(StateEvent::Loaded { player, layer, result });
        });
    }
}

type RestoreParts<'a> = (
    &'a InInstance,
    &'a mut Client,
    &'a mut Position,
    &'a mut Look,
    &'a mut Health,
    &'a mut Food,
    &'a mut GameMode,
    &'a mut Inventory,
    &'a Restoring,
);

fn apply_restores(
    mut commands: Commands,
    mut events: ResMut<StateEvents>,
    mut players: Query<(Entity, RestoreParts)>,
    layers: Query<&GameWorld>,
) {
    while let Ok(StateEvent::Loaded { player, layer, result }) = events.rx.try_recv() {
        let Ok((_, (in_instance, mut client, mut pos, mut look, mut health, mut food, mut mode, mut inventory, _))) =
            players.get_mut(player)
        else {
            continue; // left, or timed out already
        };
        let Some(world) = layers.get(layer).ok().filter(|_| in_instance.0 == layer) else {
            continue;
        };
        let mut entity = commands.entity(player);
        entity.remove::<Restoring>().insert(Ready);
        let bytes = match result {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::error!("loading saved state of a player in game {}: {e}", world.game.0);
                client.send_chat_message("§cCouldn't load your progress, so this visit won't be saved.");
                continue;
            }
        };
        entity.insert(SavesState { game: world.game });
        let Some(saved) = bytes.and_then(|b| postcard::from_bytes::<SavedPlayer>(&b).ok()) else {
            continue; // first visit
        };
        if saved.format != FORMAT {
            continue;
        }
        let at = saved.pos.map(|c| c.floor() as i32);
        if world.mirror.bounds.contains(at) {
            pos.set(saved.pos);
            look.yaw = saved.yaw;
            look.pitch = saved.pitch;
        }
        health.0 = saved.health.clamp(1.0, 20.0);
        food.0 = saved.food.clamp(0, 20);
        *mode = byte_mode(saved.mode);
        for slot in 0..inventory.slot_count() {
            inventory.set_slot(slot, ItemStack::EMPTY);
        }
        for (slot, bytes) in saved.slots {
            if slot < inventory.slot_count()
                && let Ok(stack) = ItemStack::decode(&mut bytes.as_slice())
            {
                inventory.set_slot(slot, stack);
            }
        }
    }
    for (player, (.., restoring)) in &players {
        if restoring.since.elapsed() >= RESTORE_TIMEOUT {
            commands.entity(player).remove::<Restoring>().insert(Ready);
        }
    }
}

fn save_periodically(
    players: Query<(&SavesState, &UniqueId, StateParts), With<Client>>,
    storage: Res<Storage>,
    tokio: Res<Tokio>,
    server: Res<Server>,
) {
    if server.current_tick() % SAVE_EVERY_TICKS != 0 {
        return;
    }
    for (saves, uuid, parts) in &players {
        spawn_save(&storage, &tokio, saves.game, uuid.0, capture(parts));
    }
}

/// Saves every connected player's state, for the shutdown handler.
#[derive(SystemParam)]
pub struct ShutdownSaves<'w, 's> {
    players: Query<'w, 's, (&'static SavesState, &'static UniqueId, StateParts<'static>), With<Client>>,
    storage: Res<'w, Storage>,
    tokio: Res<'w, Tokio>,
}

impl ShutdownSaves<'_, '_> {
    pub fn save_all(&mut self) -> Vec<JoinHandle<()>> {
        self.players
            .iter()
            .map(|(saves, uuid, parts)| spawn_save(&self.storage, &self.tokio, saves.game, uuid.0, capture(parts)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_state_round_trips_items() {
        use chunkedge::item::ItemComponent;
        use chunkedge::protocol::IntoTextComponent;
        let named = ItemStack::new(ItemKind::DiamondPickaxe, 1)
            .with_components(vec![ItemComponent::ItemName(Text::from("Prize").not_italic().into_text_component())]);
        let mut bytes = Vec::new();
        named.encode(&mut bytes).unwrap();
        let saved = SavedPlayer {
            format: FORMAT,
            pos: [1.5, 70.0, -3.5],
            yaw: 90.0,
            pitch: 0.0,
            health: 12.0,
            food: 7,
            mode: mode_byte(GameMode::Survival),
            slots: vec![(36, bytes)],
        };
        let back: SavedPlayer = postcard::from_bytes(&postcard::to_stdvec(&saved).unwrap()).unwrap();
        assert_eq!(back, saved);
        let stack = ItemStack::decode(&mut back.slots[0].1.as_slice()).unwrap();
        assert_eq!(stack, named);
        assert_eq!(byte_mode(back.mode), GameMode::Survival);
    }
}
