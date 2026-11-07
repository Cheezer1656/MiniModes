//! `DataStoreService`: per-game key-value storage that outlives instances.
//! Calls yield: the request goes to the host as a command, and the reply
//! comes back as an event that resumes the thread. Values are plain data
//! (nil, booleans, numbers, strings, and tables of those) stored as JSON.

use mlua::{Function, Lua, MultiValue, Table, UserDataFields, UserDataMethods, Value};

use crate::host::{DataOp, Resume, host};
use crate::io::{Command, DataEntry, DataReply, DataRequest, DataValue, RequestId, StoreRef};

const MAX_NAME_LEN: usize = 50;
const MAX_KEY_LEN: usize = 50;
const MAX_DEPTH: usize = 32;
const MAX_PAGE_SIZE: u32 = 100;
/// Integers Luau numbers hold exactly.
const MAX_SAFE_INT: f64 = 9_007_199_254_740_991.0;

#[derive(Clone)]
struct DataStore(StoreRef);

fn name_arg(what: &str, s: &str, max: usize) -> mlua::Result<()> {
    if s.is_empty() || s.len() > max {
        return Err(mlua::Error::runtime(format!("{what} must be 1 to {max} bytes")));
    }
    Ok(())
}

fn integer(what: &str, n: f64) -> mlua::Result<i64> {
    if n.fract() != 0.0 || n.abs() > MAX_SAFE_INT {
        return Err(mlua::Error::runtime(format!("{what} must be a whole number")));
    }
    Ok(n as i64)
}

/// Lua → JSON, for storing.
fn to_json(value: &Value, depth: usize) -> mlua::Result<serde_json::Value> {
    use serde_json::Value as J;
    if depth > MAX_DEPTH {
        return Err(mlua::Error::runtime("value is nested too deeply"));
    }
    Ok(match value {
        Value::Nil => J::Null,
        Value::Boolean(b) => J::Bool(*b),
        Value::Integer(n) => J::from(*n),
        Value::Number(n) => match serde_json::Number::from_f64(*n) {
            Some(n) => J::Number(n),
            None => return Err(mlua::Error::runtime("can't store NaN or infinity")),
        },
        Value::String(s) => J::String(
            s.to_str()
                .map_err(|_| mlua::Error::runtime("can't store strings that aren't UTF-8"))?
                .to_owned(),
        ),
        Value::Table(t) => {
            let len = t.raw_len();
            let mut count = 0;
            let mut object = serde_json::Map::new();
            let mut array = vec![J::Null; len];
            for pair in t.clone().pairs::<Value, Value>() {
                let (k, v) = pair?;
                count += 1;
                match k {
                    Value::Integer(i) if (1..=len as i64).contains(&i) => array[i as usize - 1] = to_json(&v, depth + 1)?,
                    Value::Number(n) if n.fract() == 0.0 && n >= 1.0 && n <= len as f64 => {
                        array[n as usize - 1] = to_json(&v, depth + 1)?
                    }
                    Value::String(s) => {
                        let key = s
                            .to_str()
                            .map_err(|_| mlua::Error::runtime("can't store keys that aren't UTF-8"))?
                            .to_owned();
                        object.insert(key, to_json(&v, depth + 1)?);
                    }
                    other => {
                        return Err(mlua::Error::runtime(format!(
                            "can't store a table with {} keys (use strings, or an array)",
                            other.type_name()
                        )));
                    }
                }
            }
            match (object.is_empty(), len == 0) {
                (true, _) if count == len => J::Array(array),
                (true, _) => return Err(mlua::Error::runtime("can't store an array with holes")),
                (false, true) => J::Object(object),
                (false, false) => {
                    return Err(mlua::Error::runtime("can't store a table that mixes array items and string keys"));
                }
            }
        }
        other => return Err(mlua::Error::runtime(format!("can't store a {}", other.type_name()))),
    })
}

