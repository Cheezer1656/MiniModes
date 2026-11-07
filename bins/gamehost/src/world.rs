//! Instance worlds: loading the right map (a draft, a published version, or
//! a persistent game's live world), keeping its [`BlockMirror`] beside the
//! layer, autosaving drafts and live worlds as new content-addressed `.mmw`
//! files, a final save when the instance stops or the shard shuts down, and
//! applying publishes to a running live world.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chunkedge::UniqueId;
use chunkedge::prelude::*;
use chunkedge::world_border::WorldBorderLerp;
use mm_core::Tokio;
use mm_db::Pool;
use mm_proto::ipc::{DiagnosticLevel, GameVersion, InstanceId, InstanceSpec, ShardMsg};
use mm_proto::{GameId, GameKind, LiveUpdate};
use mm_world::{BlockMirror, Bounds, MapHash, SectionPos, WorldStore};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::instance::{InInstance, Instance, SendToLobby, StopRequest};
use crate::link::{LobbyLink, ShutdownReply, ShutdownRequests};
use crate::script::{RestartScripts, ScriptHost};

/// Grass level of a new map.
const NEW_MAP_FLOOR: i32 = 64;

#[derive(Resource, Clone)]
pub struct Storage {
    pub db: Pool,
    pub maps: WorldStore,
}

#[derive(Resource)]
pub struct Autosave(pub Duration);

/// Blocks per tick a live update copies into a running world.
#[derive(Resource)]
pub struct UpdateBlocksPerTick(pub usize);

/// Where a world's changes go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Persist {
    /// The creator's draft map.
    Draft,
    /// A persistent game's one live world.
    Live,
    /// Matches and static open worlds start fresh every time.
    None,
}

/// A loaded instance's blocks and what it runs. Present once loading finished.
#[derive(Component)]
pub struct GameWorld {
    pub game: GameId,
    pub mirror: BlockMirror,
    pub persist: Persist,
    /// Published version whose scripts run; `None` for a draft.
    pub version: Option<u32>,
    /// Live worlds: Unix seconds of the previous unload.
    pub last_shutdown: Option<i64>,
    /// Published open worlds that keep each player's inventory and position.
    pub save_players: bool,
    last_save: Instant,
    saving: bool,
    /// A live update is in flight; saving now could undo a reset.
    updating: bool,
}

/// Instance layer whose map is still loading.
#[derive(Component)]
pub struct Loading;

/// A publish arrived while the live world was loading; apply it after.
#[derive(Component)]
struct UpdateQueued;

/// Blocks a live update still has to copy into the world.
#[derive(Component)]
struct PendingWrites(VecDeque<([i32; 3], BlockState)>);

type Chunks = Vec<([i32; 2], UnloadedChunk)>;

struct Loaded {
    mirror: BlockMirror,
    chunks: Chunks,
    persist: Persist,
    version: Option<u32>,
    last_shutdown: Option<i64>,
    save_players: bool,
    update: Option<LiveUpdate>,
}

/// What a publish changes in a running live world.
enum Change {
    Scripts,
    Writes(Vec<([i32; 3], BlockState)>),
    Reset(BlockMirror, Chunks),
}

enum WorldEvent {
    Loaded {
        layer: Entity,
        game: GameId,
        result: anyhow::Result<Loaded>,
    },
    Saved {
        layer: Entity,
        game: GameId,
        /// The new map (`None`: nothing changed, only the shutdown was
        /// recorded); on failure, sections to mark dirty again.
        result: Result<Option<MapHash>, (HashSet<SectionPos>, String)>,
        /// Set for the final save of a stopped instance.
        ended: Option<InstanceId>,
    },
    Updated {
        layer: Entity,
        game: GameId,
        /// `None`: nothing was pending.
        result: anyhow::Result<Option<(u32, LiveUpdate, Change)>>,
    },
    /// A draft save the lobby asked for (before publishing) finished.
    DraftSaved { request: u64, ok: bool },
}

#[derive(Resource)]
struct WorldEvents {
    tx: UnboundedSender<WorldEvent>,
    rx: UnboundedReceiver<WorldEvent>,
}

/// Serializes loads, saves and updates of the same game's world on this
/// shard, so a reopened world never reads the map pointer before the
/// previous save updated it.
#[derive(Resource, Default)]
struct SaveLocks(HashMap<GameId, Arc<tokio::sync::Mutex<()>>>);

