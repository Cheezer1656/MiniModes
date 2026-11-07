//! Script hosting: each instance with scripts owns one [`Vm`], stepped once
//! per tick on the async compute pool.
//!
//! The pipeline runs in two halves around the build systems:
//! - [`ScriptSet::Apply`]: take finished steps back and apply their commands,
//!   drain queued block writes, and register arrivals and departures.
//! - (building and rule enforcement push block events into the inbox)
//! - [`ScriptSet::Spawn`]: collect chat, snapshot the players, and spawn the
//!   next step for every idle VM with work. A step still running next tick
//!   just keeps its events queued.

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;

use bevy_tasks::{AsyncComputeTaskPool, Task, block_on, poll_once};
use chunkedge::prelude::bevy_ecs::system::SystemParam;
use chunkedge::UniqueId;
use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::entity::living::Health;
use chunkedge::entity::player::Food;
use chunkedge::ident::Ident;
use chunkedge::inventory::HeldItem;
use chunkedge::message::ChatReceivedMessage;
use chunkedge::prelude::*;
use chunkedge::protocol::sound::{Sound, SoundCategory};
use chunkedge::status::RequestRespawnMessage;
use mm_core::Tokio;
use mm_proto::GameId;
use mm_proto::ipc::{DiagnosticLevel, ShardMsg};
use mm_script::{Bundle, Command, Event, Limits, PlayerId, PlayerSnap, RuleSet, Snapshot, StepOutput, Vm};
use mm_world::BlockMirror;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::instance::{InInstance, Instance, SendToLobby};
use crate::link::LobbyLink;
use crate::world::{DraftWorld, Storage};

#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScriptSet {
    Apply,
    Spawn,
}

#[derive(Resource, Clone)]
pub struct ScriptConfig {
    pub limits: Limits,
    /// Script block writes applied per instance per tick; the rest wait.
    pub blocks_per_tick: usize,
}

/// The rules scripts set, enforced by the build systems. Present on every
/// instance; without scripts nothing may be broken or placed (creative
/// builders ignore rules).
#[derive(Component, Default)]
pub struct GameRules(pub RuleSet);

type StepResult = (Box<Vm>, Result<StepOutput, String>);

enum Slot {
    Idle(Box<Vm>),
    Running(Task<StepResult>),
    /// Between taking the VM out and putting it back.
    Empty,
}

#[derive(Component)]
pub struct ScriptHost {
    slot: Slot,
    inbox: Vec<Event>,
    ids: HashMap<Entity, PlayerId>,
    entities: HashMap<PlayerId, Entity>,
    next_id: PlayerId,
    pending_blocks: VecDeque<([i32; 3], BlockState)>,
    /// The VM's block copy is stale (map edited outside scripts).
    resync: bool,
    runnable: bool,
    next_timer: Option<u64>,
}

impl ScriptHost {
    fn new(vm: Box<Vm>) -> Self {
        Self {
            slot: Slot::Idle(vm),
            inbox: Vec::new(),
            ids: HashMap::new(),
            entities: HashMap::new(),
            next_id: 1,
            pending_blocks: VecDeque::new(),
            resync: false,
            runnable: true,
            next_timer: None,
        }
    }

    /// Queues an event about `player`, if the VM knows them.
    pub fn player_event(&mut self, player: Entity, event: impl FnOnce(PlayerId) -> Event) {
        if let Some(&id) = self.ids.get(&player) {
            self.inbox.push(event(id));
        }
    }

    /// The map changed in ways the VM didn't see (e.g. WorldEdit).
    pub fn mark_resync(&mut self) {
        self.resync = true;
    }
}

enum LoadEvent {
    Loaded {
        layer: Entity,
        game: GameId,
        generation: u64,
        result: Result<Option<Box<Vm>>, String>,
    },
}

#[derive(Resource)]
struct Loads {
    tx: UnboundedSender<LoadEvent>,
    rx: UnboundedReceiver<LoadEvent>,
}

/// Bumped on every (re)load, so a slow stale load is ignored.
#[derive(Component)]
struct ScriptGeneration(u64);

pub struct ScriptPlugin;