/// JSON → Lua, for reading back.
fn from_json(lua: &Lua, value: &serde_json::Value) -> mlua::Result<Value> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => Value::Nil,
        J::Bool(b) => Value::Boolean(*b),
        J::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        J::String(s) => Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table_with_capacity(items.len(), 0)?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, from_json(lua, item)?)?;
            }
            Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table_with_capacity(0, map.len())?;
            for (k, v) in map {
                t.raw_set(k.as_str(), from_json(lua, v)?)?;
            }
            Value::Table(t)
        }
    })
}

fn encode(lua: &Lua, store: &StoreRef, value: &Value) -> mlua::Result<DataValue> {
    if store.ordered {
        return match value {
            Value::Integer(n) => Ok(DataValue::Int(*n)),
            Value::Number(n) => integer("ordered store values", *n).map(DataValue::Int),
            _ => Err(mlua::Error::runtime("ordered store values must be whole numbers")),
        };
    }
    if value.is_nil() {
        return Err(mlua::Error::runtime("can't store nil (use RemoveAsync)"));
    }
    let json = to_json(value, 0)?.to_string();
    let max = host(lua).limits.max_data_value;
    if json.len() > max {
        return Err(mlua::Error::runtime(format!("value is {} bytes, the limit is {max}", json.len())));
    }
    Ok(DataValue::Json(json))
}

fn decode(lua: &Lua, value: &DataValue) -> mlua::Result<Value> {
    match value {
        DataValue::Int(n) => Ok(Value::Number(*n as f64)),
        DataValue::Json(s) => {
            let json: serde_json::Value =
                serde_json::from_str(s).map_err(|e| mlua::Error::runtime(format!("stored value is corrupt: {e}")))?;
            from_json(lua, &json)
        }
    }
}

fn entry_values(lua: &Lua, entry: Option<&DataEntry>) -> mlua::Result<(Value, Value)> {
    Ok(match entry {
        Some(e) => (decode(lua, &e.value)?, Value::Number(e.version as f64)),
        None => (Value::Nil, Value::Nil),
    })
}

/// Queues a request for the running thread, which the Luau wrapper then
/// yields. Arguments depend on `op`.
fn submit(lua: &Lua, (store, op, args): (mlua::UserDataRef<DataStore>, String, MultiValue)) -> mlua::Result<()> {
    let store = store.0.clone();
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Nil);
    let key = || -> mlua::Result<String> {
        let key = match arg(0) {
            Value::String(s) => s.to_str()?.to_owned(),
            Value::Integer(n) => n.to_string(),
            Value::Number(n) => integer("keys", n)?.to_string(),
            _ => return Err(mlua::Error::runtime("keys must be strings")),
        };
        name_arg("keys", &key, MAX_KEY_LEN)?;
        Ok(key)
    };
    let opt_int = |what: &str, v: Value| -> mlua::Result<Option<i64>> {
        match v {
            Value::Nil => Ok(None),
            Value::Integer(n) => Ok(Some(n)),
            Value::Number(n) => integer(what, n).map(Some),
            _ => Err(mlua::Error::runtime(format!("{what} must be a number"))),
        }
    };
    let (kind, request) = match op.as_str() {
        "get" => (DataOp::Get, DataRequest::Get { key: key()? }),
        "set" => (
            DataOp::Set,
            DataRequest::Set {
                key: key()?,
                value: encode(lua, &store, &arg(1))?,
                expect: None,
            },
        ),
        "cas" => (
            DataOp::CompareSet,
            DataRequest::Set {
                key: key()?,
                value: encode(lua, &store, &arg(1))?,
                expect: Some(opt_int("version", arg(2))?),
            },
        ),
        "inc" => (
            DataOp::Increment,
            DataRequest::Increment {
                key: key()?,
                delta: opt_int("the increment", arg(1))?.unwrap_or(1),
            },
        ),
        "remove" => (DataOp::Remove, DataRequest::Remove { key: key()? }),
        "sorted" => {
            if !store.ordered {
                return Err(mlua::Error::runtime("GetSortedAsync needs an ordered store"));
            }
            let limit = opt_int("pageSize", arg(1))?.unwrap_or(50);
            if !(1..=i64::from(MAX_PAGE_SIZE)).contains(&limit) {
                return Err(mlua::Error::runtime(format!("pageSize must be 1 to {MAX_PAGE_SIZE}")));
            }
            let after = match (opt_int("cursor", arg(4))?, arg(5)) {
                (Some(v), Value::String(k)) => Some((v, k.to_str()?.to_owned())),
                _ => None,
            };
            (
                DataOp::Sorted,
                DataRequest::Sorted {
                    ascending: arg(0).as_boolean().unwrap_or(false),
                    // One extra tells whether another page follows.
                    limit: limit as u32 + 1,
                    min: opt_int("minValue", arg(2))?,
                    max: opt_int("maxValue", arg(3))?,
                    after,
                },
            )
        }
        other => return Err(mlua::Error::runtime(format!("unknown DataStore operation `{other}`"))),
    };

    let thread = lua.current_thread();
    let mut host = host(lua);
    let per_minute = f64::from(host.limits.data_requests_per_minute)
        + f64::from(host.limits.data_requests_per_player) * host.players.len() as f64;
    let tick = host.tick;
    if let Err(limit) = host.data.take(tick, per_minute) {
        return Err(mlua::Error::runtime(format!(
            "too many DataStore requests (this game may make {limit} a minute right now)"
        )));
    }
    host.data.next += 1;
    let request_id = host.data.next;
    host.data.waiting.insert(request_id, (thread.clone(), kind));
    host.sched.park(&thread);
    host.command(Command::Data {
        request: request_id,
        store,
        op: request,
    });
    Ok(())
}

