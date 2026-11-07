//! Draft worlds: loading a draft's map, keeping its [`BlockMirror`] beside the
//! layer, autosaving changed maps as new content-addressed `.mmw` files, and a
//! final save when the instance stops or the shard shuts down.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chunkedge::UniqueId;
use chunkedge::prelude::*;
use chunkedge::world_border::WorldBorderLerp;
use mm_core::Tokio;
use mm_db::Pool;
use mm_proto::GameId;
use mm_proto::ipc::{DiagnosticLevel, InstanceId, ShardMsg};
use mm_world::{BlockMirror, Bounds, MapHash, SectionPos, WorldStore};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::instance::{InInstance, Instance, SendToLobby};
use crate::link::{LobbyLink, ShutdownReply, ShutdownRequests};

/// Grass level of a new draft.
const NEW_MAP_FLOOR: i32 = 64;

#[derive(Resource, Clone)]
pub struct Storage {
    pub db: Pool,
    pub maps: WorldStore,
}

#[derive(Resource)]
pub struct Autosave(pub Duration);

/// A loaded draft's blocks. Present once loading finished.
#[derive(Component)]
pub struct DraftWorld {
    pub game: GameId,
    pub mirror: BlockMirror,
    last_save: Instant,
    saving: bool,
}

/// Draft layer whose map is still loading.
#[derive(Component)]
pub struct Loading;

type Chunks = Vec<([i32; 2], UnloadedChunk)>;

enum WorldEvent {
    Loaded {
        layer: Entity,
        game: GameId,
        result: anyhow::Result<(BlockMirror, Chunks)>,
    },
    Saved {
        layer: Entity,
        game: GameId,
        /// Sections to mark dirty again if the save failed.
        result: Result<MapHash, (HashSet<SectionPos>, String)>,
        /// Set for the final save of a stopped instance.
        ended: Option<InstanceId>,
    },
}

#[derive(Resource)]
struct WorldEvents {
    tx: UnboundedSender<WorldEvent>,
    rx: UnboundedReceiver<WorldEvent>,
}

/// Serializes loads and saves of the same draft on this shard, so a reopened
/// draft never reads the map pointer before the previous save updated it.
#[derive(Resource, Default)]
struct SaveLocks(HashMap<GameId, Arc<tokio::sync::Mutex<()>>>);

impl SaveLocks {
    fn get(&mut self, game: GameId) -> Arc<tokio::sync::Mutex<()>> {
        self.0.entry(game).or_default().clone()
    }
}

/// Map loading and saving; runs after the instance systems.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorldSet;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(WorldEvents { tx, rx })
            .init_resource::<SaveLocks>()
            .add_systems(
                Update,
                (
                    start_loads,
                    apply_world_events,
                    spawn_arrivals,
                    autosave,
                    save_stopped_drafts,
                    answer_shutdown,
                )
                    .chain()
                    .in_set(WorldSet),
            );
    }
}

/// Starts loading every newly created draft layer.
fn start_loads(
    layers: Query<(Entity, &Instance, &ChunkLayer), Added<Loading>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    for (layer, instance, chunks) in &layers {
        let game = instance.spec.game;
        let (storage, tx, lock) = (storage.clone(), events.tx.clone(), locks.get(game));
        let (min_y, height) = (chunks.min_y(), chunks.height());
        tokio.0.spawn(async move {
            let _guard = lock.lock().await;
            let result = load_draft(&storage, game, min_y, height).await;
            let _ = tx.send(WorldEvent::Loaded { layer, game, result });
        });
    }
}

async fn load_draft(storage: &Storage, game: GameId, min_y: i32, height: u32) -> anyhow::Result<(BlockMirror, Chunks)> {
    let row = mm_db::game(&storage.db, game.0).await?.ok_or_else(|| anyhow::anyhow!("game {} not found", game.0))?;
    let maps = storage.maps.clone();
    tokio::task::spawn_blocking(move || {
        let mirror = match &row.draft_map_hash {
            Some(hash) => mm_world::decode(&maps.get(MapHash::parse(hash)?)?)?,
            None => BlockMirror::flat(Bounds::square(row.map_size), NEW_MAP_FLOOR),
        };
        // Build chunks here, off the main thread. One ring of empty chunks
        // around the build area keeps the edge from looking unloaded.
        let mut area = mirror.bounds;
        area.min[0] -= 16;
        area.min[2] -= 16;
        area.max[0] += 16;
        area.max[2] += 16;
        let chunks = area.chunk_columns().map(|c| (c, mirror.chunk(c, min_y, height))).collect();
        Ok((mirror, chunks))
    })
    .await?
}

