//! `game.World` (blocks), `game.Rules`, `Region` and the block events.

use std::collections::HashSet;

use chunkedge::prelude::*;
use mlua::{AnyUserData, Lua, MetaMethod, MultiValue, Table, UserData, UserDataFields, UserDataMethods, Value};
use mm_world::{Bounds, parse_state};

use super::signal::Signal;
use super::vector::{Vector3, point};
use crate::host::{Host, Region as RegionState, host};
use crate::io::{Command, RegionId};
use crate::rules::Rules;

/// A block's name for scripts: just the kind (`oak_log`) when every
/// property has its default, else the full state (`oak_log[axis=x]`).
pub fn state_name(state: BlockState) -> String {
    if state.to_kind().to_state() == state {
        state.to_kind().to_str().to_owned()
    } else {
        mm_world::state_name(state)
    }
}

fn block_state(name: &str) -> mlua::Result<BlockState> {
    parse_state(name).ok_or_else(|| mlua::Error::runtime(format!("unknown block `{name}`")))
}

fn block_kind(name: &str) -> mlua::Result<BlockKind> {
    Ok(block_state(name)?.to_kind())
}

/// The part of the box spanned by two corners that lies in the map, if it
/// isn't too large to fill.
fn fill_area(host: &Host, a: [i32; 3], b: [i32; 3]) -> mlua::Result<Option<Bounds>> {
    let Some(area) = Bounds::from_corners(a, b).intersect(&host.mirror.bounds) else {
        return Ok(None);
    };
    if area.volume() > host.limits.max_fill_volume {
        return Err(mlua::Error::runtime(format!(
            "that's {} blocks; one fill may change at most {}",
            area.volume(),
            host.limits.max_fill_volume
        )));
    }
    Ok(Some(area))
}

/// Weighted block choices for `Region:Refill`.
fn pattern(value: Value) -> mlua::Result<Vec<(BlockState, u64)>> {
    match value {
        Value::String(s) => Ok(vec![(block_state(&s.to_str()?)?, 1)]),
        Value::Table(t) => {
            let mut choices = Vec::new();
            for pair in t.pairs::<String, f64>() {
                let (name, weight) = pair?;
                if !(weight > 0.0 && weight.is_finite()) {
                    return Err(mlua::Error::runtime(format!("weight of `{name}` must be positive")));
                }
                choices.push((block_state(&name)?, (weight * 1000.0) as u64 + 1));
            }
            if choices.is_empty() {
                return Err(mlua::Error::runtime("the pattern is empty"));
            }
            // `pairs` order is unspecified; sort so a seed gives one result.
            choices.sort_by_key(|(state, _)| state.to_raw());
            Ok(choices)
        }
        other => Err(mlua::Error::runtime(format!("expected a block or pattern, got {}", other.type_name()))),
    }
}

fn fill(host: &mut Host, area: Bounds, choices: &[(BlockState, u64)]) {
    let total: u64 = choices.iter().map(|c| c.1).sum();
    let blocks: Vec<_> = area
        .iter()
        .map(|p| {
            let state = if choices.len() == 1 {
                choices[0].0
            } else {
                let mut roll = host.random() % total;
                choices
                    .iter()
                    .find(|(_, w)| {
                        if roll < *w {
                            true
                        } else {
                            roll -= w;
                            false
                        }
                    })
                    .map_or(choices[0].0, |c| c.0)
            };
            (p, state)
        })
        .collect();
    host.set_blocks(blocks);
}

/// `game.World`.
pub struct World;