/// Resumes the thread waiting on `request` with `(true, results...)`, or
/// `(false, message)` for the wrapper to raise.
pub fn reply(lua: &Lua, request: RequestId, result: Result<DataReply, String>) -> mlua::Result<()> {
    let Some((thread, op)) = host(lua).data.waiting.remove(&request) else {
        return Ok(());
    };
    let values: Vec<Value> = match result {
        Err(message) => vec![Value::Boolean(false), Value::String(lua.create_string(message)?)],
        Ok(reply) => {
            let mut values = vec![Value::Boolean(true)];
            match (op, reply) {
                (DataOp::Get, DataReply::Entry(entry)) => {
                    let (value, version) = entry_values(lua, entry.as_ref())?;
                    values.extend([value, version]);
                }
                (DataOp::Remove, DataReply::Entry(entry)) => values.push(entry_values(lua, entry.as_ref())?.0),
                (DataOp::Set, DataReply::Written(_)) => {}
                (DataOp::Increment, DataReply::Written(entry)) => values.push(decode(lua, &entry.value)?),
                (DataOp::CompareSet, DataReply::Written(_)) => values.push(Value::Boolean(true)),
                (DataOp::CompareSet, DataReply::Conflict(entry)) => {
                    let (value, version) = entry_values(lua, entry.as_ref())?;
                    values.extend([Value::Boolean(false), value, version]);
                }
                (DataOp::Sorted, DataReply::Sorted(entries)) => {
                    let list = lua.create_table_with_capacity(entries.len(), 0)?;
                    for (key, value) in entries {
                        let item = lua.create_table_with_capacity(0, 2)?;
                        item.raw_set("key", key)?;
                        item.raw_set("value", value as f64)?;
                        list.raw_push(item)?;
                    }
                    values.push(Value::Table(list));
                }
                (op, reply) => {
                    values = vec![
                        Value::Boolean(false),
                        Value::String(lua.create_string(format!("unexpected reply {reply:?} to {op:?}"))?),
                    ];
                }
            }
            values
        }
    };
    host(lua).sched.wake(thread, Resume::Args(MultiValue::from_vec(values)));
    Ok(())
}

