//! Game instances: one layer each, created and stopped by the lobby. Players
//! arrive through Gate after the lobby's `ExpectPlayer` and land in the
//! instance they were expected in.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::prelude::*;
use chunkedge::UniqueId;
use mm_core::Tokio;
use chunkedge::world_border::WorldBorderBundle;
use mm_proto::ipc::{GameVersion, InstanceId, InstanceSpec, InstanceStatus, LobbyMsg, ShardMsg};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::link::{GateLink, LobbyLink};
use crate::world::Loading;

/// Matches the lobby's handoff timeout.
const EXPECT_TTL: Duration = Duration::from_secs(10);
const SPAWN: DVec3 = DVec3::new(0.5, 65.0, 0.5);
const PLATFORM_RADIUS: i32 = 12;
const VOID_Y: f64 = 0.0;
const LOBBY_COMMANDS: [&str; 4] = ["lobby", "hub", "l", "leave"];

#[derive(Component)]
pub struct Instance {
    pub spec: InstanceSpec,
    started: bool,
    empty_since: Option<Instant>,
}

impl Instance {
    pub fn is_draft(&self) -> bool {
        self.spec.version == GameVersion::Draft
    }
}

/// The instance systems; world and build systems run after them.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InstanceSet;

/// Holding layer for players who arrived unexpected or whose instance stopped,
/// while Gate moves them back to the lobby.
#[derive(Component)]
struct Limbo;

/// The instance layer a player is in.
#[derive(Component)]
pub struct InInstance(pub Entity);

struct Expectation {
    instance: InstanceId,
    deadline: Instant,
}

#[derive(Resource, Default)]
struct Instances {
    by_id: HashMap<InstanceId, Entity>,
    expected: HashMap<Uuid, Expectation>,
}

impl Instances {
    fn expected_in(&self, id: InstanceId) -> u32 {
        self.expected.values().filter(|e| e.instance == id).count() as u32
    }
}

#[derive(Resource)]
pub struct IdleUnload(pub Duration);

/// Ask Gate to move a player back to the lobby.
#[derive(Message, Clone, Copy)]
pub struct SendToLobby(pub Entity);

#[derive(Resource)]
struct LobbyTransfers {
    failed: (UnboundedSender<Entity>, UnboundedReceiver<Entity>),
}

/// Recent tick durations for the heartbeat's p99.
#[derive(Resource, Default)]
struct TickTimes {
    start: Option<Instant>,
    samples: std::collections::VecDeque<f32>,
}

pub struct InstancePlugin;

impl Plugin for InstancePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Instances>()
            .init_resource::<TickTimes>()
            .insert_resource(LobbyTransfers {
                failed: mpsc::unbounded_channel(),
            })
            .add_message::<SendToLobby>()
            .add_systems(Startup, spawn_limbo)
            .add_systems(First, start_tick_timer)
            .add_systems(
                Update,
                (
                    despawn_disconnected_clients,
                    apply_lobby_messages,
                    place_arrivals,
                    evacuate_orphans,
                    lobby_commands,
                    send_to_lobby,
                    return_from_void,
                    unload_idle,
                    heartbeat,
                )
                    .chain()
                    .in_set(InstanceSet),
            )
            .add_systems(Last, end_tick_timer);
    }
}

fn spawn_limbo(
    mut commands: Commands,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
) {
    let layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
    commands.spawn((layer, Limbo));
}

/// A placeholder world until published maps load from `.mmw` files.
fn build_layer(server: &Server, dimensions: &DimensionTypeRegistry, biomes: &BiomeRegistry) -> LayerBundle {
    let mut layer = LayerBundle::new(ident!("overworld"), dimensions, biomes, server);
    let chunks = (PLATFORM_RADIUS + 15) / 16 + 1;
    for z in -chunks..chunks {
        for x in -chunks..chunks {
            layer.chunk.insert_chunk([x, z], UnloadedChunk::new());
        }
    }
    for z in -PLATFORM_RADIUS..PLATFORM_RADIUS {
        for x in -PLATFORM_RADIUS..PLATFORM_RADIUS {
            let edge = x == -PLATFORM_RADIUS || z == -PLATFORM_RADIUS || x == PLATFORM_RADIUS - 1 || z == PLATFORM_RADIUS - 1;
            let block = if edge { BlockState::STONE_BRICKS } else { BlockState::SMOOTH_STONE };
            layer.chunk.set_block([x, 64, z], block);
        }
    }
    layer
}