impl SaveLocks {
    fn get(&mut self, game: GameId) -> Arc<tokio::sync::Mutex<()>> {
        self.0.entry(game).or_default().clone()
    }
}

/// The lobby asked for a draft save (`LobbyMsg::SaveDraft`).
#[derive(Message, Clone, Copy)]
pub struct SaveDraft {
    pub game: GameId,
    pub request: u64,
}

/// A version was published (`LobbyMsg::ApplyPublish`).
#[derive(Message, Clone, Copy)]
pub struct ApplyPublish {
    pub game: GameId,
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
            .add_message::<SaveDraft>()
            .add_message::<ApplyPublish>()
            .add_systems(
                Update,
                (
                    start_loads,
                    apply_world_events,
                    spawn_arrivals,
                    save_requested_drafts,
                    start_updates,
                    write_updates,
                    autosave,
                    save_stopped_worlds,
                    answer_shutdown,
                )
                    .chain()
                    .in_set(WorldSet),
            );
    }
}

impl GameWorld {
    pub fn is_draft(&self) -> bool {
        self.persist == Persist::Draft
    }
}

/// Starts loading every newly created instance layer.
fn start_loads(
    layers: Query<(Entity, &Instance, &ChunkLayer), Added<Loading>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    for (layer, instance, chunks) in &layers {
        let spec = instance.spec.clone();
        let game = spec.game;
        let (storage, tx, lock) = (storage.clone(), events.tx.clone(), locks.get(game));
        let (min_y, height) = (chunks.min_y(), chunks.height());
        tokio.0.spawn(async move {
            let _guard = lock.lock().await;
            let result = load(&storage, &spec, min_y, height).await;
            let _ = tx.send(WorldEvent::Loaded { layer, game, result });
        });
    }
}

fn read_map(maps: &WorldStore, hash: Option<&str>, size: i32) -> anyhow::Result<BlockMirror> {
    match hash {
        Some(hash) => mm_world::decode(&maps.get(MapHash::parse(hash)?)?),
        None => Ok(BlockMirror::flat(Bounds::square(size), NEW_MAP_FLOOR)),
    }
}

/// Every chunk of the map plus one empty ring, so the edge doesn't look unloaded.
fn build_chunks(mirror: &BlockMirror, min_y: i32, height: u32) -> Chunks {
    let mut area = mirror.bounds;
    area.min[0] -= 16;
    area.min[2] -= 16;
    area.max[0] += 16;
    area.max[2] += 16;
    area.chunk_columns().map(|c| (c, mirror.chunk(c, min_y, height))).collect()
}

fn region_bounds(corners: &[mm_db::publish::Corners]) -> impl Iterator<Item = Bounds> + '_ {
    corners.iter().map(|(a, b)| Bounds::from_corners(*a, *b))
}

async fn load(storage: &Storage, spec: &InstanceSpec, min_y: i32, height: u32) -> anyhow::Result<Loaded> {
    let game = spec.game.0;
    let row = mm_db::game(&storage.db, game).await?.ok_or_else(|| anyhow::anyhow!("game {game} not found"))?;
    let persistent = spec.kind
        == GameKind::OpenWorld {
            persistent_world: true,
        };
    let save_players = row.save_player_state && matches!(spec.kind, GameKind::OpenWorld { .. });
    let mut regions = Vec::new();
    let mut region_map = None;
    let (map, persist, version, last_shutdown, update, save_players) = match spec.version {
        GameVersion::Draft => (row.draft_map_hash.clone(), Persist::Draft, None, None, None, false),
        GameVersion::Published(_) if persistent => {
            let live = mm_db::publish::claim_live_world(&storage.db, game)
                .await?
                .ok_or_else(|| anyhow::anyhow!("game {game} isn't published"))?;
            regions = live.regions;
            region_map = live.version_map;
            (live.map_hash, Persist::Live, Some(live.version), live.last_shutdown, live.update, save_players)
        }
        GameVersion::Published(v) => {
            let published = mm_db::publish::version(&storage.db, game, v)
                .await?
                .ok_or_else(|| anyhow::anyhow!("game {game} has no version {v}"))?;
            (published.map_hash, Persist::None, Some(v), None, None, save_players)
        }
    };
    let maps = storage.maps.clone();
    let size = row.map_size;
    tokio::task::spawn_blocking(move || {
        let mut mirror = read_map(&maps, map.as_deref(), size)?;
        if !regions.is_empty() {
            // Copied regions are dirty, so the next autosave keeps them.
            let source = read_map(&maps, region_map.as_deref(), size)?;
            for region in region_bounds(&regions) {
                for (p, state) in mirror.differences(&source, region) {
                    mirror.set(p, state);
                }
            }
        }
        let chunks = build_chunks(&mirror, min_y, height);
        Ok(Loaded {
            mirror,
            chunks,
            persist,
            version,
            last_shutdown,
            save_players,
            update,
        })
    })
    .await?
}