/// The yielding methods, which only Luau code can implement. `call` raises
/// errors at `level`, which points at the script line that made the call.
const METHODS: &str = r#"
local submit, yield, pack, unpack, newStore = ...

local function call(level, store, op, ...)
	submit(store, op, ...)
	local result = pack(yield())
	if not result[1] then
		error(result[2], level)
	end
	return unpack(result, 2, result.n)
end

local methods = {}

function methods.GetAsync(self, key)
	local value = call(3, self, "get", key)
	return value
end

function methods.SetAsync(self, key, value)
	call(3, self, "set", key, value)
end

function methods.IncrementAsync(self, key, delta)
	return call(3, self, "inc", key, delta)
end

function methods.RemoveAsync(self, key)
	return call(3, self, "remove", key)
end

-- `transform` gets the current value and returns the new one, or nil to
-- leave it. It runs again if another server wrote the key in between.
function methods.UpdateAsync(self, key, transform)
	local value, version = call(3, self, "get", key)
	for _ = 1, 8 do
		local new = transform(value)
		if new == nil then
			return nil
		end
		local written, current, currentVersion = call(3, self, "cas", key, new, version)
		if written then
			return new
		end
		value, version = current, currentVersion
	end
	error("UpdateAsync: the key kept changing; try again later", 2)
end

local function fetch(pages, level)
	local s = pages._state
	local list = call(level, s.store, "sorted", s.ascending, s.size, s.min, s.max, s.afterValue, s.afterKey)
	local page = {}
	for i = 1, math.min(#list, s.size) do
		page[i] = list[i]
	end
	local last = page[#page]
	if last then
		s.afterValue, s.afterKey = last.value, last.key
	end
	s.page = page
	pages.IsFinished = #list <= s.size
end

function methods.GetSortedAsync(self, ascending, pageSize, minValue, maxValue)
	local pages = {
		IsFinished = false,
		_state = {
			store = self,
			ascending = ascending == true,
			size = pageSize or 50,
			min = minValue,
			max = maxValue,
		},
	}
	function pages.GetCurrentPage(p)
		local copy = {}
		for i, item in p._state.page do
			copy[i] = { key = item.key, value = item.value }
		end
		return copy
	end
	function pages.AdvanceToNextPageAsync(p)
		if p.IsFinished then
			error("there are no more pages", 2)
		end
		fetch(p, 4)
	end
	fetch(pages, 4)
	return pages
end

local service = {}

function service.GetDataStore(_, name)
	return newStore(name, false)
end

function service.GetOrderedDataStore(_, name)
	return newStore(name, true)
end

return methods, service
"#;

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let coroutine: Table = globals.get("coroutine")?;
    let table: Table = globals.get("table")?;
    let new_store = lua.create_function(|lua, (name, ordered): (String, bool)| {
        name_arg("DataStore names", &name, MAX_NAME_LEN)?;
        lua.create_any_userdata(DataStore(StoreRef { name, ordered }))
    })?;
    let (methods, service): (Table, Table) = lua.load(METHODS).set_name("=DataStoreService").call((
        lua.create_function(submit)?,
        coroutine.get::<Function>("yield")?,
        table.get::<Function>("pack")?,
        table.get::<Function>("unpack")?,
        new_store,
    ))?;
    lua.register_userdata_type::<DataStore>(move |reg| {
        for pair in methods.pairs::<String, Function>() {
            let (name, f) = pair.expect("method table");
            reg.add_field(name, f);
        }
        reg.add_field_method_get("Name", |_, s| Ok(s.0.name.clone()));
        reg.add_meta_method("__tostring", |_, s, ()| {
            Ok(format!("{}DataStore({})", if s.0.ordered { "Ordered" } else { "" }, s.0.name))
        });
    })?;
    globals.set("DataStoreService", service)
}