#[allow(clippy::too_many_arguments)]
fn apply_lobby_messages(
    mut commands: Commands,
    mut link: ResMut<LobbyLink>,
    mut instances: ResMut<Instances>,
    layers: Query<&Instance>,
    players: Query<&InInstance, With<Client>>,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
    mut stops: Local<Vec<InstanceId>>,
) {
    while let Ok(msg) = link.inbox.try_recv() {
        match msg {
            LobbyMsg::CreateInstance(spec) => {
                if instances.by_id.contains_key(&spec.id) {
                    continue;
                }
                tracing::info!("creating instance {} of game {} ({:?})", spec.id.0, spec.game.0, spec.version);
                let id = spec.id;
                let draft = spec.version == GameVersion::Draft;
                let instance = Instance {
                    spec,
                    started: false,
                    empty_since: Some(Instant::now()),
                };
                let entity = if draft {
                    // Chunks arrive when the map finishes loading (see `world`).
                    let layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
                    commands.spawn((layer, WorldBorderBundle::default(), instance, Loading)).id()
                } else {
                    commands.spawn((build_layer(&server, &dimensions, &biomes), instance)).id()
                };
                instances.by_id.insert(id, entity);
            }
            LobbyMsg::ExpectPlayer { uuid, instance, ticket } => {
                // Instances spawned earlier this tick aren't queryable yet,
                // but have no players either.
                let accepted = instances.by_id.get(&instance).is_some_and(|&layer| {
                    let max = layers.get(layer).map_or(u32::MAX, |i| i.spec.max_players);
                    let players = players.iter().filter(|p| p.0 == layer).count() as u32;
                    players + instances.expected_in(instance) < max
                });
                if accepted {
                    instances.expected.insert(
                        uuid,
                        Expectation {
                            instance,
                            deadline: Instant::now() + EXPECT_TTL,
                        },
                    );
                }
                link.send(ShardMsg::ExpectAck { ticket, accepted });
            }
            LobbyMsg::StopInstance { instance } => stops.push(instance),
            LobbyMsg::KillGame { game } => {
                stops.extend(layers.iter().filter(|i| i.spec.game == game).map(|i| i.spec.id));
            }
        }
    }
    for id in stops.drain(..) {
        let draft = instances
            .by_id
            .get(&id)
            .and_then(|e| layers.get(*e).ok())
            .is_some_and(Instance::is_draft);
        stop_instance(&mut commands, &mut instances, &link, id, draft);
    }
}

/// Despawns an instance. Its players are moved to limbo and sent to the
/// lobby by [`evacuate_orphans`]. Drafts report `InstanceEnded` after their
/// final save instead (see `world::save_stopped_drafts`).
fn stop_instance(commands: &mut Commands, instances: &mut Instances, link: &LobbyLink, id: InstanceId, draft: bool) {
    let Some(entity) = instances.by_id.remove(&id) else {
        return;
    };
    tracing::info!("stopping instance {}", id.0);
    instances.expected.retain(|_, e| e.instance != id);
    commands.entity(entity).insert(Despawned);
    if !draft {
        link.send(ShardMsg::InstanceEnded { instance: id });
    }
}

fn place_arrivals(
    mut commands: Commands,
    mut clients: Query<
        (
            Entity,
            &mut Client,
            &UniqueId,
            &mut EntityLayerId,
            &mut VisibleChunkLayer,
            &mut VisibleEntityLayers,
            &mut Position,
            &mut GameMode,
        ),
        Added<Client>,
    >,
    mut instances: ResMut<Instances>,
    layers: Query<&Instance>,
    limbo: Single<Entity, With<Limbo>>,
    mut to_lobby: MessageWriter<SendToLobby>,
) {
    for (entity, mut client, uuid, mut layer_id, mut chunks, mut entities, mut pos, mut mode) in &mut clients {
        let target = instances
            .expected
            .remove(&uuid.0)
            .and_then(|e| instances.by_id.get(&e.instance).copied());
        let layer = match target {
            Some(layer) => layer,
            None => {
                tracing::info!("unexpected player {}; sending to lobby", uuid.0);
                client.send_chat_message("§cYou weren't expected on this server. Returning you to the lobby...");
                to_lobby.write(SendToLobby(entity));
                *limbo
            }
        };
        layer_id.0 = layer;
        chunks.0 = layer;
        entities.0.insert(layer);
        pos.set(SPAWN);
        *mode = GameMode::Adventure;
        if target.is_some() {
            commands.entity(entity).insert(InInstance(layer));
            if let Ok(instance) = layers.get(layer) {
                client.send_chat_message(format!(
                    "§aJoined game #{} §8(instance {}) §7· /lobby to leave",
                    instance.spec.game.0, instance.spec.id.0
                ));
                if instance.is_draft() {
                    *mode = GameMode::Creative;
                    commands.entity(entity).insert(crate::build::Builder::default());
                    client.send_chat_message(
                        "§7Editing the draft. Building tools: §f//wand //pos1 //pos2 //set //replace //copy //paste //undo",
                    );
                }
            }
        }
    }
}

fn lobby_commands(mut commands: MessageReader<CommandExecutionMessage>, mut to_lobby: MessageWriter<SendToLobby>) {
    for cmd in commands.read() {
        let name = cmd.command.split_whitespace().next().unwrap_or("");
        if LOBBY_COMMANDS.iter().any(|c| c.eq_ignore_ascii_case(name)) {
            to_lobby.write(SendToLobby(cmd.executor));
        }
    }
}

