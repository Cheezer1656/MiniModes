//! The `task` library, with Roblox semantics measured in ticks (20 per second).

use mlua::{Function, Lua, MultiValue, Table, Thread, Value};

use crate::host::{Resume, TICKS_PER_SECOND, host};
use crate::vm::run_thread;

fn ticks(seconds: Option<f64>) -> u64 {
    let s = seconds.unwrap_or(0.0);
    if s.is_nan() || s <= 0.0 {
        1
    } else {
        ((s * TICKS_PER_SECOND).ceil() as u64).clamp(1, 1 << 40)
    }
}

fn thread_of(lua: &Lua, f: Value) -> mlua::Result<Thread> {
    match f {
        Value::Function(f) => lua.create_thread(f),
        Value::Thread(t) => Ok(t),
        other => Err(mlua::Error::runtime(format!(
            "expected a function or thread, got {}",
            other.type_name()
        ))),
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let task = lua.create_table()?;

    // Runs now, until its first yield.
    task.set(
        "spawn",
        lua.create_function(|lua, (f, args): (Value, MultiValue)| {
            let thread = thread_of(lua, f)?;
            if let Some(interrupted) = run_thread(lua, thread.clone(), Resume::Args(args)) {
                let mut host = host(lua);
                let job = host.sched.requeue(interrupted);
                host.sched.push_front(vec![job]);
            }
            Ok(thread)
        })?,
    )?;
    // Runs after the current thread yields, still this step.
    task.set(
        "defer",
        lua.create_function(|lua, (f, args): (Value, MultiValue)| {
            let thread = thread_of(lua, f)?;
            host(lua).sched.push(thread.clone(), Resume::Args(args));
            Ok(thread)
        })?,
    )?;
    task.set(
        "delay",
        lua.create_function(|lua, (seconds, f, args): (Option<f64>, Value, MultiValue)| {
            let thread = thread_of(lua, f)?;
            let mut host = host(lua);
            let at = host.tick + ticks(seconds);
            host.sched.sleep(thread.clone(), at, Resume::Args(args));
            Ok(thread)
        })?,
    )?;
    task.set(
        "cancel",
        lua.create_function(|lua, thread: Thread| {
            host(lua).sched.cancel(&thread);
            Ok(())
        })?,
    )?;

    let park = lua.create_function(|lua, seconds: Option<f64>| {
        let thread = lua.current_thread();
        let mut host = host(lua);
        let (now, at) = (host.tick, host.tick + ticks(seconds));
        host.sched.sleep(thread.clone(), at, Resume::Elapsed(now));
        host.sched.parked = Some(crate::host::thread_key(&thread));
        Ok(())
    })?;
    let coroutine: Table = lua.globals().get("coroutine")?;
    let wait: Function = lua
        .load("local park, yield = ...\nreturn function(seconds) park(seconds) return yield() end")
        .set_name("=task")
        .call((park, coroutine.get::<Function>("yield")?))?;
    task.set("wait", wait)?;

    lua.globals().set("task", task)?;
    lua.globals().set(
        "time",
        lua.create_function(|lua, ()| Ok(host(lua).tick as f64 / TICKS_PER_SECOND))?,
    )
}