#[allow(clippy::too_many_arguments)]
fn apply_world_events(
    mut commands: Commands,
    mut events: ResMut<WorldEvents>,
    mut layers: Query<(&Instance, &mut ChunkLayer, &mut WorldBorderLerp, Option<&mut DraftWorld>), Without<Despawned>>,
    mut players: Query<(Entity, &InInstance, &mut Position, &mut Look, &mut Client)>,
    mut to_lobby: MessageWriter<SendToLobby>,
    link: Res<LobbyLink>,
) {
    while let Ok(event) = events.rx.try_recv() {
        match event {
            WorldEvent::Loaded { layer, game, result } => {
                let Ok((_, mut chunks, mut border, _)) = layers.get_mut(layer) else {
                    continue; // stopped while loading
                };
                match result {
                    Ok((mirror, loaded)) => {
                        for (pos, chunk) in loaded {
                            chunks.insert_chunk(pos, chunk);
                        }
                        let size = mirror.bounds.size()[0] as f64;
                        border.current_diameter = size;
                        border.target_diameter = size;
                        let spawn = mirror.spawn;
                        for (_, in_instance, mut pos, mut look, _) in &mut players {
                            if in_instance.0 == layer {
                                pos.set(spawn.pos);
                                look.yaw = spawn.yaw;
                                look.pitch = spawn.pitch;
                            }
                        }
                        commands.entity(layer).remove::<Loading>().insert(DraftWorld {
                            game,
                            mirror,
                            last_save: Instant::now(),
                            saving: false,
                        });
                        tracing::info!("loaded draft of game {}", game.0);
                    }
                    Err(e) => {
                        tracing::error!("loading draft of game {}: {e:#}", game.0);
                        link.send(ShardMsg::Diagnostic {
                            game,
                            level: DiagnosticLevel::Error,
                            message: format!("couldn't load the draft map: {e:#}"),
                        });
                        for (entity, in_instance, .., mut client) in &mut players {
                            if in_instance.0 == layer {
                                client.send_chat_message("§cThis draft's map couldn't be loaded. Returning you to the lobby.");
                                to_lobby.write(SendToLobby(entity));
                            }
                        }
                    }
                }
            }
            WorldEvent::Saved {
                layer,
                game,
                result,
                ended,
            } => {
                match result {
                    Ok(hash) => tracing::info!("saved draft of game {} as {hash}", game.0),
                    Err((sections, e)) => {
                        tracing::error!("saving draft of game {}: {e}", game.0);
                        if let Ok((.., Some(mut world))) = layers.get_mut(layer) {
                            world.mirror.mark_dirty(sections);
                        }
                    }
                }
                if let Ok((.., Some(mut world))) = layers.get_mut(layer) {
                    world.saving = false;
                }
                if let Some(instance) = ended {
                    link.send(ShardMsg::InstanceEnded { instance });
                }
            }
        }
    }
}

/// Players entering an already-loaded draft start at its spawn. (Players
/// who arrive while it loads are moved when loading finishes.)
fn spawn_arrivals(mut players: Query<(&InInstance, &mut Position, &mut Look), Added<InInstance>>, layers: Query<&DraftWorld>) {
    for (in_instance, mut pos, mut look) in &mut players {
        if let Ok(world) = layers.get(in_instance.0) {
            pos.set(world.mirror.spawn.pos);
            look.yaw = world.mirror.spawn.yaw;
            look.pitch = world.mirror.spawn.pitch;
        }
    }
}

/// Snapshots the mirror (cheap: sections are shared) and saves it on tokio.
fn start_save(
    layer: Entity,
    world: &mut DraftWorld,
    ended: Option<InstanceId>,
    storage: &Storage,
    events: &WorldEvents,
    locks: &mut SaveLocks,
    tokio: &Tokio,
) -> JoinHandle<()> {
    let dirty = world.mirror.take_dirty();
    let snapshot = world.mirror.clone();
    world.saving = true;
    world.last_save = Instant::now();
    let (game, storage, tx, lock) = (world.game, storage.clone(), events.tx.clone(), locks.get(world.game));
    tokio.0.spawn(async move {
        let _guard = lock.lock().await;
        let result = async {
            let maps = storage.maps.clone();
            let hash = tokio::task::spawn_blocking(move || maps.put(&mm_world::encode(&snapshot)?)).await??;
            mm_db::set_draft_map(&storage.db, game.0, &hash.to_string()).await?;
            anyhow::Ok(hash)
        }
        .await
        .map_err(|e| (dirty, format!("{e:#}")));
        let _ = tx.send(WorldEvent::Saved {
            layer,
            game,
            result,
            ended,
        });
    })
}

fn autosave(
    mut layers: Query<(Entity, &mut DraftWorld), Without<Despawned>>,
    interval: Res<Autosave>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    for (layer, mut world) in &mut layers {
        if !world.saving && world.mirror.is_dirty() && world.last_save.elapsed() >= interval.0 {
            start_save(layer, &mut world, None, &storage, &events, &mut locks, &tokio);
        }
    }
}

/// Stopped draft instances report `InstanceEnded` only after a final save.
fn save_stopped_drafts(
    mut layers: Query<(Entity, &Instance, Option<&mut DraftWorld>), Added<Despawned>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
    link: Res<LobbyLink>,
) {
    for (layer, instance, world) in &mut layers {
        if !instance.is_draft() {
            continue;
        }
        match world {
            Some(mut world) if world.mirror.is_dirty() => {
                start_save(layer, &mut world, Some(instance.spec.id), &storage, &events, &mut locks, &tokio);
            }
            _ => link.send(ShardMsg::InstanceEnded {
                instance: instance.spec.id,
            }),
        }
    }
}

fn answer_shutdown(
    mut requests: ResMut<ShutdownRequests>,
    clients: Query<&UniqueId, With<Client>>,
    mut layers: Query<(Entity, &mut DraftWorld), Without<Despawned>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    while let Ok(reply) = requests.0.try_recv() {
        let saves = layers
            .iter_mut()
            .filter(|(_, w)| w.mirror.is_dirty())
            .map(|(layer, mut world)| start_save(layer, &mut world, None, &storage, &events, &mut locks, &tokio))
            .collect();
        let _ = reply.send(ShutdownReply {
            players: clients.iter().map(|u| u.0).collect(),
            saves,
        });
    }
}
