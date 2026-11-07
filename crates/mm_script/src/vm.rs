//! One sandboxed Luau VM per instance, stepped once per tick on the async
//! compute pool. A step applies the tick's events, then runs ready threads
//! until they all yield or the CPU slice runs out; the interrupt then yields
//! the running thread so it continues next step.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, LuaOptions, MultiValue, StdLib, Thread, Value, VmState};
use mm_proto::ipc::DiagnosticLevel;
use mm_world::BlockMirror;

use crate::api;
use crate::bundle::{Bundle, ENTRY};
use crate::host::{Host, Resume, TICKS_PER_SECOND, host, thread_key};
use crate::io::{Event, Limits, Snapshot, StepOutput};

/// How long past its slice a thread that can't be yielded (inside a
/// metamethod, a `table.sort` comparator, or a coroutine of its own) may run
/// before it's raised an error instead.
const HARD_OVERRUN: Duration = Duration::from_millis(50);

/// Shared with the interrupt callback.
pub(crate) struct Budget {
    base: Instant,
    /// Nanoseconds since `base`.
    deadline: AtomicU64,
    hard_deadline: AtomicU64,
    /// Threads the scheduler is resuming, innermost last. Only the innermost
    /// may be yielded by the interrupt; any other yield would land in a
    /// script's own `coroutine.resume`.
    running: Mutex<Vec<usize>>,
    /// The thread the interrupt last yielded.
    yielded: AtomicUsize,
}

impl Budget {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            deadline: AtomicU64::new(u64::MAX),
            hard_deadline: AtomicU64::new(u64::MAX),
            running: Mutex::new(Vec::new()),
            yielded: AtomicUsize::new(0),
        }
    }

    fn now(&self) -> u64 {
        self.base.elapsed().as_nanos() as u64
    }

    fn start(&self, slice: Duration) {
        let now = self.now();
        let deadline = now + slice.as_nanos() as u64;
        self.deadline.store(deadline, Ordering::Relaxed);
        self.hard_deadline
            .store(deadline + HARD_OVERRUN.as_nanos() as u64, Ordering::Relaxed);
    }

    fn expired(&self) -> bool {
        self.now() >= self.deadline.load(Ordering::Relaxed)
    }

    fn interrupt(&self, lua: &Lua) -> mlua::Result<VmState> {
        let now = self.now();
        if now < self.deadline.load(Ordering::Relaxed) {
            return Ok(VmState::Continue);
        }
        // Luau ignores a yield where it can't happen, so past the hard
        // deadline the thread is stopped with an error instead.
        if now >= self.hard_deadline.load(Ordering::Relaxed) {
            return Err(mlua::Error::runtime("script ran too long without yielding"));
        }
        let current = thread_key(&lua.current_thread());
        if self.running.lock().unwrap().last() == Some(&current) {
            self.yielded.store(current, Ordering::Relaxed);
            return Ok(VmState::Yield);
        }
        Ok(VmState::Continue)
    }
}

pub struct Vm {
    lua: Lua,
    budget: Arc<Budget>,
    main: Option<Function>,
    overrun_streak: u32,
}

impl Vm {
    /// Compiles the bundle and sets up the sandbox. Runs no script code: the
    /// entry module starts on the first step, under the budget.
    pub fn new(bundle: Bundle, limits: Limits, blocks: BlockMirror) -> Result<Self, String> {
        if !bundle.has_entry() {
            return Err(format!("no `{ENTRY}`"));
        }
        let libs = StdLib::COROUTINE
            | StdLib::TABLE
            | StdLib::OS
            | StdLib::STRING
            | StdLib::UTF8
            | StdLib::BIT
            | StdLib::MATH
            | StdLib::BUFFER
            | StdLib::VECTOR;
        let lua = Lua::new_with(libs, LuaOptions::new()).map_err(|e| e.to_string())?;
        let memory = limits.memory;
        let budget = Arc::new(Budget::new());
        lua.set_app_data(Host::new(limits, blocks));
        let setup = || -> mlua::Result<Function> {
            api::install(&lua, &bundle)?;
            lua.sandbox(true)?;
            api::load_module(&lua, ENTRY)
        };
        let main = setup().map_err(|e| api::describe_error(&e))?;
        lua.set_memory_limit(memory).map_err(|e| e.to_string())?;
        let interrupt = budget.clone();
        lua.set_interrupt(move |lua| interrupt.interrupt(lua));
        lua.set_app_data(budget.clone());
        Ok(Self {
            lua,
            budget,
            main: Some(main),
            overrun_streak: 0,
        })
    }