impl UserData for World {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("BlockBroken", |lua, _| Ok(Signal(host(lua).game.block_broken)));
        fields.add_field_method_get("BlockPlaced", |lua, _| Ok(Signal(host(lua).game.block_placed)));
        fields.add_field_method_get("Min", |lua, _| Ok(Vector3(host(lua).mirror.bounds.min.map(f64::from))));
        fields.add_field_method_get("Max", |lua, _| {
            Ok(Vector3(host(lua).mirror.bounds.max.map(|c| f64::from(c - 1))))
        });
        fields.add_field_method_get("Spawn", |lua, _| Ok(Vector3(host(lua).mirror.spawn.pos)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("GetBlock", |lua, _, args: MultiValue| {
            let p = Vector3(point(&args, lua)?).block();
            Ok(state_name(host(lua).mirror.get(p)))
        });
        methods.add_method("SetBlock", |lua, _, (pos, name): (Vector3, String)| {
            let state = block_state(&name)?;
            let p = pos.block();
            let mut host = host(lua);
            let inside = host.mirror.bounds.contains(p);
            host.set_blocks([(p, state)]);
            Ok(inside)
        });
        methods.add_method("Fill", |lua, _, (a, b, name): (Vector3, Vector3, String)| {
            let state = block_state(&name)?;
            let mut host = host(lua);
            if let Some(area) = fill_area(&host, a.block(), b.block())? {
                fill(&mut host, area, &[(state, 1)]);
            }
            Ok(())
        });
    }
}

/// `game.Rules` (`scope: None`) or a region's `Rules`.
#[derive(Clone, Copy)]
pub struct RulesRef {
    scope: Option<RegionId>,
}

fn rules_mut(host: &mut Host, scope: Option<RegionId>) -> Option<&mut Rules> {
    match scope {
        None => Some(&mut host.game_rules),
        Some(id) => host.regions.get_mut(&id).map(|r| &mut r.rules),
    }
}

fn names(lua: &Lua, kinds: &Option<Vec<BlockKind>>) -> mlua::Result<Value> {
    match kinds {
        None => Ok(Value::Nil),
        Some(kinds) => Ok(Value::Table(lua.create_sequence_from(kinds.iter().map(|k| k.to_str()))?)),
    }
}

fn kinds(value: Value) -> mlua::Result<Option<Vec<BlockKind>>> {
    match value {
        Value::Nil => Ok(None),
        Value::Table(t) => t
            .sequence_values::<String>()
            .map(|name| block_kind(&name?))
            .collect::<mlua::Result<_>>()
            .map(Some),
        other => Err(mlua::Error::runtime(format!("expected a list of blocks, got {}", other.type_name()))),
    }
}

impl UserData for RulesRef {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, r, key: String| {
            let mut host = host(lua);
            let Some(rules) = rules_mut(&mut host, r.scope) else {
                return Ok(Value::Nil);
            };
            let rules = rules.clone();
            drop(host);
            match key.as_str() {
                "CanBreak" => Ok(rules.can_break.map_or(Value::Nil, Value::Boolean)),
                "CanPlace" => Ok(rules.can_place.map_or(Value::Nil, Value::Boolean)),
                "BreakableBlocks" => names(lua, &rules.breakable),
                "PlaceableBlocks" => names(lua, &rules.placeable),
                _ => Err(mlua::Error::runtime(format!("`{key}` is not a rule"))),
            }
        });
        methods.add_meta_method(MetaMethod::NewIndex, |lua, r, (key, value): (String, Value)| {
            let flag = |v: &Value| match v {
                Value::Nil => Ok(None),
                Value::Boolean(b) => Ok(Some(*b)),
                other => Err(mlua::Error::runtime(format!("`{key}` must be a boolean or nil, got {}", other.type_name()))),
            };
            let mut host = host(lua);
            let Some(rules) = rules_mut(&mut host, r.scope) else {
                return Err(mlua::Error::runtime("this region was destroyed"));
            };
            match key.as_str() {
                "CanBreak" => rules.can_break = flag(&value)?,
                "CanPlace" => rules.can_place = flag(&value)?,
                "BreakableBlocks" => rules.breakable = kinds(value)?,
                "PlaceableBlocks" => rules.placeable = kinds(value)?,
                _ => return Err(mlua::Error::runtime(format!("`{key}` is not a rule"))),
            }
            let command = match r.scope {
                None => Some(Command::SetRules {
                    region: None,
                    rules: host.game_rules.clone(),
                }),
                Some(id) => host.region_rules_command(id),
            };
            host.command(command.expect("scope exists"));
            Ok(())
        });
    }
}

/// A box of blocks with enter/leave events and its own rules.
pub struct Region(RegionId);

fn region<R>(lua: &Lua, id: RegionId, f: impl FnOnce(&mut Host, &RegionState) -> R) -> mlua::Result<R> {
    let mut host = host(lua);
    let Some(state) = host.regions.remove(&id) else {
        return Err(mlua::Error::runtime("this region was destroyed"));
    };
    let out = f(&mut host, &state);
    host.regions.insert(id, state);
    Ok(out)
}