#[allow(clippy::too_many_arguments)]
fn apply_world_events(
    mut commands: Commands,
    mut events: ResMut<WorldEvents>,
    mut layers: Query<
        (&mut Instance, &mut ChunkLayer, &mut WorldBorderLerp, Option<&mut GameWorld>),
        Without<Despawned>,
    >,
    mut players: Query<(Entity, &InInstance, &mut Position, &mut Look, &mut Client)>,
    mut to_lobby: MessageWriter<SendToLobby>,
    mut stops: MessageWriter<StopRequest>,
    mut restarts: MessageWriter<RestartScripts>,
    link: Res<LobbyLink>,
) {
    while let Ok(event) = events.rx.try_recv() {
        match event {
            WorldEvent::Loaded { layer, game, result } => {
                let Ok((mut instance, mut chunks, mut border, _)) = layers.get_mut(layer) else {
                    continue; // stopped while loading
                };
                match result {
                    Ok(loaded) => {
                        for (pos, chunk) in loaded.chunks {
                            chunks.insert_chunk(pos, chunk);
                        }
                        let size = loaded.mirror.bounds.size()[0] as f64;
                        border.current_diameter = size;
                        border.target_diameter = size;
                        let spawn = loaded.mirror.spawn;
                        for (_, in_instance, mut pos, mut look, _) in &mut players {
                            if in_instance.0 == layer {
                                pos.set(spawn.pos);
                                look.yaw = spawn.yaw;
                                look.pitch = spawn.pitch;
                            }
                        }
                        if let Some(v) = loaded.version {
                            // A live world runs whatever version it caught up to.
                            instance.spec.version = GameVersion::Published(v);
                        }
                        if let Some(update) = loaded.update {
                            let v = loaded.version.unwrap_or(0);
                            report(&link, game, DiagnosticLevel::System, &format!("live world updated to v{v} ({})", update.as_str()));
                        }
                        commands.entity(layer).remove::<Loading>().insert(GameWorld {
                            game,
                            mirror: loaded.mirror,
                            persist: loaded.persist,
                            version: loaded.version,
                            last_shutdown: loaded.last_shutdown,
                            save_players: loaded.save_players,
                            last_save: Instant::now(),
                            saving: false,
                            updating: false,
                        });
                        tracing::info!("loaded world of game {} ({:?})", game.0, instance.spec.version);
                    }
                    Err(e) => {
                        tracing::error!("loading game {}: {e:#}", game.0);
                        report(&link, game, DiagnosticLevel::Error, &format!("couldn't load the map: {e:#}"));
                        for (entity, in_instance, .., mut client) in &mut players {
                            if in_instance.0 == layer {
                                client.send_chat_message("§cThis game's map couldn't be loaded. Returning you to the lobby.");
                                to_lobby.write(SendToLobby(entity));
                            }
                        }
                        stops.write(StopRequest {
                            instance: instance.spec.id,
                            graceful: false,
                        });
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
                    Ok(Some(hash)) => tracing::info!("saved world of game {} as {hash}", game.0),
                    Ok(None) => {}
                    Err((sections, e)) => {
                        tracing::error!("saving world of game {}: {e}", game.0);
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
            WorldEvent::DraftSaved { request, ok } => link.send(ShardMsg::DraftSaved { request, ok }),
            WorldEvent::Updated { layer, game, result } => {
                let Ok((mut instance, mut chunks, _, Some(mut world))) = layers.get_mut(layer) else {
                    continue;
                };
                world.updating = false;
                let (version, mode, change) = match result {
                    Ok(Some(update)) => update,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("updating the live world of game {}: {e:#}", game.0);
                        report(&link, game, DiagnosticLevel::Error, &format!("couldn't apply the publish: {e:#}"));
                        continue;
                    }
                };
                instance.spec.version = GameVersion::Published(version);
                world.version = Some(version);
                match change {
                    Change::Scripts => {}
                    Change::Writes(writes) => {
                        commands.entity(layer).insert(PendingWrites(writes.into()));
                    }
                    Change::Reset(mirror, loaded) => {
                        for (pos, chunk) in loaded {
                            chunks.insert_chunk(pos, chunk);
                        }
                        let spawn = mirror.spawn;
                        world.mirror = mirror;
                        world.last_save = Instant::now();
                        for (_, in_instance, mut pos, mut look, _) in &mut players {
                            if in_instance.0 == layer {
                                pos.set(spawn.pos);
                                look.yaw = spawn.yaw;
                                look.pitch = spawn.pitch;
                            }
                        }
                    }
                }
                let text = match mode {
                    LiveUpdate::Scripts => format!("§eThis world was updated to v{version}."),
                    LiveUpdate::Regions => format!("§eThis world was updated to v{version}; parts of the map were rebuilt."),
                    LiveUpdate::Reset => format!("§eThis world was reset for v{version}."),
                };
                for (.., in_instance, _, _, mut client) in &mut players {
                    if in_instance.0 == layer {
                        client.send_chat_message(text.clone());
                    }
                }
                report(&link, game, DiagnosticLevel::System, &format!("live world updated to v{version} ({})", mode.as_str()));
                restarts.write(RestartScripts { layer });
            }
        }
    }
}

fn report(link: &LobbyLink, game: GameId, level: DiagnosticLevel, message: &str) {
    link.send(ShardMsg::Diagnostic {
        game,
        level,
        message: message.to_owned(),
    });
}

/// Players entering an already-loaded world start at its spawn. (Players
/// who arrive while it loads are moved when loading finishes.)
fn spawn_arrivals(mut players: Query<(&InInstance, &mut Position, &mut Look), Added<InInstance>>, layers: Query<&GameWorld>) {
    for (in_instance, mut pos, mut look) in &mut players {
        if let Ok(world) = layers.get(in_instance.0) {
            pos.set(world.mirror.spawn.pos);
            look.yaw = world.mirror.spawn.yaw;
            look.pitch = world.mirror.spawn.pitch;
        }
    }
}

/// What a finished save reports.
enum SaveKind {
    Autosave,
    /// The instance stopped; report `InstanceEnded` after.
    Final(InstanceId),
    /// The lobby wants the draft saved before publishing it.
    Requested(u64),
    /// The shard is exiting.
    Shutdown,
}

/// Snapshots the mirror (cheap: sections are shared) and saves it on tokio.
fn start_save(
    layer: Entity,
    world: &mut GameWorld,
    kind: SaveKind,
    storage: &Storage,
    events: &WorldEvents,
    locks: &mut SaveLocks,
    tokio: &Tokio,
) -> JoinHandle<()> {
    let dirty = world.mirror.take_dirty();
    let snapshot = world.mirror.clone();
    world.saving = true;
    world.last_save = Instant::now();
    let (game, persist) = (world.game, world.persist);
    let (storage, tx, lock) = (storage.clone(), events.tx.clone(), locks.get(game));
    let shutdown = !matches!(kind, SaveKind::Autosave);
    tokio.0.spawn(async move {
        let _guard = lock.lock().await;
        let result = async {
            let maps = storage.maps.clone();
            let hash = tokio::task::spawn_blocking(move || maps.put(&mm_world::encode(&snapshot)?)).await??;
            match persist {
                Persist::Draft => mm_db::set_draft_map(&storage.db, game.0, &hash.to_string()).await?,
                Persist::Live => mm_db::publish::save_live_world(&storage.db, game.0, &hash.to_string(), shutdown).await?,
                Persist::None => {}
            }
            anyhow::Ok(Some(hash))
        }
        .await
        .map_err(|e| (dirty, format!("{e:#}")));
        if let SaveKind::Requested(request) = kind {
            let _ = tx.send(WorldEvent::DraftSaved {
                request,
                ok: result.is_ok(),
            });
        }
        let ended = match kind {
            SaveKind::Final(id) => Some(id),
            _ => None,
        };
        let _ = tx.send(WorldEvent::Saved {
            layer,
            game,
            result,
            ended,
        });
    })
}

fn autosave(
    mut layers: Query<(Entity, &mut GameWorld), Without<Despawned>>,
    interval: Res<Autosave>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    for (layer, mut world) in &mut layers {
        if world.persist != Persist::None
            && !world.saving
            && !world.updating
            && world.mirror.is_dirty()
            && world.last_save.elapsed() >= interval.0
        {
            start_save(layer, &mut world, SaveKind::Autosave, &storage, &events, &mut locks, &tokio);
        }
    }
}

/// Answers `SaveDraft`: saves now if anything changed, else confirms at once.
fn save_requested_drafts(
    mut requests: MessageReader<SaveDraft>,
    mut layers: Query<(Entity, &mut GameWorld), Without<Despawned>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    for request in requests.read() {
        let draft = layers.iter_mut().find(|(_, w)| w.game == request.game && w.is_draft());
        match draft {
            // An autosave in flight finishes first (same lock), so saving
            // again only writes what changed since it started.
            Some((layer, mut world)) if world.mirror.is_dirty() || world.saving => {
                start_save(layer, &mut world, SaveKind::Requested(request.request), &storage, &events, &mut locks, &tokio);
            }
            // A stopped draft's final save may still be running; it holds the lock.
            _ => {
                let (tx, lock, request) = (events.tx.clone(), locks.get(request.game), request.request);
                tokio.0.spawn(async move {
                    let _guard = lock.lock().await;
                    let _ = tx.send(WorldEvent::DraftSaved { request, ok: true });
                });
            }
        }
    }
}

/// Claims a publish for each running live world it reached and works out
/// the change off the main thread.
#[allow(clippy::too_many_arguments)]
fn start_updates(
    mut commands: Commands,
    mut publishes: MessageReader<ApplyPublish>,
    mut layers: Query<(Entity, &mut GameWorld, &ChunkLayer), Without<Despawned>>,
    loading: Query<(Entity, &Instance), With<Loading>>,
    queued: Query<Entity, (With<UpdateQueued>, With<GameWorld>)>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
) {
    let mut targets: Vec<Entity> = queued.iter().collect();
    for publish in publishes.read() {
        for (layer, instance) in &loading {
            // Its load may have claimed before the publish committed.
            if instance.spec.game == publish.game && instance.spec.version != GameVersion::Draft {
                commands.entity(layer).insert(UpdateQueued);
            }
        }
        targets.extend(
            layers
                .iter()
                .filter(|(_, w, _)| w.game == publish.game && w.persist == Persist::Live)
                .map(|(e, ..)| e),
        );
    }
    for layer in targets {
        commands.entity(layer).remove::<UpdateQueued>();
        let Ok((_, mut world, chunks)) = layers.get_mut(layer) else {
            continue;
        };
        if world.updating {
            commands.entity(layer).insert(UpdateQueued);
            continue;
        }
        world.updating = true;
        let snapshot = world.mirror.clone();
        let (game, storage, tx, lock) = (world.game, storage.clone(), events.tx.clone(), locks.get(world.game));
        let (min_y, height) = (chunks.min_y(), chunks.height());
        tokio.0.spawn(async move {
            let _guard = lock.lock().await;
            let result = async {
                let Some(live) = mm_db::publish::claim_live_world(&storage.db, game.0).await? else {
                    return Ok(None);
                };
                let Some(mode) = live.update else {
                    return Ok(None);
                };
                let size = snapshot.bounds.size()[0];
                let maps = storage.maps.clone();
                let change = tokio::task::spawn_blocking(move || -> anyhow::Result<Change> {
                    Ok(match mode {
                        LiveUpdate::Scripts => Change::Scripts,
                        LiveUpdate::Regions => {
                            let source = read_map(&maps, live.version_map.as_deref(), size)?;
                            let writes = region_bounds(&live.regions)
                                .flat_map(|r| snapshot.differences(&source, r))
                                .collect();
                            Change::Writes(writes)
                        }
                        LiveUpdate::Reset => {
                            let mirror = read_map(&maps, live.map_hash.as_deref(), size)?;
                            let chunks = build_chunks(&mirror, min_y, height);
                            Change::Reset(mirror, chunks)
                        }
                    })
                })
                .await??;
                anyhow::Ok(Some((live.version, mode, change)))
            }
            .await;
            let _ = tx.send(WorldEvent::Updated { layer, game, result });
        });
    }
}

/// Copies a regions update into the world a slice at a time.
fn write_updates(
    mut commands: Commands,
    mut layers: Query<(Entity, &mut PendingWrites, &mut GameWorld, &mut ChunkLayer, Option<&mut ScriptHost>)>,
    per_tick: Res<UpdateBlocksPerTick>,
) {
    for (layer, mut writes, mut world, mut chunks, host) in &mut layers {
        let n = writes.0.len().min(per_tick.0);
        for (p, state) in writes.0.drain(..n) {
            if world.mirror.set(p, state).is_some() {
                chunks.set_block(BlockPos::new(p[0], p[1], p[2]), state);
            }
        }
        if writes.0.is_empty() {
            commands.entity(layer).remove::<PendingWrites>();
        }
        if let Some(mut host) = host {
            host.mark_resync();
        }
    }
}

/// Stopped drafts and live worlds report `InstanceEnded` only after a final save.
fn save_stopped_worlds(
    mut layers: Query<(Entity, &Instance, Option<&mut GameWorld>), Added<Despawned>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
    link: Res<LobbyLink>,
) {
    for (layer, instance, world) in &mut layers {
        let id = instance.spec.id;
        let Some(mut world) = world else {
            link.send(ShardMsg::InstanceEnded { instance: id });
            continue;
        };
        match world.persist {
            Persist::None => link.send(ShardMsg::InstanceEnded { instance: id }),
            // A half-applied update may have replaced the map in Postgres;
            // this mirror must not overwrite it.
            Persist::Live if world.updating => {
                let (db, game, lock) = (storage.db.clone(), world.game, locks.get(world.game));
                let link_tx = events.tx.clone();
                tokio.0.spawn(async move {
                    let _guard = lock.lock().await;
                    if let Err(e) = mm_db::publish::mark_live_shutdown(&db, game.0).await {
                        tracing::error!("recording shutdown of game {}: {e}", game.0);
                    }
                    let _ = link_tx.send(WorldEvent::Saved {
                        layer,
                        game,
                        result: Err((HashSet::new(), "skipped: an update was in flight".into())),
                        ended: Some(id),
                    });
                });
            }
            _ if world.mirror.is_dirty() => {
                start_save(layer, &mut world, SaveKind::Final(id), &storage, &events, &mut locks, &tokio);
            }
            Persist::Live => {
                let (db, game, tx, lock) = (storage.db.clone(), world.game, events.tx.clone(), locks.get(world.game));
                tokio.0.spawn(async move {
                    let _guard = lock.lock().await;
                    if let Err(e) = mm_db::publish::mark_live_shutdown(&db, game.0).await {
                        tracing::error!("recording shutdown of game {}: {e}", game.0);
                    }
                    let _ = tx.send(WorldEvent::Saved {
                        layer,
                        game,
                        result: Ok(None),
                        ended: Some(id),
                    });
                });
            }
            Persist::Draft => link.send(ShardMsg::InstanceEnded { instance: id }),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn answer_shutdown(
    mut requests: ResMut<ShutdownRequests>,
    clients: Query<&UniqueId, With<Client>>,
    mut layers: Query<(Entity, &mut GameWorld), Without<Despawned>>,
    storage: Res<Storage>,
    events: Res<WorldEvents>,
    mut locks: ResMut<SaveLocks>,
    tokio: Res<Tokio>,
    mut player_saves: crate::players::ShutdownSaves,
) {
    while let Ok(reply) = requests.0.try_recv() {
        let mut saves: Vec<_> = layers
            .iter_mut()
            .filter(|(_, w)| w.persist != Persist::None && !w.updating && (w.mirror.is_dirty() || w.persist == Persist::Live))
            .map(|(layer, mut world)| {
                start_save(layer, &mut world, SaveKind::Shutdown, &storage, &events, &mut locks, &tokio)
            })
            .collect();
        saves.extend(player_saves.save_all());
        let _ = reply.send(ShutdownReply {
            players: clients.iter().map(|u| u.0).collect(),
            saves,
        });
    }
}