    pub fn used_memory(&self) -> usize {
        self.lua.used_memory()
    }

    pub fn step(&mut self, snapshot: Snapshot, events: Vec<Event>) -> StepOutput {
        let lua = &self.lua;
        let slice = {
            let mut host = host(lua);
            host.tick = snapshot.tick;
            host.snapshot = snapshot.players.into_iter().map(|p| (p.id, p)).collect();
            if let Some(blocks) = snapshot.blocks {
                host.mirror = blocks;
            }
            host.limits.slice
        };
        self.budget.start(slice);

        // The entry module runs first, so it can connect handlers for the
        // events (players already in the instance) that arrive with it.
        let mut carried = Vec::new();
        if let Some(main) = self.main.take() {
            match lua.create_thread(main) {
                Ok(thread) => carried.extend(run_thread(lua, thread, Resume::Args(MultiValue::new()))),
                Err(e) => report(lua, &e),
            }
        }
        for event in events {
            if let Err(e) = api::dispatch(lua, event) {
                report(lua, &e);
            }
        }
        if let Err(e) = api::track_regions(lua) {
            report(lua, &e);
        }
        let tick = host(lua).tick;
        host(lua).sched.wake_timers(tick);

        let mut overran = false;
        loop {
            if self.budget.expired() {
                overran = true;
                break;
            }
            let Some(job) = host(lua).sched.pop() else {
                break;
            };
            if let Some(thread) = run_thread(lua, job.thread, job.resume) {
                carried.push(thread);
            }
        }
        let mut host = host(lua);
        if !carried.is_empty() {
            overran = true;
            let jobs = carried.into_iter().map(|t| host.sched.requeue(t)).collect();
            host.sched.push_front(jobs);
        }
        let (commands, diagnostics) = host.take_output();
        let mut out = StepOutput {
            commands,
            diagnostics,
            overran,
            runnable: host.sched.has_ready(),
            next_timer: host.sched.next_timer(),
            fatal: None,
        };
        let max_streak = host.limits.max_overrun_steps;
        drop(host);
        self.overrun_streak = if overran { self.overrun_streak + 1 } else { 0 };
        if self.overrun_streak > max_streak {
            out.fatal = Some(format!(
                "scripts used their whole time slice for {} ticks in a row",
                self.overrun_streak
            ));
        }
        out
    }
}

/// Resumes `thread` with `resume`'s values. Returns it if the budget
/// interrupt yielded it, so the caller can queue it to continue.
pub(crate) fn run_thread(lua: &Lua, thread: Thread, resume: Resume) -> Option<Thread> {
    let args = match resume {
        Resume::Args(args) => args,
        Resume::Elapsed(since) => {
            let ticks = host(lua).tick.saturating_sub(since);
            MultiValue::from_vec(vec![Value::Number(ticks as f64 / TICKS_PER_SECOND)])
        }
    };
    let budget = lua.app_data_ref::<Arc<Budget>>().expect("budget").clone();
    let key = thread_key(&thread);
    budget.running.lock().unwrap().push(key);
    let result = thread.resume::<MultiValue>(args);
    budget.running.lock().unwrap().pop();
    match result {
        Err(mlua::Error::CoroutineUnresumable) => None,
        Err(e) => {
            report(lua, &e);
            None
        }
        Ok(_) if thread.status() != ThreadStatus::Resumable => None,
        Ok(_) => {
            if budget.yielded.swap(0, Ordering::Relaxed) == key {
                return Some(thread);
            }
            let mut host = host(lua);
            if host.sched.parked == Some(key) {
                host.sched.parked = None;
            }
            // Otherwise a bare `coroutine.yield()`: the thread stays suspended
            // until something resumes it.
            None
        }
    }
}

pub(crate) fn report(lua: &Lua, e: &mlua::Error) {
    host(lua).diagnostic(DiagnosticLevel::Error, api::describe_error(e));
}