/// Moves players whose instance was despawned into limbo and back to the lobby.
fn evacuate_orphans(
    mut commands: Commands,
    mut players: Query<
        (Entity, &InInstance, &mut EntityLayerId, &mut VisibleChunkLayer, &mut VisibleEntityLayers),
        With<Client>,
    >,
    layers: Query<(), (With<Instance>, Without<Despawned>)>,
    limbo: Single<Entity, With<Limbo>>,
    mut to_lobby: MessageWriter<SendToLobby>,
) {
    for (entity, in_instance, mut layer_id, mut chunks, mut entities) in &mut players {
        if layers.contains(in_instance.0) {
            continue;
        }
        entities.0.remove(&in_instance.0);
        layer_id.0 = *limbo;
        chunks.0 = *limbo;
        entities.0.insert(*limbo);
        commands.entity(entity).remove::<InInstance>();
        to_lobby.write(SendToLobby(entity));
    }
}

fn send_to_lobby(
    mut requests: MessageReader<SendToLobby>,
    mut clients: Query<(&mut Client, &UniqueId)>,
    mut transfers: ResMut<LobbyTransfers>,
    gate: Res<GateLink>,
    tokio: Res<Tokio>,
) {
    for SendToLobby(entity) in requests.read() {
        let Ok((_, uuid)) = clients.get(*entity) else {
            continue;
        };
        let (gate, player, entity, failed) = (
            gate.clone(),
            uuid.0.hyphenated().to_string(),
            *entity,
            transfers.failed.0.clone(),
        );
        tokio.0.spawn(async move {
            if let Err(e) = gate.client.connect_player(&player, &gate.lobby_server).await {
                tracing::warn!("sending {player} to the lobby: {e}");
                let _ = failed.send(entity);
            }
        });
    }
    while let Ok(entity) = transfers.failed.1.try_recv() {
        if let Ok((mut client, _)) = clients.get_mut(entity) {
            client.send_chat_message("§cCouldn't reach the lobby. Try /lobby again in a moment.");
        }
    }
}

fn return_from_void(mut clients: Query<&mut Position, (With<Client>, With<InInstance>)>) {
    for mut pos in &mut clients {
        if pos.0.y < VOID_Y {
            pos.set(SPAWN);
        }
    }
}

fn unload_idle(
    mut commands: Commands,
    mut instances: ResMut<Instances>,
    mut layers: Query<(Entity, &mut Instance), Without<Despawned>>,
    players: Query<&InInstance, With<Client>>,
    link: Res<LobbyLink>,
    idle: Res<IdleUnload>,
) {
    let now = Instant::now();
    instances.expected.retain(|_, e| now < e.deadline);

    let mut idle_ids = Vec::new();
    for (entity, mut instance) in &mut layers {
        let occupied = players.iter().any(|p| p.0 == entity) || instances.expected_in(instance.spec.id) > 0;
        match (occupied, instance.empty_since) {
            (true, _) => instance.empty_since = None,
            (false, None) => instance.empty_since = Some(now),
            (false, Some(since)) if now - since >= idle.0 => idle_ids.push((instance.spec.id, instance.is_draft())),
            (false, Some(_)) => {}
        }
    }
    for (id, draft) in idle_ids {
        stop_instance(&mut commands, &mut instances, &link, id, draft);
    }
}

fn heartbeat(
    link: Res<LobbyLink>,
    instances: Res<Instances>,
    layers: Query<(Entity, &Instance), Without<Despawned>>,
    players: Query<&InInstance, With<Client>>,
    times: Res<TickTimes>,
    server: Res<Server>,
) {
    if server.current_tick() % 20 != 0 {
        return;
    }
    let mut counts: HashMap<Entity, u32> = HashMap::new();
    for p in &players {
        *counts.entry(p.0).or_default() += 1;
    }
    let statuses: Vec<_> = layers
        .iter()
        .map(|(entity, i)| InstanceStatus {
            spec: i.spec.clone(),
            players: counts.get(&entity).copied().unwrap_or(0),
            expected: instances.expected_in(i.spec.id),
            started: i.started,
        })
        .collect();
    *link.snapshot.lock().unwrap() = statuses.clone();
    link.send(ShardMsg::Heartbeat {
        instances: statuses,
        tick_p99_ms: times.p99(),
    });
}

impl TickTimes {
    fn p99(&self) -> f32 {
        let mut sorted: Vec<f32> = self.samples.iter().copied().collect();
        if sorted.is_empty() {
            return 0.0;
        }
        sorted.sort_by(f32::total_cmp);
        sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)]
    }
}

fn start_tick_timer(mut times: ResMut<TickTimes>) {
    times.start = Some(Instant::now());
}

fn end_tick_timer(mut times: ResMut<TickTimes>) {
    if let Some(start) = times.start.take() {
        if times.samples.len() == 200 {
            times.samples.pop_front();
        }
        times.samples.push_back(start.elapsed().as_secs_f32() * 1000.0);
    }
}
