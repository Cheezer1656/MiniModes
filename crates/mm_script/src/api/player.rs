//! `game.Players`, `Player` and `Item`.

use chunkedge::item::ItemComponent;
use chunkedge::prelude::*;
use chunkedge::protocol::IntoTextComponent;
use mlua::{AnyUserData, FromLua, Lua, MetaMethod, Table, UserData, UserDataFields, UserDataMethods, Value};

use super::signal::Signal;
use super::vector::Vector3;
use crate::host::{SignalId, host};
use crate::io::{Command, PlayerId};

/// Destroyed signals have no slot; this one never existed.
const DEAD_SIGNAL: SignalId = SignalId(u32::MAX);

/// One player of the instance. The same userdata is handed out every time,
/// so players work as table keys and compare with `==`.
pub struct Player {
    pub id: PlayerId,
    pub name: String,
    pub uuid: Uuid,
}

fn mode_name(mode: GameMode) -> &'static str {
    match mode {
        GameMode::Survival => "survival",
        GameMode::Creative => "creative",
        GameMode::Adventure => "adventure",
        GameMode::Spectator => "spectator",
    }
}

fn parse_mode(name: &str) -> mlua::Result<GameMode> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "survival" => GameMode::Survival,
        "creative" => GameMode::Creative,
        "adventure" => GameMode::Adventure,
        "spectator" => GameMode::Spectator,
        _ => return Err(mlua::Error::runtime(format!("unknown game mode `{name}`"))),
    })
}

fn player_signal(lua: &Lua, id: PlayerId, pick: impl Fn(&crate::host::PlayerEntry) -> SignalId) -> Signal {
    Signal(host(lua).players.get(&id).map_or(DEAD_SIGNAL, pick))
}

/// Reads a field of the player's snapshot; `nil` once they've left.
fn snap<R>(lua: &Lua, id: PlayerId, f: impl Fn(&crate::io::PlayerSnap) -> R) -> Option<R> {
    host(lua).snapshot.get(&id).map(f)
}

fn send(lua: &Lua, id: PlayerId, command: Command) {
    host(lua).player_command(id, command);
}

/// Lua inventory slots: 1-9 are the hotbar, 10-36 the main inventory.
fn inventory_slot(slot: u16) -> mlua::Result<u16> {
    match slot {
        1..=9 => Ok(35 + slot),
        10..=36 => Ok(slot - 1),
        _ => Err(mlua::Error::runtime(format!("slot {slot} is outside 1-36"))),
    }
}

impl UserData for Player {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("Name", |_, p| Ok(p.name.clone()));
        fields.add_field_method_get("UserId", |_, p| Ok(p.uuid.hyphenated().to_string()));
        fields.add_field_method_get("Position", |lua, p| Ok(snap(lua, p.id, |s| Vector3(s.pos))));
        fields.add_field_method_get("Yaw", |lua, p| Ok(snap(lua, p.id, |s| s.yaw)));
        fields.add_field_method_get("Pitch", |lua, p| Ok(snap(lua, p.id, |s| s.pitch)));
        fields.add_field_method_get("Health", |lua, p| Ok(snap(lua, p.id, |s| s.health)));
        fields.add_field_method_set("Health", |lua, p, health: f32| {
            send(lua, p.id, Command::SetHealth { player: p.id, health: health.clamp(0.0, 20.0) });
            Ok(())
        });
        fields.add_field_method_get("Food", |lua, p| Ok(snap(lua, p.id, |s| s.food)));
        fields.add_field_method_set("Food", |lua, p, food: i32| {
            send(lua, p.id, Command::SetFood { player: p.id, food: food.clamp(0, 20) });
            Ok(())
        });
        fields.add_field_method_get("GameMode", |lua, p| Ok(snap(lua, p.id, |s| mode_name(s.game_mode))));
        fields.add_field_method_set("GameMode", |lua, p, mode: String| {
            send(lua, p.id, Command::SetGameMode { player: p.id, mode: parse_mode(&mode)? });
            Ok(())
        });
        fields.add_field_method_get("HeldItem", |lua, p| Ok(snap(lua, p.id, |s| s.held.to_str())));
        fields.add_field_method_get("Chatted", |lua, p| Ok(player_signal(lua, p.id, |e| e.chatted)));
        fields.add_field_method_get("Died", |lua, p| Ok(player_signal(lua, p.id, |e| e.died)));
        fields.add_field_method_get("Respawned", |lua, p| Ok(player_signal(lua, p.id, |e| e.respawned)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("SendMessage", |lua, p, text: String| {
            send(lua, p.id, Command::Message { player: p.id, text });
            Ok(())
        });
        methods.add_method(
            "Title",
            |lua, p, (title, subtitle, fade_in, stay, fade_out): (String, Option<String>, Option<i32>, Option<i32>, Option<i32>)| {
                send(
                    lua,
                    p.id,
                    Command::Title {
                        player: p.id,
                        title,
                        subtitle: subtitle.unwrap_or_default(),
                        times: [fade_in.unwrap_or(10), stay.unwrap_or(70), fade_out.unwrap_or(20)],
                    },
                );
                Ok(())
            },
        );
        methods.add_method("ActionBar", |lua, p, text: String| {
            send(lua, p.id, Command::ActionBar { player: p.id, text });
            Ok(())
        });
        methods.add_method("Teleport", |lua, p, (pos, yaw, pitch): (Vector3, Option<f32>, Option<f32>)| {
            if pos.0.iter().any(|c| !c.is_finite()) {
                return Err(mlua::Error::runtime("position must be finite"));
            }
            let look = match (yaw, pitch) {
                (None, None) => None,
                (yaw, pitch) => {
                    let current = snap(lua, p.id, |s| (s.yaw, s.pitch)).unwrap_or_default();
                    Some([yaw.unwrap_or(current.0), pitch.unwrap_or(current.1).clamp(-90.0, 90.0)])
                }
            };
            send(lua, p.id, Command::Teleport { player: p.id, pos: pos.0, look });
            Ok(())
        });
        methods.add_method("Give", |lua, p, (item, count): (Value, Option<i8>)| {
            let item = to_stack(lua, item, count)?;
            send(lua, p.id, Command::Give { player: p.id, item });
            Ok(())
        });
        methods.add_method("SetSlot", |lua, p, (slot, item): (u16, Value)| {
            let slot = inventory_slot(slot)?;
            let item = if item.is_nil() { ItemStack::EMPTY } else { to_stack(lua, item, None)? };
            send(lua, p.id, Command::SetSlot { player: p.id, slot, item });
            Ok(())
        });
        methods.add_method("ClearInventory", |lua, p, ()| {
            send(lua, p.id, Command::ClearInventory { player: p.id });
            Ok(())
        });
        methods.add_method("Kill", |lua, p, ()| {
            send(lua, p.id, Command::SetHealth { player: p.id, health: 0.0 });
            Ok(())
        });
        methods.add_method("PlaySound", |lua, p, (sound, volume, pitch): (String, Option<f32>, Option<f32>)| {
            send(
                lua,
                p.id,
                Command::PlaySound {
                    player: p.id,
                    sound,
                    volume: volume.unwrap_or(1.0).clamp(0.0, 10.0),
                    pitch: pitch.unwrap_or(1.0).clamp(0.5, 2.0),
                },
            );
            Ok(())
        });
        methods.add_method("SendToLobby", |lua, p, ()| {
            send(lua, p.id, Command::SendToLobby { player: p.id });
            Ok(())
        });
        methods.add_method("IsInGame", |lua, p, ()| Ok(host(lua).players.contains_key(&p.id)));
        methods.add_meta_method(MetaMethod::ToString, |_, p, ()| Ok(p.name.clone()));
    }
}

pub struct Item(pub ItemStack);

impl UserData for Item {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("Id", |_, i| Ok(i.0.item.to_str()));
        fields.add_field_method_get("Count", |_, i| Ok(i.0.count));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, i, ()| Ok(format!("{} x{}", i.0.item.to_str(), i.0.count)));
    }
}

