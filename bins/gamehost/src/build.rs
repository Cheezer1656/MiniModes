//! Building in drafts: creative placing and breaking inside the map bounds,
//! and WorldEdit-lite (`//wand //pos1 //pos2 //set //replace //copy //paste
//! //undo`). Edits run as jobs drained at a fixed block budget per tick, so a
//! large `//set` never stalls the shard. `//region add|remove|list` marks
//! the boxes a "scripts + regions" publish copies over a persistent world.

use std::collections::VecDeque;
use std::sync::Arc;

use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::hand_swing::HandSwingWriter;
use chunkedge::interact_block::InteractBlockMessage;
use chunkedge::inventory::HeldItem;
use chunkedge::inventory::player_inventory::PlayerInventory;
use chunkedge::prelude::*;
use chunkedge::protocol::packets::play::BlockChangedAckS2c;
use chunkedge::protocol::{VarInt, WritePacket};
use mm_core::Tokio;
use mm_script::Event;
use mm_world::{Bounds, parse_state, state_name};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::instance::InInstance;
use crate::script::{GameRules, ScriptHost};
use crate::world::{GameWorld, Storage};

/// Largest box `//region add` accepts.
const MAX_REGION_VOLUME: u64 = 16 << 20;

const WAND: ItemKind = ItemKind::WoodenAxe;
const UNDO_DEPTH: usize = 3;

#[derive(Resource, Clone, Copy)]
pub struct EditLimits {
    /// Largest region one command may touch.
    pub max_volume: u64,
    /// Blocks changed per tick across all edit jobs on the shard.
    pub blocks_per_tick: usize,
}

struct Clipboard {
    size: [i32; 3],
    /// Region minimum relative to the player at copy time.
    offset: [i32; 3],
    blocks: Vec<BlockState>,
}

/// Per-player building state in a draft.
#[derive(Component, Default)]
pub struct Builder {
    pos1: Option<[i32; 3]>,
    pos2: Option<[i32; 3]>,
    clipboard: Option<Arc<Clipboard>>,
    undo: VecDeque<Vec<([i32; 3], BlockState)>>,
    /// An edit job is queued or running.
    busy: bool,
}

impl Builder {
    fn selection(&self) -> Option<Bounds> {
        Some(Bounds::from_corners(self.pos1?, self.pos2?))
    }
}

enum JobKind {
    /// Set every block in `region` (or only those equal to `only`) to `to`.
    Fill {
        region: Bounds,
        to: BlockState,
        only: Option<BlockState>,
    },
    Paste {
        origin: [i32; 3],
        clipboard: Arc<Clipboard>,
    },
    Copy {
        region: Bounds,
        offset: [i32; 3],
        blocks: Vec<BlockState>,
    },
    /// Put back the previous states recorded by an earlier job.
    Restore(Vec<([i32; 3], BlockState)>),
}

struct EditJob {
    player: Entity,
    layer: Entity,
    kind: JobKind,
    cursor: usize,
    /// Previous states of changed blocks, for `//undo`.
    changes: Vec<([i32; 3], BlockState)>,
}

impl EditJob {
    fn len(&self) -> usize {
        match &self.kind {
            JobKind::Fill { region, .. } | JobKind::Copy { region, .. } => region.volume() as usize,
            JobKind::Paste { clipboard, .. } => clipboard.blocks.len(),
            JobKind::Restore(changes) => changes.len(),
        }
    }
}

#[derive(Resource, Default)]
struct EditQueue(VecDeque<EditJob>);

/// Answers to `//region` commands, which need the database.
#[derive(Resource)]
struct RegionReplies {
    tx: UnboundedSender<(Entity, String)>,
    rx: UnboundedReceiver<(Entity, String)>,
}

/// Building and edit jobs; runs between the two script sets.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BuildSet;

pub struct BuildPlugin;

impl Plugin for BuildPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.init_resource::<EditQueue>()
            .insert_resource(RegionReplies { tx, rx })
            .add_systems(
                Update,
                (break_blocks, place_blocks, edit_commands, region_replies, run_edit_jobs)
                    .chain()
                    .in_set(BuildSet),
            );
    }
}

