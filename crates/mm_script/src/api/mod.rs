//! The globals scripts see. Roblox-flavored: `game.Players`, `game.World`,
//! `game.Rules`, `task`, `Vector3`, `Region`, `Item`, and `require` over the
//! game's own module tree.

mod player;
mod signal;
mod task;
mod vector;
mod world;

use std::collections::{HashMap, HashSet};

use chunkedge::prelude::*;
use mlua::chunk::{ChunkMode, Compiler};
use mlua::{Function, Lua, MultiValue, RegistryKey, Value};
use mm_proto::ipc::DiagnosticLevel;

pub use world::track_regions;

use crate::bundle::Bundle;
use crate::host::{PlayerEntry, host};
use crate::io::Event;

/// Error text is cut to this many bytes.
const MAX_ERROR_LEN: usize = 2000;

struct Modules {
    bundle: Bundle,
    bytecode: HashMap<String, Vec<u8>>,
    /// Modules that asked for native codegen and got it.
    native: HashSet<String>,
    loaded: HashMap<String, RegistryKey>,
    loading: HashSet<String>,
}

fn compiler() -> Compiler {
    Compiler::new().set_optimization_level(1).set_debug_level(1)
}

pub fn install(lua: &Lua, bundle: &Bundle) -> mlua::Result<()> {
    let max_native = host(lua).limits.max_native_modules;
    let compiler = compiler();
    let mut bytecode = HashMap::new();
    let mut native = HashSet::new();
    for (path, source) in &bundle.files {
        let code = compiler
            .compile(source)
            .map_err(|e| mlua::Error::runtime(format!("{path}: {e}")))?;
        if source.trim_start().starts_with("--!native") && native.len() < max_native {
            native.insert(path.clone());
        }
        bytecode.insert(path.clone(), code);
    }
    lua.set_app_data(Modules {
        bundle: bundle.clone(),
        bytecode,
        native,
        loaded: HashMap::new(),
        loading: HashSet::new(),
    });
    // Code is interpreted unless its module opts in (see `load_module`).
    lua.enable_jit(false);

    let globals = lua.globals();
    for name in ["loadstring", "getfenv", "setfenv", "gcinfo"] {
        globals.raw_remove(name)?;
    }
    let os: mlua::Table = globals.get("os")?;
    let safe_os = lua.create_table()?;
    for name in ["clock", "time", "date", "difftime"] {
        safe_os.set(name, os.get::<Value>(name)?)?;
    }
    globals.set("os", safe_os)?;
    globals.set("print", lua.create_function(|lua, args: MultiValue| log(lua, DiagnosticLevel::Info, args))?)?;
    globals.set("warn", lua.create_function(|lua, args: MultiValue| log(lua, DiagnosticLevel::Warn, args))?)?;
    globals.set("require", lua.create_function(require)?)?;

    signal::install(lua)?;
    task::install(lua)?;
    vector::install(lua)?;
    let game = lua.create_table()?;
    player::install(lua, &game)?;
    world::install(lua, &game)?;
    globals.set("game", game)
}

fn log(lua: &Lua, level: DiagnosticLevel, args: MultiValue) -> mlua::Result<()> {
    let parts: Vec<String> = args.iter().map(|v| v.to_string()).collect::<mlua::Result<_>>()?;
    let mut message = parts.join(" ");
    truncate(&mut message);
    host(lua).diagnostic(level, message);
    Ok(())
}