impl UserData for Region {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("Min", |lua, r| region(lua, r.0, |_, s| Vector3(s.bounds.min.map(f64::from))));
        fields.add_field_method_get("Max", |lua, r| {
            region(lua, r.0, |_, s| Vector3(s.bounds.max.map(|c| f64::from(c - 1))))
        });
        fields.add_field_method_get("Entered", |lua, r| region(lua, r.0, |_, s| Signal(s.entered)));
        fields.add_field_method_get("Left", |lua, r| region(lua, r.0, |_, s| Signal(s.left)));
        fields.add_field_method_get("Rules", |_, r| Ok(RulesRef { scope: Some(r.0) }));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("Contains", |lua, r, args: MultiValue| {
            let p = Vector3(point(&args, lua)?).block();
            region(lua, r.0, |_, s| s.bounds.contains(p))
        });
        methods.add_method("GetPlayers", |lua, r, ()| {
            let players: Vec<AnyUserData> = region(lua, r.0, |host, s| {
                host.players
                    .iter()
                    .filter(|(id, _)| s.inside.contains(id))
                    .map(|(_, e)| e.userdata.clone())
                    .collect()
            })?;
            Ok(players)
        });
        // `Fill("stone")` or `Refill({ stone = 70, coal_ore = 30 })`.
        for name in ["Fill", "Refill"] {
            methods.add_method(name, |lua, r, choice: Value| {
                let choices = pattern(choice)?;
                region(lua, r.0, |host, s| -> mlua::Result<()> {
                    let max = s.bounds.max.map(|c| c - 1);
                    if let Some(area) = fill_area(host, s.bounds.min, max)? {
                        fill(host, area, &choices);
                    }
                    Ok(())
                })?
            });
        }
        methods.add_method("Destroy", |lua, r, ()| {
            let mut host = host(lua);
            if let Some(state) = host.regions.remove(&r.0) {
                host.signals.destroy(state.entered);
                host.signals.destroy(state.left);
                host.command(Command::RemoveRegion(r.0));
            }
            Ok(())
        });
    }
}

fn new_region(lua: &Lua, (a, b): (Vector3, Vector3)) -> mlua::Result<Region> {
    let mut host = host(lua);
    if host.regions.len() >= 1024 {
        return Err(mlua::Error::runtime("too many regions (the limit is 1024)"));
    }
    let id = host.next_region;
    host.next_region += 1;
    let state = RegionState {
        bounds: Bounds::from_corners(a.block(), b.block()),
        rules: Rules::default(),
        inside: HashSet::new(),
        entered: host.signals.create(),
        left: host.signals.create(),
    };
    host.regions.insert(id, state);
    Ok(Region(id))
}

/// Fires `Entered`/`Left` for players who crossed a region's edge since the
/// last step. Players who left the game just drop out.
pub fn track_regions(lua: &Lua) -> mlua::Result<()> {
    let mut crossings = Vec::new();
    {
        let mut host = host(lua);
        let Host {
            regions,
            snapshot,
            players,
            ..
        } = &mut *host;
        for region in regions.values_mut() {
            for (id, entry) in players.iter() {
                let inside = snapshot
                    .get(id)
                    .is_some_and(|s| region.bounds.contains(Vector3(s.pos).block()));
                if inside && region.inside.insert(*id) {
                    crossings.push((region.entered, entry.userdata.clone()));
                } else if !inside && region.inside.remove(id) {
                    crossings.push((region.left, entry.userdata.clone()));
                }
            }
            region.inside.retain(|id| players.contains_key(id));
        }
    }
    for (signal, player) in crossings {
        crate::host::fire(lua, signal, player)?;
    }
    Ok(())
}

/// The argument of `BlockBroken`/`BlockPlaced`. `Cancel()` undoes the change.
pub struct BlockEvent {
    pub player: Option<AnyUserData>,
    pub pos: [i32; 3],
    pub old: BlockState,
    pub new: BlockState,
}

impl UserData for BlockEvent {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("Player", |_, e| Ok(e.player.clone()));
        fields.add_field_method_get("Position", |_, e| Ok(Vector3(e.pos.map(f64::from))));
        fields.add_field_method_get("Block", |_, e| {
            Ok(state_name(if e.new.is_air() { e.old } else { e.new }))
        });
        fields.add_field_method_get("PreviousBlock", |_, e| Ok(state_name(e.old)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("Cancel", |lua, e, ()| {
            let mut host = host(lua);
            if host.mirror.get(e.pos) == e.new {
                host.mirror.set(e.pos, e.old);
                host.command(Command::Revert {
                    pos: e.pos,
                    expect: e.new,
                    restore: e.old,
                });
            }
            Ok(())
        });
    }
}

pub fn install(lua: &Lua, game: &Table) -> mlua::Result<()> {
    game.set("World", World)?;
    game.set("Rules", RulesRef { scope: None })?;
    let class = lua.create_table()?;
    class.set("new", lua.create_function(new_region)?)?;
    lua.globals().set("Region", class)
}
