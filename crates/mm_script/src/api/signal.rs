//! `RBXScriptSignal`-style signals: `:Connect(fn)`, `:Once(fn)`, `:Wait()`.

use mlua::{FromLua, Function, IntoLua, Lua, UserData, UserDataFields, UserDataMethods, Value};

use crate::host::{SignalId, host};

/// A handle to one of the host's signals. Copies refer to the same signal.
#[derive(Clone, Copy)]
pub struct Signal(pub(crate) SignalId);

impl IntoLua for Signal {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        lua.create_any_userdata(self).map(Value::UserData)
    }
}

impl FromLua for Signal {
    fn from_lua(value: Value, _: &Lua) -> mlua::Result<Self> {
        match &value {
            Value::UserData(ud) => Ok(*ud.borrow::<Self>()?),
            _ => Err(mlua::Error::runtime("expected a signal")),
        }
    }
}

pub struct Connection {
    signal: SignalId,
    id: Option<u32>,
}

impl UserData for Connection {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("Connected", |lua, c| {
            Ok(c.id.is_some_and(|id| host(lua).signals.is_connected(c.signal, id)))
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("Disconnect", |lua, c, ()| {
            if let Some(id) = c.id {
                host(lua).signals.disconnect(c.signal, id);
            }
            Ok(())
        });
    }
}

fn connect(lua: &Lua, signal: SignalId, f: Function, once: bool) -> Connection {
    Connection {
        signal,
        id: host(lua).signals.connect(signal, f, once),
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    // `Wait` has to yield, which only Luau code can do.
    let park = lua.create_function(|lua, signal: Signal| {
        let thread = lua.current_thread();
        let mut host = host(lua);
        if host.signals.wait(signal.0, thread.clone()) {
            host.sched.park(&thread);
            Ok(())
        } else {
            Err(mlua::Error::runtime("this signal can no longer fire"))
        }
    })?;
    let wait: Function = lua
        .load("local park, yield = ...\nreturn function(signal) park(signal) return yield() end")
        .set_name("=signal")
        .call((park, lua.globals().get::<mlua::Table>("coroutine")?.get::<Function>("yield")?))?;

    lua.register_userdata_type::<Signal>(move |reg| {
        reg.add_method("Connect", |lua, s, f: Function| Ok(connect(lua, s.0, f, false)));
        reg.add_method("Once", |lua, s, f: Function| Ok(connect(lua, s.0, f, true)));
        reg.add_field("Wait", wait);
        reg.add_meta_method("__tostring", |_, _, ()| Ok("Signal"));
    })
}
