//! Game-wide fields: `game.GameId`, `game.Version`, `game.IsDraft`,
//! `game.LastShutdown`, `game:BindToClose`, and `game.Match` in match games.

use mlua::{Function, Lua, Table, UserData, UserDataFields, UserDataMethods, Value};
use mm_proto::GameKind;

use super::player::Player;
use super::signal::Signal;
use crate::host::host;
use crate::io::{Command, MatchPhase, PlayerId};

/// Longest countdown scripts may ask for.
const MAX_COUNTDOWN: u32 = 60;

pub fn install(lua: &Lua, game: &Table) -> mlua::Result<()> {
    let info = host(lua).info.clone();
    game.set("GameId", info.game_id)?;
    game.set("Version", info.version)?;
    game.set("IsDraft", info.version.is_none())?;
    game.set("LastShutdown", info.last_shutdown)?;
    game.set(
        "BindToClose",
        lua.create_function(|lua, (_, f): (Value, Function)| {
            host(lua).close_handlers.push(f);
            Ok(())
        })?,
    )?;
    if info.kind == GameKind::Match {
        game.set("Match", MatchApi)?;
    }
    Ok(())
}

struct MatchApi;

fn settings(lua: &Lua) {
    let mut host = host(lua);
    let command = Command::MatchSettings {
        min_players: host.matches.min_players,
        countdown: host.matches.countdown,
    };
    host.command(command);
}

/// `nil`, one player, or a list of players.
fn winners(value: Value) -> mlua::Result<Vec<PlayerId>> {
    let one = |v: &Value| -> mlua::Result<PlayerId> {
        match v {
            Value::UserData(ud) => Ok(ud.borrow::<Player>()?.id),
            _ => Err(mlua::Error::runtime("winners must be players")),
        }
    };
    match &value {
        Value::Nil => Ok(Vec::new()),
        Value::Table(t) => t.sequence_values::<Value>().map(|v| one(&v?)).collect(),
        v => Ok(vec![one(v)?]),
    }
}

impl UserData for MatchApi {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("State", |lua, _| Ok(host(lua).matches.phase.name()));
        fields.add_field_method_get("MaxPlayers", |lua, _| Ok(host(lua).info.max_players));
        fields.add_field_method_get("MinPlayers", |lua, _| Ok(host(lua).matches.min_players));
        fields.add_field_method_set("MinPlayers", |lua, _, n: u32| {
            {
                let mut host = host(lua);
                host.matches.min_players = n.clamp(1, host.info.max_players.max(1));
            }
            settings(lua);
            Ok(())
        });
        fields.add_field_method_get("CountdownSeconds", |lua, _| Ok(host(lua).matches.countdown));
        fields.add_field_method_set("CountdownSeconds", |lua, _, n: u32| {
            host(lua).matches.countdown = n.min(MAX_COUNTDOWN);
            settings(lua);
            Ok(())
        });
        fields.add_field_method_get("Starting", |lua, _| Ok(Signal(host(lua).game.match_starting)));
        fields.add_field_method_get("Started", |lua, _| Ok(Signal(host(lua).game.match_started)));
        fields.add_field_method_get("Ended", |lua, _| Ok(Signal(host(lua).game.match_ended)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("End", |lua, _, value: Value| {
            let winners = winners(value)?;
            let mut host = host(lua);
            if host.matches.phase == MatchPhase::Ended {
                return Ok(());
            }
            host.command(Command::EndMatch { winners });
            Ok(())
        });
        methods.add_meta_method("__tostring", |_, _, ()| Ok("Match"));
    }
}

pub fn match_event(lua: &Lua, phase: MatchPhase, seconds: u32, winners: Vec<PlayerId>) -> mlua::Result<()> {
    let (signal, players) = {
        let mut host = host(lua);
        host.matches.phase = phase;
        let signal = match phase {
            MatchPhase::Waiting => return Ok(()),
            MatchPhase::Starting => host.game.match_starting,
            MatchPhase::Started => host.game.match_started,
            MatchPhase::Ended => host.game.match_ended,
        };
        let players: Vec<_> = winners
            .iter()
            .filter_map(|id| host.players.get(id).map(|e| e.userdata.clone()))
            .collect();
        (signal, players)
    };
    match phase {
        MatchPhase::Starting => crate::host::fire(lua, signal, seconds),
        MatchPhase::Ended => crate::host::fire(lua, signal, lua.create_sequence_from(players)?),
        _ => crate::host::fire(lua, signal, ()),
    }
}

/// Starts every `BindToClose` callback on its own thread.
pub fn closing(lua: &Lua) -> mlua::Result<()> {
    let handlers = {
        let mut host = host(lua);
        if host.closing.is_some() {
            return Ok(());
        }
        std::mem::take(&mut host.close_handlers)
    };
    let threads = handlers
        .into_iter()
        .map(|f| lua.create_thread(f))
        .collect::<mlua::Result<Vec<_>>>()?;
    let mut host = host(lua);
    for thread in &threads {
        host.sched.push(thread.clone(), crate::host::Resume::Args(mlua::MultiValue::new()));
    }
    host.closing = Some(threads);
    Ok(())
}