fn truncate(s: &mut String) {
    if s.len() > MAX_ERROR_LEN {
        let mut end = MAX_ERROR_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
}

/// Loads a compiled module as a function, without running it.
pub fn load_module(lua: &Lua, path: &str) -> mlua::Result<Function> {
    let (code, native) = {
        let modules = lua.app_data_ref::<Modules>().expect("modules");
        let code = modules.bytecode.get(path).cloned();
        (code, modules.native.contains(path))
    };
    let code = code.ok_or_else(|| mlua::Error::runtime(format!("no module `{path}`")))?;
    lua.enable_jit(native);
    let f = lua
        .load(&code[..])
        .set_mode(ChunkMode::Binary)
        .set_name(format!("@{path}"))
        .into_function();
    lua.enable_jit(false);
    f
}

/// The module that called the current Rust function.
fn calling_module(lua: &Lua) -> String {
    lua.inspect_stack(1, |d| d.source().source.map(|s| s.trim_start_matches('@').to_owned()))
        .flatten()
        .unwrap_or_default()
}

fn require(lua: &Lua, name: String) -> mlua::Result<Value> {
    let from = calling_module(lua);
    let path = {
        let modules = lua.app_data_ref::<Modules>().expect("modules");
        let path = modules
            .bundle
            .resolve(&from, &name)
            .ok_or_else(|| mlua::Error::runtime(format!("module `{name}` not found")))?
            .to_owned();
        if let Some(key) = modules.loaded.get(&path) {
            return lua.registry_value(key);
        }
        if modules.loading.contains(&path) {
            return Err(mlua::Error::runtime(format!("`{path}` requires itself (through `{from}`)")));
        }
        path
    };
    lua.app_data_mut::<Modules>().expect("modules").loading.insert(path.clone());
    // A module's body runs to completion inside this call and can't yield.
    let result = load_module(lua, &path).and_then(|f| f.call::<Value>(()));
    let mut modules = lua.app_data_mut::<Modules>().expect("modules");
    modules.loading.remove(&path);
    let value = match result? {
        Value::Nil => Value::Boolean(true),
        v => v,
    };
    let key = lua.create_registry_value(value.clone())?;
    modules.loaded.insert(path, key);
    Ok(value)
}

pub fn describe_error(e: &mlua::Error) -> String {
    let mut text = match e {
        mlua::Error::CallbackError { traceback, cause } => format!("{cause}\n{traceback}"),
        mlua::Error::MemoryError(_) => "out of memory".to_owned(),
        mlua::Error::RuntimeError(msg) => msg.clone(),
        e => e.to_string(),
    };
    truncate(&mut text);
    text
}

/// Applies one main-thread event and fires the signals it maps to.
pub fn dispatch(lua: &Lua, event: Event) -> mlua::Result<()> {
    match event {
        Event::PlayerAdded { id, name, uuid } => {
            let userdata = lua.create_userdata(player::Player {
                id,
                name: name.clone(),
                uuid,
            })?;
            let signal = {
                let mut host = host(lua);
                let entry = PlayerEntry {
                    userdata: userdata.clone(),
                    name,
                    chatted: host.signals.create(),
                    died: host.signals.create(),
                    respawned: host.signals.create(),
                };
                host.players.insert(id, entry);
                host.game.player_added
            };
            crate::host::fire(lua, signal, userdata)
        }
        Event::PlayerRemoving { id } => {
            let (signal, entry) = {
                let mut host = host(lua);
                let Some(entry) = host.players.remove(&id) else {
                    return Ok(());
                };
                for s in [entry.chatted, entry.died, entry.respawned] {
                    host.signals.destroy(s);
                }
                host.snapshot.remove(&id);
                (host.game.player_removing, entry)
            };
            crate::host::fire(lua, signal, entry.userdata)
        }
        Event::Chatted { id, message } => {
            let Some(signal) = host(lua).players.get(&id).map(|e| e.chatted) else {
                return Ok(());
            };
            crate::host::fire(lua, signal, message)
        }
        Event::Died { id } | Event::Respawned { id } => {
            let died = matches!(event, Event::Died { .. });
            let target = host(lua)
                .players
                .get(&id)
                .map(|e| (if died { e.died } else { e.respawned }, e.userdata.clone()));
            match target {
                Some((signal, player)) => crate::host::fire(lua, signal, player),
                None => Ok(()),
            }
        }
        Event::BlockBroken { player, pos, old } => block_event(lua, player, pos, old, BlockState::AIR, false),
        Event::BlockPlaced { player, pos, old, new } => block_event(lua, player, pos, old, new, true),
    }
}

fn block_event(
    lua: &Lua,
    player: crate::io::PlayerId,
    pos: [i32; 3],
    old: BlockState,
    new: BlockState,
    placed: bool,
) -> mlua::Result<()> {
    let (signal, player) = {
        let mut host = host(lua);
        host.mirror.set(pos, new);
        let signal = if placed { host.game.block_placed } else { host.game.block_broken };
        (signal, host.players.get(&player).map(|e| e.userdata.clone()))
    };
    let event = world::BlockEvent { player, pos, old, new };
    crate::host::fire(lua, signal, event)
}