/// `i`th position of a region, x fastest, then z, then y.
fn nth(region: &Bounds, i: usize) -> [i32; 3] {
    let [sx, _, sz] = region.size().map(|s| s as usize);
    [
        region.min[0] + (i % sx) as i32,
        region.min[1] + (i / (sx * sz)) as i32,
        region.min[2] + ((i / sx) % sz) as i32,
    ]
}

fn block_pos(p: [i32; 3]) -> BlockPos {
    BlockPos::new(p[0], p[1], p[2])
}

fn arr(p: BlockPos) -> [i32; 3] {
    [p.x, p.y, p.z]
}

type BuildLayer<'a> = (
    &'a mut ChunkLayer,
    &'a mut GameWorld,
    &'a GameRules,
    Option<&'a mut ScriptHost>,
);

/// Creative players break instantly and ignore rules; others break when
/// digging finishes, if the rules allow it.
fn break_blocks(
    mut messages: MessageReader<DiggingMessage>,
    mut players: Query<(&mut Client, &InInstance, &GameMode, &Inventory, &HeldItem, Option<&mut Builder>)>,
    mut layers: Query<BuildLayer>,
) {
    for msg in messages.read() {
        let Ok((mut client, in_instance, mode, inventory, held, builder)) = players.get_mut(msg.client) else {
            continue;
        };
        let creative = *mode == GameMode::Creative;
        let finished = match msg.state {
            DiggingState::Start => creative,
            DiggingState::Stop => !creative,
            _ => false,
        };
        if !finished {
            continue;
        }
        let pos = arr(msg.position);
        // The client's predicted break is rolled back by the dig ack unless the
        // block really changes.
        if let Some(mut builder) = builder.filter(|_| creative && inventory.slot(held.slot()).item == WAND) {
            builder.pos1 = Some(pos);
            client.send_chat_message(format!("§dPosition 1 set to {pos:?}{}", volume_hint(&builder)));
            continue;
        }
        let Ok((mut layer, mut world, rules, host)) = layers.get_mut(in_instance.0) else {
            continue;
        };
        let old = world.mirror.get(pos);
        if old.is_air() || !(creative || rules.0.may_break(pos, old.to_kind())) {
            continue;
        }
        if world.mirror.set(pos, BlockState::AIR).is_some() {
            layer.set_block(msg.position, BlockState::AIR);
            if let Some(mut host) = host {
                host.player_event(msg.client, |player| Event::BlockBroken { player, pos, old });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn place_blocks(
    mut messages: MessageReader<InteractBlockMessage>,
    mut players: Query<(
        &mut Client,
        &InInstance,
        &GameMode,
        &mut Inventory,
        &HeldItem,
        &Look,
        Option<&mut Builder>,
    )>,
    mut layers: Query<BuildLayer>,
    mut swings: HandSwingWriter,
) {
    for msg in messages.read() {
        let Ok((mut client, in_instance, mode, mut inventory, held, look, builder)) = players.get_mut(msg.client)
        else {
            continue;
        };
        // Confirm the client's prediction either way; without an ack a
        // rejected placement would linger on the client.
        client.write_packet(&BlockChangedAckS2c {
            sequence: VarInt(msg.sequence),
        });
        let creative = *mode == GameMode::Creative;
        if !matches!(*mode, GameMode::Creative | GameMode::Survival) {
            continue;
        }
        let slot = match msg.hand {
            Hand::Main => held.slot(),
            Hand::Off => PlayerInventory::SLOT_OFFHAND,
        };
        let stack = inventory.slot(slot).clone();
        if let Some(mut builder) = builder.filter(|_| creative && stack.item == WAND) {
            if msg.hand == Hand::Main {
                builder.pos2 = Some(arr(msg.position));
                client.send_chat_message(format!("§dPosition 2 set to {:?}{}", arr(msg.position), volume_hint(&builder)));
            }
            continue;
        }
        let Some(kind) = BlockKind::from_item_kind(stack.item) else {
            continue;
        };
        let Ok((mut layer, mut world, rules, host)) = layers.get_mut(in_instance.0) else {
            continue;
        };
        let clicked = arr(msg.position);
        let target = if world.mirror.get(clicked).is_replaceable() {
            clicked
        } else {
            arr(msg.position.get_in_direction(msg.face))
        };
        let old = world.mirror.get(target);
        if !old.is_replaceable() || !(creative || rules.0.may_place(target, kind)) {
            continue;
        }
        let state = oriented(kind, msg.face, look.yaw);
        if world.mirror.set(target, state).is_some() {
            layer.set_block(block_pos(target), state);
            swings.swing(msg.client, msg.hand);
            if !creative {
                if stack.count > 1 {
                    inventory.set_slot_amount(slot, stack.count - 1);
                } else {
                    inventory.set_slot(slot, ItemStack::EMPTY);
                }
            }
            if let Some(mut host) = host {
                host.player_event(msg.client, |player| Event::BlockPlaced {
                    player,
                    pos: target,
                    old,
                    new: state,
                });
            }
        }
    }
}

/// A placed block's state: logs follow the clicked face's axis; blocks with a
/// horizontal facing face the player (stairs face away, like vanilla).
fn oriented(kind: BlockKind, face: Direction, yaw: f32) -> BlockState {
    let mut state = kind.to_state();
    let props = kind.props();
    if props.contains(&PropName::Axis) {
        state = state.set(
            PropName::Axis,
            match face {
                Direction::Down | Direction::Up => PropValue::Y,
                Direction::North | Direction::South => PropValue::Z,
                Direction::West | Direction::East => PropValue::X,
            },
        );
    }
    if props.contains(&PropName::Facing) {
        // Yaw 0 looks south (+z), 90 west.
        let looking = match ((yaw.rem_euclid(360.0) + 45.0) / 90.0) as u32 % 4 {
            0 => PropValue::South,
            1 => PropValue::West,
            2 => PropValue::North,
            _ => PropValue::East,
        };
        let opposite = match looking {
            PropValue::South => PropValue::North,
            PropValue::West => PropValue::East,
            PropValue::North => PropValue::South,
            _ => PropValue::West,
        };
        let facing = if kind.to_str().ends_with("_stairs") { looking } else { opposite };
        let candidate = state.set(PropName::Facing, facing);
        // Only apply if the block accepts horizontal facings.
        if candidate.get(PropName::Facing) == Some(facing) {
            state = candidate;
        }
    }
    state
}

fn volume_hint(builder: &Builder) -> String {
    builder
        .selection()
        .map_or(String::new(), |s| format!(" §7({} blocks)", s.volume()))
}

#[allow(clippy::too_many_arguments)]
fn edit_commands(
    mut commands: MessageReader<CommandExecutionMessage>,
    mut players: Query<(&mut Client, Option<&InInstance>, &Position, &mut Inventory, &HeldItem, Option<&mut Builder>)>,
    drafts: Query<&GameWorld>,
    mut queue: ResMut<EditQueue>,
    limits: Res<EditLimits>,
    storage: Res<Storage>,
    replies: Res<RegionReplies>,
    tokio: Res<Tokio>,
) {
    for cmd in commands.read() {
        // `//set stone` arrives as the command `/set stone`.
        let Some(line) = cmd.command.strip_prefix('/') else {
            continue;
        };
        let mut args = line.split_whitespace();
        let Some(name) = args.next() else {
            continue;
        };
        let args: Vec<&str> = args.collect();
        let Ok((mut client, in_instance, pos, mut inventory, held, builder)) = players.get_mut(cmd.executor) else {
            continue;
        };
        let (Some(in_instance), Some(mut builder)) = (in_instance, builder) else {
            client.send_chat_message("§cBuilding tools only work while editing a draft.");
            continue;
        };
        let Ok(world) = drafts.get(in_instance.0) else {
            client.send_chat_message("§cThe map is still loading.");
            continue;
        };
        let here = pos.0.floor().as_ivec3().to_array();

        let reply = |client: &mut Client, msg: String| client.send_chat_message(msg);
        let job = match name {
            "wand" => {
                inventory.set_slot(held.slot(), ItemStack::new(WAND, 1));
                reply(&mut client, "§dLeft-click a block for position 1, right-click for position 2.".into());
                None
            }
            "pos1" | "pos2" => {
                if name == "pos1" {
                    builder.pos1 = Some(here);
                } else {
                    builder.pos2 = Some(here);
                }
                let n = &name[3..];
                reply(&mut client, format!("§dPosition {n} set to {here:?}{}", volume_hint(&builder)));
                None
            }
            "sel" => {
                let msg = match builder.selection() {
                    Some(s) => format!("§dSelection {:?} to {:?} ({} blocks)", s.min, s.max.map(|c| c - 1), s.volume()),
                    None => "§7Nothing selected. Use //wand or //pos1 and //pos2.".into(),
                };
                reply(&mut client, msg);
                None
            }
            "region" => {
                match region_command(&args, &builder, world) {
                    Ok(op) => {
                        let (db, tx, game, player) = (storage.db.clone(), replies.tx.clone(), world.game.0, cmd.executor);
                        tokio.0.spawn(async move {
                            let reply = op.run(&db, game).await.unwrap_or_else(|e| {
                                tracing::error!("//region in game {game}: {e}");
                                "§cCouldn't reach the database; try again.".into()
                            });
                            let _ = tx.send((player, reply));
                        });
                    }
                    Err(msg) => reply(&mut client, format!("§c{msg}")),
                }
                None
            }
            "set" | "replace" | "copy" | "paste" | "undo" => {
                if builder.busy {
                    reply(&mut client, "§cYour previous edit is still running.".into());
                    continue;
                }
                match plan_edit(name, &args, &builder, world, here, &limits) {
                    Ok(kind) => Some(kind),
                    Err(msg) => {
                        reply(&mut client, format!("§c{msg}"));
                        None
                    }
                }
            }
            _ => continue, // not ours (e.g. /lobby)
        };
        if let Some(kind) = job {
            if matches!(kind, JobKind::Restore(_)) {
                builder.undo.pop_back();
            }
            builder.busy = true;
            queue.0.push_back(EditJob {
                player: cmd.executor,
                layer: in_instance.0,
                kind,
                cursor: 0,
                changes: Vec::new(),
            });
        }
    }
}

enum RegionOp {
    Add(String, mm_db::publish::Corners),
    Remove(String),
    List,
}

impl RegionOp {
    async fn run(self, db: &mm_db::Pool, game: i64) -> Result<String, mm_db::Error> {
        use mm_db::publish::{MAX_PUBLISH_REGIONS, delete_publish_region, publish_regions, set_publish_region};
        Ok(match self {
            Self::Add(name, corners) => {
                if set_publish_region(db, game, &name, corners).await? {
                    format!("§dSaved region §f{name}§d. \"Scripts + regions\" publishes copy it over the live world.")
                } else {
                    format!("§cA game can have at most {MAX_PUBLISH_REGIONS} regions.")
                }
            }
            Self::Remove(name) => {
                if delete_publish_region(db, game, &name).await? {
                    format!("§dRemoved region §f{name}§d.")
                } else {
                    format!("§cNo region called {name}.")
                }
            }
            Self::List => {
                let regions = publish_regions(db, game).await?;
                if regions.is_empty() {
                    "§7No regions yet. Select a box and use §f//region add <name>§7.".into()
                } else {
                    let list: Vec<_> = regions
                        .iter()
                        .map(|(name, (a, b))| format!("§f{name} §8{a:?}..{b:?}"))
                        .collect();
                    format!("§dRegions: {}", list.join("§7, "))
                }
            }
        })
    }
}

fn region_command(args: &[&str], builder: &Builder, world: &GameWorld) -> Result<RegionOp, String> {
    let valid = |name: &str| {
        (1..=32).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    match args {
        ["add", name] if valid(name) => {
            let sel = builder.selection().ok_or("Select a box first (//wand or //pos1 and //pos2).")?;
            let sel = sel.intersect(&world.mirror.bounds).ok_or("The selection is outside the map.")?;
            if sel.volume() > MAX_REGION_VOLUME {
                return Err(format!("Regions can be at most {MAX_REGION_VOLUME} blocks."));
            }
            Ok(RegionOp::Add(name.to_string(), (sel.min, sel.max.map(|c| c - 1))))
        }
        ["remove", name] if valid(name) => Ok(RegionOp::Remove(name.to_string())),
        ["add" | "remove", _] => Err("Region names use letters, digits, - and _ (up to 32).".into()),
        ["list"] | [] => Ok(RegionOp::List),
        _ => Err("Usage: //region add <name>, //region remove <name> or //region list".into()),
    }
}

fn region_replies(mut replies: ResMut<RegionReplies>, mut clients: Query<&mut Client>) {
    while let Ok((player, msg)) = replies.rx.try_recv() {
        if let Ok(mut client) = clients.get_mut(player) {
            client.send_chat_message(msg);
        }
    }
}

fn plan_edit(
    name: &str,
    args: &[&str],
    builder: &Builder,
    world: &GameWorld,
    here: [i32; 3],
    limits: &EditLimits,
) -> Result<JobKind, String> {
    let state = |text: &str| parse_state(text).ok_or_else(|| format!("Unknown block `{text}`."));
    let selection = || -> Result<Bounds, String> {
        let sel = builder.selection().ok_or("Select a region first (//wand, or //pos1 and //pos2).")?;
        let sel = sel.intersect(&world.mirror.bounds).ok_or("Your selection is outside the map.")?;
        if sel.volume() > limits.max_volume {
            return Err(format!("That's {} blocks; the limit is {}.", sel.volume(), limits.max_volume));
        }
        Ok(sel)
    };
    Ok(match (name, args) {
        ("set", [block]) => JobKind::Fill {
            region: selection()?,
            to: state(block)?,
            only: None,
        },
        ("replace", [from, to]) => JobKind::Fill {
            region: selection()?,
            to: state(to)?,
            only: Some(state(from)?),
        },
        ("copy", []) => {
            let region = selection()?;
            JobKind::Copy {
                region,
                offset: [0, 1, 2].map(|i| region.min[i] - here[i]),
                blocks: Vec::with_capacity(region.volume() as usize),
            }
        }
        ("paste", []) => {
            let clipboard = builder.clipboard.clone().ok_or("Your clipboard is empty; //copy first.")?;
            JobKind::Paste {
                origin: [0, 1, 2].map(|i| here[i] + clipboard.offset[i]),
                clipboard,
            }
        }
        ("undo", []) => JobKind::Restore(builder.undo.back().cloned().ok_or("Nothing to undo.")?),
        ("set", _) => return Err("Usage: //set <block>".into()),
        ("replace", _) => return Err("Usage: //replace <from> <to>".into()),
        _ => return Err(format!("Usage: //{name}")),
    })
}

fn run_edit_jobs(
    mut queue: ResMut<EditQueue>,
    mut layers: Query<(&mut ChunkLayer, &mut GameWorld, Option<&mut ScriptHost>)>,
    mut players: Query<(&mut Client, &mut Builder)>,
    limits: Res<EditLimits>,
) {
    let mut budget = limits.blocks_per_tick;
    while budget > 0 {
        let Some(job) = queue.0.front_mut() else {
            return;
        };
        let Ok((mut layer, mut world, host)) = layers.get_mut(job.layer) else {
            // The instance stopped; drop the job.
            queue.0.pop_front();
            continue;
        };
        if let Some(mut host) = host.filter(|_| !matches!(job.kind, JobKind::Copy { .. })) {
            host.mark_resync();
        }
        let end = (job.cursor + budget).min(job.len());
        let EditJob {
            kind, cursor, changes, ..
        } = job;
        match kind {
            JobKind::Fill { region, to, only } => {
                for i in *cursor..end {
                    let p = nth(region, i);
                    if only.is_none_or(|o| world.mirror.get(p) == o) {
                        apply(&mut layer, &mut world, p, *to, changes);
                    }
                }
            }
            JobKind::Paste { origin, clipboard } => {
                let local = Bounds {
                    min: [0; 3],
                    max: clipboard.size,
                };
                for (i, &state) in clipboard.blocks.iter().enumerate().take(end).skip(*cursor) {
                    let r = nth(&local, i);
                    let p = [0, 1, 2].map(|a| origin[a] + r[a]);
                    apply(&mut layer, &mut world, p, state, changes);
                }
            }
            JobKind::Copy { region, blocks, .. } => {
                blocks.extend((*cursor..end).map(|i| world.mirror.get(nth(region, i))));
            }
            JobKind::Restore(list) => {
                for i in *cursor..end {
                    let (p, state) = list[list.len() - 1 - i];
                    apply(&mut layer, &mut world, p, state, changes);
                }
            }
        }
        budget -= end - *cursor;
        *cursor = end;
        if job.cursor >= job.len() {
            let job = queue.0.pop_front().expect("front exists");
            finish_job(job, &mut players);
        }
    }
}

fn apply(
    layer: &mut ChunkLayer,
    world: &mut GameWorld,
    p: [i32; 3],
    to: BlockState,
    changes: &mut Vec<([i32; 3], BlockState)>,
) {
    if let Some(old) = world.mirror.set(p, to)
        && old != to
    {
        layer.set_block(block_pos(p), to);
        changes.push((p, old));
    }
}

fn finish_job(job: EditJob, players: &mut Query<(&mut Client, &mut Builder)>) {
    let Ok((mut client, mut builder)) = players.get_mut(job.player) else {
        return;
    };
    builder.busy = false;
    match job.kind {
        JobKind::Copy { region, offset, blocks } => {
            client.send_chat_message(format!("§aCopied {} blocks.", blocks.len()));
            builder.clipboard = Some(Arc::new(Clipboard {
                size: region.size(),
                offset,
                blocks,
            }));
        }
        JobKind::Restore(_) => {
            client.send_chat_message(format!("§aUndid {} block changes.", job.changes.len()));
        }
        JobKind::Fill { to, .. } => {
            client.send_chat_message(format!("§aChanged {} blocks to {}.", job.changes.len(), state_name(to)));
            push_undo(&mut builder, job.changes);
        }
        JobKind::Paste { .. } => {
            client.send_chat_message(format!("§aPasted; {} blocks changed.", job.changes.len()));
            push_undo(&mut builder, job.changes);
        }
    }
}

fn push_undo(builder: &mut Builder, changes: Vec<([i32; 3], BlockState)>) {
    if changes.is_empty() {
        return;
    }
    builder.undo.push_back(changes);
    while builder.undo.len() > UNDO_DEPTH {
        builder.undo.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nth_visits_every_position_once() {
        let region = Bounds::from_corners([-2, 5, 3], [1, 7, 4]);
        let mut seen: Vec<_> = (0..region.volume() as usize).map(|i| nth(&region, i)).collect();
        let mut expected: Vec<_> = region.iter().collect();
        assert_eq!(seen, expected, "same order as Bounds::iter");
        seen.sort();
        expected.sort();
        seen.dedup();
        assert_eq!(seen.len(), region.volume() as usize);
    }

    #[test]
    fn placed_blocks_face_sensibly() {
        let log = oriented(BlockKind::OakLog, Direction::East, 0.0);
        assert_eq!(log.get(PropName::Axis), Some(PropValue::X));
        // Looking south (yaw 0): stairs face south, furnaces face the player (north).
        let stairs = oriented(BlockKind::OakStairs, Direction::Up, 0.0);
        assert_eq!(stairs.get(PropName::Facing), Some(PropValue::South));
        let furnace = oriented(BlockKind::Furnace, Direction::Up, 0.0);
        assert_eq!(furnace.get(PropName::Facing), Some(PropValue::North));
        let furnace = oriented(BlockKind::Furnace, Direction::Up, 90.0);
        assert_eq!(furnace.get(PropName::Facing), Some(PropValue::East));
    }
}
