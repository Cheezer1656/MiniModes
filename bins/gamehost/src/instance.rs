//! Game instances: one layer each, created and stopped by the lobby. Players
//! arrive through Gate after the lobby's `ExpectPlayer` and land in the
//! instance they were expected in.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::message::ChatReceivedMessage;
use chunkedge::prelude::*;
use chunkedge::UniqueId;
use mm_core::Tokio;
use mm_core::metrics::TickTimes;
use chunkedge::world_border::WorldBorderBundle;
use mm_proto::ipc::{GameVersion, InstanceId, InstanceSpec, InstanceStatus, LobbyMsg, ShardMsg};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::link::{GateLink, LobbyLink};
use crate::world::Loading;

/// Matches the lobby's handoff timeout.
const EXPECT_TTL: Duration = Duration::from_secs(10);
/// Where players wait until the map has loaded.
const SPAWN: DVec3 = DVec3::new(0.5, 65.0, 0.5);
const VOID_Y: f64 = 0.0;
/// How long `game:BindToClose` callbacks get before the instance goes anyway.
const CLOSE_GRACE: Duration = Duration::from_secs(5);
const LOBBY_COMMANDS: [&str; 4] = ["lobby", "hub", "l", "leave"];

#[derive(Component)]
pub struct Instance {
    pub spec: InstanceSpec,
    /// A match that has started; no one else may join.
    pub started: bool,
    empty_since: Option<Instant>,
    /// Stopping: players were sent away and `BindToClose` callbacks run
    /// until they finish or this deadline passes.
    pub closing: Option<Instant>,
}

impl Instance {
    #[cfg(test)]
    pub fn new_for_test(spec: InstanceSpec) -> Self {
        Self {
            spec,
            started: false,
            empty_since: None,
            closing: None,
        }
    }

    pub fn is_draft(&self) -> bool {
        self.spec.version == GameVersion::Draft
    }
}