impl Plugin for ScriptPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(Loads { tx, rx })
            .add_systems(
                Update,
                (
                    load_new_drafts,
                    script_commands,
                    apply_loads,
                    poll_steps,
                    apply_pending_blocks,
                    track_members,
                    respawn,
                )
                    .chain()
                    .in_set(ScriptSet::Apply),
            )
            .add_systems(Update, (collect_chat, spawn_steps).chain().in_set(ScriptSet::Spawn));
    }
}

/// What starting a script load needs.
#[derive(SystemParam)]
struct Loader<'w> {
    storage: Res<'w, Storage>,
    loads: Res<'w, Loads>,
    config: Res<'w, ScriptConfig>,
    tokio: Res<'w, Tokio>,
}

impl Loader<'_> {
    /// Loads the draft's scripts and builds a VM off the main thread.
    fn start(&self, layer: Entity, game: GameId, generation: u64, blocks: BlockMirror) {
        let (db, tx, limits) = (self.storage.db.clone(), self.loads.tx.clone(), self.config.limits.clone());
        self.tokio.0.spawn(async move {
            let result = async {
                let files = mm_db::draft_files(&db, game.0).await.map_err(|e| format!("loading scripts: {e}"))?;
                if files.is_empty() {
                    return Ok(None);
                }
                let bundle = Bundle::new(files)?;
                tokio::task::spawn_blocking(move || Vm::new(bundle, limits, blocks).map(|vm| Some(Box::new(vm))))
                    .await
                    .map_err(|e| e.to_string())?
            }
            .await;
            let _ = tx.send(LoadEvent::Loaded {
                layer,
                game,
                generation,
                result,
            });
        });
    }
}

/// Drafts start their scripts once the map is loaded.
fn load_new_drafts(
    mut commands: Commands,
    layers: Query<(Entity, &Instance, &DraftWorld), Added<DraftWorld>>,
    loader: Loader,
) {
    for (layer, instance, world) in &layers {
        commands.entity(layer).insert((ScriptGeneration(1), GameRules::default()));
        loader.start(layer, instance.spec.game, 1, world.mirror.clone());
    }
}

/// `/script reload|stop` inside a draft.
fn script_commands(
    mut commands: Commands,
    mut messages: MessageReader<CommandExecutionMessage>,
    mut players: Query<(&mut Client, &InInstance)>,
    mut layers: Query<(&Instance, &DraftWorld, &mut ScriptGeneration, &mut GameRules)>,
    loader: Loader,
) {
    for cmd in messages.read() {
        let mut args = cmd.command.split_whitespace();
        if args.next() != Some("script") {
            continue;
        }
        let Ok((mut client, in_instance)) = players.get_mut(cmd.executor) else {
            continue;
        };
        let Ok((instance, world, mut generation, mut rules)) = layers.get_mut(in_instance.0) else {
            client.send_chat_message("§cScripts can only be controlled while editing a draft.");
            continue;
        };
        match args.next() {
            Some("reload") => {
                generation.0 += 1;
                rules.0 = RuleSet::default();
                commands.entity(in_instance.0).remove::<ScriptHost>();
                loader.start(in_instance.0, instance.spec.game, generation.0, world.mirror.clone());
                client.send_chat_message("§7Reloading scripts...");
            }
            Some("stop") => {
                generation.0 += 1;
                rules.0 = RuleSet::default();
                commands.entity(in_instance.0).remove::<ScriptHost>();
                client.send_chat_message("§7Scripts stopped. §f/script reload§7 starts them again.");
            }
            _ => client.send_chat_message("§7Usage: §f/script reload§7 or §f/script stop"),
        }
    }
}