pub fn item_kind(name: &str) -> mlua::Result<ItemKind> {
    let name = name.strip_prefix("minecraft:").unwrap_or(name);
    ItemKind::from_str(name)
        .filter(|k| *k != ItemKind::Air)
        .ok_or_else(|| mlua::Error::runtime(format!("unknown item `{name}`")))
}

/// An `Item` or an item id.
fn to_stack(lua: &Lua, item: Value, count: Option<i8>) -> mlua::Result<ItemStack> {
    let mut stack = match &item {
        Value::UserData(ud) => ud.borrow::<Item>()?.0.clone(),
        _ => ItemStack::new(item_kind(&String::from_lua(item, lua)?)?, 1),
    };
    if let Some(count) = count {
        stack.count = count.clamp(1, 99);
    }
    Ok(stack)
}

fn new_item(_: &Lua, (id, props): (String, Option<Table>)) -> mlua::Result<Item> {
    let mut stack = ItemStack::new(item_kind(&id)?, 1);
    let Some(props) = props else {
        return Ok(Item(stack));
    };
    if let Some(count) = props.get::<Option<i8>>("Count")? {
        stack.count = count.clamp(1, 99);
    }
    let mut components = Vec::new();
    if let Some(name) = props.get::<Option<String>>("Name")? {
        components.push(ItemComponent::ItemName(Text::from(name).not_italic().into_text_component()));
    }
    if let Some(lore) = props.get::<Option<Vec<String>>>("Lore")? {
        let lines = lore
            .into_iter()
            .take(32)
            .map(|line| Text::from(line).not_italic().into_text_component())
            .collect();
        components.push(ItemComponent::Lore(lines));
    }
    Ok(Item(stack.with_components(components)))
}

/// `game.Players`.
pub struct Players;

impl UserData for Players {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("PlayerAdded", |lua, _| Ok(Signal(host(lua).game.player_added)));
        fields.add_field_method_get("PlayerRemoving", |lua, _| Ok(Signal(host(lua).game.player_removing)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("GetPlayers", |lua, _, ()| {
            let players: Vec<AnyUserData> = host(lua).players.values().map(|e| e.userdata.clone()).collect();
            Ok(players)
        });
        methods.add_method("FindPlayer", |lua, _, name: String| {
            let host = host(lua);
            Ok(host
                .players
                .values()
                .find(|e| e.name.eq_ignore_ascii_case(&name))
                .map(|e| e.userdata.clone()))
        });
    }
}

pub fn install(lua: &Lua, game: &Table) -> mlua::Result<()> {
    game.set("Players", Players)?;
    let item = lua.create_table()?;
    item.set("new", lua.create_function(new_item)?)?;
    lua.globals().set("Item", item)
}