/// Stop an instance from inside the shard (a finished match, a failed load).
#[derive(Message, Clone, Copy)]
pub struct StopRequest {
    pub instance: InstanceId,
    /// Let scripts run their `BindToClose` callbacks first.
    pub graceful: bool,
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

pub struct InstancePlugin;

impl Plugin for InstancePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Instances>()
            .insert_resource(LobbyTransfers {
                failed: mpsc::unbounded_channel(),
            })
            .add_message::<SendToLobby>()
            .add_message::<StopRequest>()
            .add_systems(Startup, spawn_limbo)
            .add_systems(
                Update,
                (
                    despawn_disconnected_clients,
                    apply_lobby_messages,
                    place_arrivals,
                    evacuate_orphans,
                    lobby_commands,
                    instance_chat,
                    send_to_lobby,
                    return_from_void,
                    unload_idle,
                    finish_closing,
                    heartbeat,
                )
                    .chain()
                    .in_set(InstanceSet),
            );
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
    mut stops: MessageReader<StopRequest>,
    mut reloads: MessageWriter<crate::script::ReloadDraft>,
    mut saves: MessageWriter<crate::world::SaveDraft>,
    mut publishes: MessageWriter<crate::world::ApplyPublish>,
    mut quotas: ResMut<crate::moderation::RankQuotas>,
) {
    let mut stopping: Vec<(InstanceId, bool)> = stops.read().map(|s| (s.instance, s.graceful)).collect();
    while let Ok(msg) = link.inbox.try_recv() {
        match msg {
            LobbyMsg::CreateInstance(spec) => {
                if instances.by_id.contains_key(&spec.id) {
                    continue;
                }
                tracing::info!("creating instance {} of game {} ({:?})", spec.id.0, spec.game.0, spec.version);
                let id = spec.id;
                let instance = Instance {
                    spec,
                    started: false,
                    empty_since: Some(Instant::now()),
                    closing: None,
                };
                // Chunks arrive when the map finishes loading (see `world`).
                let layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
                let entity = commands.spawn((layer, WorldBorderBundle::default(), instance, Loading)).id();
                instances.by_id.insert(id, entity);
            }
            LobbyMsg::ExpectPlayer { uuid, instance, ticket } => {
                // Instances spawned earlier this tick aren't queryable yet,
                // but have no players either.
                let accepted = instances.by_id.get(&instance).is_some_and(|&layer| {
                    let (max, open) = layers
                        .get(layer)
                        .map_or((u32::MAX, true), |i| (i.spec.max_players, !i.started && i.closing.is_none()));
                    let players = players.iter().filter(|p| p.0 == layer).count() as u32;
                    open && players + instances.expected_in(instance) < max
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
            LobbyMsg::StopInstance { instance } => stopping.push((instance, true)),
            // Moderation: no callbacks.
            LobbyMsg::KillGame { game } => {
                stopping.extend(layers.iter().filter(|i| i.spec.game == game).map(|i| (i.spec.id, false)));
            }
            LobbyMsg::ReloadDraft { game, by } => {
                reloads.write(crate::script::ReloadDraft { game, by });
            }
            LobbyMsg::SaveDraft { game, request } => {
                saves.write(crate::world::SaveDraft { game, request });
            }
            LobbyMsg::ApplyPublish { game, version } => {
                tracing::info!("game {} published v{version}", game.0);
                publishes.write(crate::world::ApplyPublish { game });
            }
            LobbyMsg::RankQuotas { default, ranks } => {
                quotas.default = Some(default);
                quotas.ranks = ranks.into_iter().collect();
            }
        }
    }
    for (id, graceful) in stopping {
        let Some(&entity) = instances.by_id.get(&id) else {
            continue;
        };
        if graceful {
            begin_closing(&mut commands, entity, id);
        } else {
            despawn_instance(&mut commands, &mut instances, id);
        }
    }
}

/// Marks an instance as closing: its players go to the lobby and its
/// scripts get [`CLOSE_GRACE`] for `BindToClose` before [`finish_closing`]
/// despawns it.
fn begin_closing(commands: &mut Commands, entity: Entity, id: InstanceId) {
    tracing::info!("closing instance {}", id.0);
    commands.queue(move |world: &mut World| {
        if let Some(mut instance) = world.get_mut::<Instance>(entity)
            && instance.closing.is_none()
        {
            instance.closing = Some(Instant::now() + CLOSE_GRACE);
        }
    });
}

/// Despawns an instance now. Its players are moved to limbo and sent to the
/// lobby by [`evacuate_orphans`]; `InstanceEnded` follows any final save
/// (see `world::save_stopped_worlds`).
fn despawn_instance(commands: &mut Commands, instances: &mut Instances, id: InstanceId) {
    let Some(entity) = instances.by_id.remove(&id) else {
        return;
    };
    tracing::info!("stopping instance {}", id.0);
    instances.expected.retain(|_, e| e.instance != id);
    commands.entity(entity).insert(Despawned);
}

fn finish_closing(
    mut commands: Commands,
    mut instances: ResMut<Instances>,
    layers: Query<(&Instance, Option<&crate::script::ScriptHost>), Without<Despawned>>,
) {
    let now = Instant::now();
    let done: Vec<_> = layers
        .iter()
        .filter(|(i, host)| i.closing.is_some_and(|deadline| now >= deadline || host.is_none_or(|h| h.closed())))
        .map(|(i, _)| i.spec.id)
        .collect();
    for id in done {
        despawn_instance(&mut commands, &mut instances, id);
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
                let version = match instance.spec.version {
                    GameVersion::Draft => "draft".to_owned(),
                    GameVersion::Published(v) => format!("v{v}"),
                };
                client.send_chat_message(format!(
                    "§aJoined game #{} {version} §8(instance {}) §7· /lobby to leave",
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

/// Chat reaches the other players in the same instance.
fn instance_chat(
    mut messages: MessageReader<ChatReceivedMessage>,
    players: Query<(&InInstance, &Username)>,
    mut layers: Query<&mut ChunkLayer, With<Instance>>,
) {
    for msg in messages.read() {
        let Ok((in_instance, name)) = players.get(msg.client) else {
            continue;
        };
        if let Ok(mut layer) = layers.get_mut(in_instance.0) {
            layer.send_chat_message(format!("§7{}§8: §f{}", name.0, msg.message));
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

/// Moves players whose instance was despawned or is closing into limbo and
/// back to the lobby.
fn evacuate_orphans(
    mut commands: Commands,
    mut players: Query<
        (Entity, &InInstance, &mut EntityLayerId, &mut VisibleChunkLayer, &mut VisibleEntityLayers),
        With<Client>,
    >,
    layers: Query<&Instance, Without<Despawned>>,
    limbo: Single<Entity, With<Limbo>>,
    mut to_lobby: MessageWriter<SendToLobby>,
) {
    for (entity, in_instance, mut layer_id, mut chunks, mut entities) in &mut players {
        if layers.get(in_instance.0).is_ok_and(|i| i.closing.is_none()) {
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

fn return_from_void(
    mut clients: Query<(&InInstance, &mut Position), With<Client>>,
    worlds: Query<&crate::world::GameWorld>,
) {
    for (in_instance, mut pos) in &mut clients {
        if pos.0.y < VOID_Y {
            pos.set(worlds.get(in_instance.0).map_or(SPAWN, |w| DVec3::from(w.mirror.spawn.pos)));
        }
    }
}

fn unload_idle(
    mut commands: Commands,
    mut instances: ResMut<Instances>,
    mut layers: Query<(Entity, &mut Instance), Without<Despawned>>,
    players: Query<&InInstance, With<Client>>,
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
            (false, Some(since)) if now - since >= idle.0 && instance.closing.is_none() => idle_ids.push(entity),
            (false, Some(_)) => {}
        }
    }
    for entity in idle_ids {
        if let Ok((_, instance)) = layers.get(entity) {
            begin_closing(&mut commands, entity, instance.spec.id);
        }
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
            started: i.started || i.closing.is_some(),
        })
        .collect();
    *link.snapshot.lock().unwrap() = statuses.clone();
    link.send(ShardMsg::Heartbeat {
        instances: statuses,
        tick_p99_ms: times.quantile(0.99),
    });
}