fn apply_loads(
    mut commands: Commands,
    mut loads: ResMut<Loads>,
    layers: Query<&ScriptGeneration, Without<Despawned>>,
    mut players: Query<(&InInstance, &mut Client)>,
    link: Res<LobbyLink>,
) {
    while let Ok(LoadEvent::Loaded {
        layer,
        game,
        generation,
        result,
    }) = loads.rx.try_recv()
    {
        let Ok(current) = layers.get(layer) else {
            continue;
        };
        if current.0 != generation {
            continue;
        }
        let notice = match result {
            Ok(Some(vm)) => {
                commands.entity(layer).insert(ScriptHost::new(vm));
                "§aScripts started.".to_owned()
            }
            Ok(None) => continue,
            Err(e) => {
                report(&link, game, DiagnosticLevel::Error, &e);
                format!("§cScripts failed to start: §7{e}")
            }
        };
        for (in_instance, mut client) in &mut players {
            if in_instance.0 == layer {
                client.send_chat_message(notice.clone());
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

type PlayerParts<'a> = (
    &'a mut Client,
    &'a mut Position,
    &'a mut Look,
    &'a mut GameMode,
    &'a mut Health,
    &'a mut Food,
    &'a mut Inventory,
);

#[allow(clippy::too_many_arguments)]
fn poll_steps(
    mut commands: Commands,
    mut layers: Query<(Entity, &Instance, &mut ScriptHost, &mut GameRules, &mut ChunkLayer, &mut DraftWorld)>,
    mut players: Query<PlayerParts, With<InInstance>>,
    mut to_lobby: MessageWriter<SendToLobby>,
    link: Res<LobbyLink>,
) {
    for (layer, instance, mut host, mut rules, mut chunks, mut world) in &mut layers {
        let Slot::Running(task) = &mut host.slot else {
            continue;
        };
        let Some((vm, result)) = block_on(poll_once(task)) else {
            continue;
        };
        let game = instance.spec.game;
        let out = match result {
            Ok(out) => out,
            Err(panic) => StepOutput {
                fatal: Some(format!("the script runtime crashed: {panic}")),
                ..StepOutput::default()
            },
        };
        host.slot = Slot::Idle(vm);
        host.runnable = out.runnable;
        host.next_timer = out.next_timer;

        let mut chat = Vec::new();
        for d in out.diagnostics {
            let color = match d.level {
                DiagnosticLevel::Info => "§7",
                DiagnosticLevel::Warn => "§e",
                DiagnosticLevel::Error => "§c",
            };
            chat.push(format!("§8[script] {color}{}", d.message));
            // Prints stay in the draft's chat until the editor console
            // (phase 6) streams them.
            if d.level != DiagnosticLevel::Info {
                report(&link, game, d.level, &d.message);
            }
        }
        for command in out.commands {
            apply_command(command, &mut host, &mut rules, &mut chunks, &mut world, &mut players, &mut to_lobby);
        }
        if let Some(reason) = out.fatal {
            tracing::warn!("stopping scripts of game {}: {reason}", game.0);
            report(&link, game, DiagnosticLevel::Error, &format!("scripts stopped: {reason}"));
            chat.push(format!("§cScripts stopped: §7{reason}"));
            rules.0 = RuleSet::default();
            commands.entity(layer).remove::<ScriptHost>();
        }
        if !chat.is_empty() && instance.is_draft() {
            for &entity in host.entities.values() {
                if let Ok((mut client, ..)) = players.get_mut(entity) {
                    for line in &chat {
                        client.send_chat_message(line.clone());
                    }
                }
            }
        }
    }
}

fn apply_command(
    command: Command,
    host: &mut ScriptHost,
    rules: &mut GameRules,
    chunks: &mut ChunkLayer,
    world: &mut DraftWorld,
    players: &mut Query<PlayerParts, With<InInstance>>,
    to_lobby: &mut MessageWriter<SendToLobby>,
) {
    let player = |host: &ScriptHost, id: PlayerId| host.entities.get(&id).copied();
    match command {
        Command::SetBlocks(blocks) => host.pending_blocks.extend(blocks),
        Command::Revert { pos, expect, restore } => {
            if world.mirror.get(pos) == expect && world.mirror.set(pos, restore).is_some() {
                chunks.set_block(BlockPos::new(pos[0], pos[1], pos[2]), restore);
            }
        }
        Command::SetRules { region, rules: r } => rules.0.set(region, r),
        Command::RemoveRegion(id) => {
            rules.0.regions.remove(&id);
        }
        Command::SendToLobby { player: id } => {
            if let Some(entity) = player(host, id) {
                to_lobby.write(SendToLobby(entity));
            }
        }
        command => {
            let id = match &command {
                Command::Message { player, .. }
                | Command::Title { player, .. }
                | Command::ActionBar { player, .. }
                | Command::Teleport { player, .. }
                | Command::SetGameMode { player, .. }
                | Command::SetHealth { player, .. }
                | Command::SetFood { player, .. }
                | Command::Give { player, .. }
                | Command::SetSlot { player, .. }
                | Command::ClearInventory { player }
                | Command::PlaySound { player, .. } => *player,
                _ => return,
            };
            let Some(entity) = player(host, id) else {
                return;
            };
            let Ok((mut client, mut pos, mut look, mut mode, mut health, mut food, mut inventory)) =
                players.get_mut(entity)
            else {
                return;
            };
            match command {
                Command::Message { text, .. } => client.send_chat_message(text),
                Command::Title {
                    title, subtitle, times, ..
                } => {
                    client.set_title_times(times[0], times[1], times[2]);
                    if !subtitle.is_empty() {
                        client.set_subtitle(subtitle);
                    }
                    client.set_title(title);
                }
                Command::ActionBar { text, .. } => client.set_action_bar(text),
                Command::Teleport { pos: p, look: l, .. } => {
                    pos.set(p);
                    if let Some([yaw, pitch]) = l {
                        look.yaw = yaw;
                        look.pitch = pitch;
                    }
                }
                Command::SetGameMode { mode: m, .. } => *mode = m,
                Command::SetHealth { health: h, .. } => {
                    let died = h <= 0.0 && health.0 > 0.0;
                    health.0 = h;
                    if died {
                        host.inbox.push(Event::Died { id });
                    }
                }
                Command::SetFood { food: f, .. } => food.0 = f,
                Command::Give { item, .. } => give(&mut inventory, item),
                Command::SetSlot { slot, item, .. } => inventory.set_slot(slot, item),
                Command::ClearInventory { .. } => {
                    for slot in 0..inventory.slot_count() {
                        inventory.set_slot(slot, ItemStack::EMPTY);
                    }
                }
                Command::PlaySound {
                    sound, volume, pitch, ..
                } => {
                    let name = if sound.contains(':') { sound } else { format!("minecraft:{sound}") };
                    if let Some(sound) = Ident::new(name).ok().and_then(|id| Sound::from_ident(id.as_str_ident())) {
                        client.play_sound(sound, SoundCategory::Master, pos.0, volume, pitch);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Adds to matching stacks first, then empty slots (hotbar first, like
/// vanilla). Whatever doesn't fit is lost.
fn give(inventory: &mut Inventory, mut item: ItemStack) {
    let max = item.item.max_stack();
    let mut key = item.clone();
    key.count = 1;
    let same = |stack: &ItemStack| {
        let mut other = stack.clone();
        other.count = 1;
        other == key
    };
    let order: Vec<u16> = (36..45).chain(9..36).collect();
    for &slot in &order {
        let stack = inventory.slot(slot);
        if !stack.is_empty() && same(stack) && stack.count < max {
            let moved = (max - stack.count).min(item.count);
            let count = stack.count + moved;
            inventory.set_slot_amount(slot, count);
            item.count -= moved;
            if item.count == 0 {
                return;
            }
        }
    }
    for &slot in &order {
        if inventory.slot(slot).is_empty() {
            let mut stack = item.clone();
            stack.count = item.count.min(max);
            item.count -= stack.count;
            inventory.set_slot(slot, stack);
            if item.count == 0 {
                return;
            }
        }
    }
}

fn apply_pending_blocks(mut layers: Query<(&mut ScriptHost, &mut ChunkLayer, &mut DraftWorld)>, config: Res<ScriptConfig>) {
    for (mut host, mut chunks, mut world) in &mut layers {
        let n = host.pending_blocks.len().min(config.blocks_per_tick);
        for (p, state) in host.pending_blocks.drain(..n) {
            if world.mirror.set(p, state).is_some() {
                chunks.set_block(BlockPos::new(p[0], p[1], p[2]), state);
            }
        }
    }
}

/// Tells each VM who arrived and who left since the last tick.
fn track_members(
    mut layers: Query<(Entity, &mut ScriptHost)>,
    players: Query<(Entity, &InInstance, &Username, &UniqueId), With<Client>>,
) {
    let mut members: HashMap<Entity, Vec<(Entity, &Username, &UniqueId)>> = HashMap::new();
    for (entity, in_instance, name, uuid) in &players {
        members.entry(in_instance.0).or_default().push((entity, name, uuid));
    }
    for (layer, mut host) in &mut layers {
        let present = members.remove(&layer).unwrap_or_default();
        let gone: Vec<_> = host
            .ids
            .iter()
            .filter(|(e, _)| !present.iter().any(|(p, ..)| p == *e))
            .map(|(e, id)| (*e, *id))
            .collect();
        for (entity, id) in gone {
            host.ids.remove(&entity);
            host.entities.remove(&id);
            host.inbox.push(Event::PlayerRemoving { id });
        }
        for (entity, name, uuid) in present {
            if host.ids.contains_key(&entity) {
                continue;
            }
            let id = host.next_id;
            host.next_id += 1;
            host.ids.insert(entity, id);
            host.entities.insert(id, entity);
            host.inbox.push(Event::PlayerAdded {
                id,
                name: name.0.clone(),
                uuid: uuid.0,
            });
        }
    }
}

/// The respawn button: back to the map spawn at full health.
fn respawn(
    mut messages: MessageReader<RequestRespawnMessage>,
    mut players: Query<(&InInstance, &mut Position, &mut Health, &mut Food)>,
    mut layers: Query<(&DraftWorld, Option<&mut ScriptHost>)>,
) {
    for msg in messages.read() {
        let Ok((in_instance, mut pos, mut health, mut food)) = players.get_mut(msg.client) else {
            continue;
        };
        let Ok((world, host)) = layers.get_mut(in_instance.0) else {
            continue;
        };
        pos.set(world.mirror.spawn.pos);
        health.0 = 20.0;
        food.0 = 20;
        if let Some(mut host) = host {
            host.player_event(msg.client, |id| Event::Respawned { id });
        }
    }
}

fn collect_chat(
    mut messages: MessageReader<ChatReceivedMessage>,
    players: Query<&InInstance>,
    mut layers: Query<&mut ScriptHost>,
) {
    for msg in messages.read() {
        let Ok(in_instance) = players.get(msg.client) else {
            continue;
        };
        if let Ok(mut host) = layers.get_mut(in_instance.0) {
            let message = msg.message.to_string();
            host.player_event(msg.client, |id| Event::Chatted { id, message });
        }
    }
}

type SnapParts<'a> = (
    &'a InInstance,
    &'a Position,
    &'a Look,
    &'a Health,
    &'a Food,
    &'a GameMode,
    &'a Inventory,
    &'a HeldItem,
);

fn spawn_steps(
    mut layers: Query<(Entity, &mut ScriptHost, &DraftWorld), Without<Despawned>>,
    players: Query<SnapParts, With<Client>>,
    server: Res<Server>,
) {
    let tick = server.current_tick() as u64;
    for (layer, mut host, world) in &mut layers {
        let has_players = !host.ids.is_empty();
        let due = host.next_timer.is_some_and(|t| t <= tick);
        if !(has_players || due || host.runnable || !host.inbox.is_empty()) {
            continue;
        }
        let Slot::Idle(_) = host.slot else {
            continue;
        };
        let Slot::Idle(mut vm) = std::mem::replace(&mut host.slot, Slot::Empty) else {
            unreachable!()
        };
        let snaps = host
            .ids
            .iter()
            .filter_map(|(&entity, &id)| {
                let (in_instance, pos, look, health, food, mode, inventory, held) = players.get(entity).ok()?;
                (in_instance.0 == layer).then(|| PlayerSnap {
                    id,
                    pos: pos.0.to_array(),
                    yaw: look.yaw,
                    pitch: look.pitch,
                    health: health.0,
                    food: food.0,
                    game_mode: *mode,
                    held: inventory.slot(held.slot()).item,
                })
            })
            .collect();
        let blocks = (host.resync && host.pending_blocks.is_empty()).then(|| {
            host.resync = false;
            world.mirror.clone()
        });
        let snapshot = Snapshot {
            tick,
            players: snaps,
            blocks,
        };
        let events = std::mem::take(&mut host.inbox);
        let task = AsyncComputeTaskPool::get().spawn(async move {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| vm.step(snapshot, events)));
            let result = result.map_err(|panic| {
                panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".to_owned())
            });
            (vm, result)
        });
        host.slot = Slot::Running(task);
    }
}
