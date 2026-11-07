//! The VM's Rust-side state, stored as Lua app data so API callbacks can
//! reach it. Borrows of it must never be held across a call into Lua.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};

use chunkedge::prelude::*;
use mlua::{AnyUserData, Function, IntoLuaMulti, Lua, MultiValue, Thread};
use mm_proto::ipc::DiagnosticLevel;
use mm_world::{BlockMirror, Bounds};

use crate::io::{Command, Diagnostic, GameInfo, Limits, MatchPhase, PlayerId, PlayerSnap, RegionId};
use crate::rules::Rules;

pub(crate) const TICKS_PER_SECOND: f64 = 20.0;
/// Match defaults until scripts change them; the host uses the same.
pub const DEFAULT_MIN_PLAYERS: u32 = 2;
pub const DEFAULT_COUNTDOWN: u32 = 10;

pub(crate) fn host(lua: &Lua) -> mlua::AppDataRefMut<'_, Host> {
    lua.app_data_mut::<Host>().expect("VM host state")
}

pub(crate) fn thread_key(thread: &Thread) -> usize {
    thread.to_pointer() as usize
}

pub(crate) struct Host {
    pub tick: u64,
    pub limits: Limits,
    pub snapshot: HashMap<PlayerId, PlayerSnap>,
    /// Players in the instance, in join order.
    pub players: BTreeMap<PlayerId, PlayerEntry>,
    pub mirror: BlockMirror,
    out: Vec<Command>,
    dropped_commands: usize,
    diagnostics: Vec<Diagnostic>,
    dropped_diagnostics: usize,
    pub signals: Signals,
    pub sched: Sched,
    pub regions: BTreeMap<RegionId, Region>,
    pub next_region: RegionId,
    pub game_rules: Rules,
    pub game: GameSignals,
    pub info: GameInfo,
    pub matches: MatchView,
    /// `game:BindToClose` callbacks.
    pub close_handlers: Vec<Function>,
    /// Threads running them, once the instance started closing.
    pub closing: Option<Vec<Thread>>,
    rng: u64,
}

/// The match as scripts see it; the host owns the real state.
pub(crate) struct MatchView {
    pub phase: MatchPhase,
    pub min_players: u32,
    pub countdown: u32,
}

pub(crate) struct GameSignals {
    pub player_added: SignalId,
    pub player_removing: SignalId,
    pub block_broken: SignalId,
    pub block_placed: SignalId,
    pub match_starting: SignalId,
    pub match_started: SignalId,
    pub match_ended: SignalId,
}

pub(crate) struct PlayerEntry {
    pub userdata: AnyUserData,
    pub name: String,
    pub chatted: SignalId,
    pub died: SignalId,
    pub respawned: SignalId,
}

pub(crate) struct Region {
    pub bounds: Bounds,
    pub rules: Rules,
    pub inside: HashSet<PlayerId>,
    pub entered: SignalId,
    pub left: SignalId,
}

impl Host {
    pub fn new(limits: Limits, mirror: BlockMirror, info: GameInfo) -> Self {
        let mut signals = Signals::default();
        let game = GameSignals {
            player_added: signals.create(),
            player_removing: signals.create(),
            block_broken: signals.create(),
            block_placed: signals.create(),
            match_starting: signals.create(),
            match_started: signals.create(),
            match_ended: signals.create(),
        };
        let matches = MatchView {
            phase: MatchPhase::Waiting,
            min_players: DEFAULT_MIN_PLAYERS.min(info.max_players),
            countdown: DEFAULT_COUNTDOWN,
        };
        Self {
            tick: 0,
            limits,
            snapshot: HashMap::new(),
            players: BTreeMap::new(),
            mirror,
            out: Vec::new(),
            dropped_commands: 0,
            diagnostics: Vec::new(),
            dropped_diagnostics: 0,
            signals,
            sched: Sched::default(),
            regions: BTreeMap::new(),
            next_region: 1,
            game_rules: Rules::default(),
            game,
            info,
            matches,
            close_handlers: Vec::new(),
            closing: None,
            rng: 0x9e37_79b9_7f4a_7c15,
        }
    }

    pub fn command(&mut self, command: Command) {
        if self.out.len() < self.limits.max_commands {
            self.out.push(command);
        } else {
            self.dropped_commands += 1;
        }
    }

    /// Commands for a player who has left are dropped.
    pub fn player_command(&mut self, player: PlayerId, command: Command) {
        if self.players.contains_key(&player) {
            self.command(command);
        }
    }

    pub fn diagnostic(&mut self, level: DiagnosticLevel, message: String) {
        if self.diagnostics.len() < self.limits.max_diagnostics {
            self.diagnostics.push(Diagnostic { level, message });
        } else {
            self.dropped_diagnostics += 1;
        }
    }

    pub fn take_output(&mut self) -> (Vec<Command>, Vec<Diagnostic>) {
        if self.dropped_commands > 0 {
            let n = std::mem::take(&mut self.dropped_commands);
            self.diagnostic(DiagnosticLevel::Warn, format!("dropped {n} commands over the per-tick limit"));
        }
        if self.dropped_diagnostics > 0 {
            let n = std::mem::take(&mut self.dropped_diagnostics);
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Warn,
                message: format!("{n} more messages suppressed"),
            });
        }
        (std::mem::take(&mut self.out), std::mem::take(&mut self.diagnostics))
    }

    /// Sets blocks in the mirror and queues the changes for the host.
    pub fn set_blocks(&mut self, blocks: impl IntoIterator<Item = ([i32; 3], BlockState)>) {
        let changed: Vec<_> = blocks
            .into_iter()
            .filter(|&(p, state)| self.mirror.set(p, state).is_some_and(|old| old != state))
            .collect();
        if !changed.is_empty() {
            self.command(Command::SetBlocks(changed));
        }
    }

    /// xorshift64*; good enough for picking blocks.
    pub fn random(&mut self) -> u64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn region_rules_command(&self, id: RegionId) -> Option<Command> {
        let region = self.regions.get(&id)?;
        Some(Command::SetRules {
            region: Some((id, region.bounds)),
            rules: region.rules.clone(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SignalId(pub u32);

struct Handler {
    id: u32,
    function: Function,
    once: bool,
}

#[derive(Default)]
struct SignalState {
    handlers: Vec<Handler>,
    waiting: Vec<Thread>,
    next_connection: u32,
}

/// Every signal of the VM. Destroyed signals (a departed player's) keep
/// their slot so stale ids stay harmless.
#[derive(Default)]
pub(crate) struct Signals(Vec<Option<SignalState>>);

impl Signals {
    pub fn create(&mut self) -> SignalId {
        self.0.push(Some(SignalState::default()));
        SignalId(self.0.len() as u32 - 1)
    }

    pub fn destroy(&mut self, id: SignalId) {
        if let Some(slot) = self.0.get_mut(id.0 as usize) {
            *slot = None;
        }
    }

    fn get(&mut self, id: SignalId) -> Option<&mut SignalState> {
        self.0.get_mut(id.0 as usize)?.as_mut()
    }

    /// Returns the connection id, or `None` for a destroyed signal.
    pub fn connect(&mut self, id: SignalId, function: Function, once: bool) -> Option<u32> {
        let signal = self.get(id)?;
        signal.next_connection += 1;
        let conn = signal.next_connection;
        signal.handlers.push(Handler { id: conn, function, once });
        Some(conn)
    }

    pub fn disconnect(&mut self, id: SignalId, conn: u32) {
        if let Some(signal) = self.get(id) {
            signal.handlers.retain(|h| h.id != conn);
        }
    }

    pub fn is_connected(&mut self, id: SignalId, conn: u32) -> bool {
        self.get(id).is_some_and(|s| s.handlers.iter().any(|h| h.id == conn))
    }

    pub fn wait(&mut self, id: SignalId, thread: Thread) -> bool {
        self.get(id).map(|s| s.waiting.push(thread)).is_some()
    }

    /// Handlers to call and threads to resume for one firing.
    fn take_fire(&mut self, id: SignalId) -> (Vec<Function>, Vec<Thread>) {
        let Some(signal) = self.get(id) else {
            return (Vec::new(), Vec::new());
        };
        let handlers = signal.handlers.iter().map(|h| h.function.clone()).collect();
        signal.handlers.retain(|h| !h.once);
        (handlers, std::mem::take(&mut signal.waiting))
    }
}

/// Queues every handler of `signal` (each on a new thread) and every thread
/// waiting on it. They run later in this step, in connection order.
pub(crate) fn fire(lua: &Lua, signal: SignalId, args: impl IntoLuaMulti) -> mlua::Result<()> {
    let (handlers, waiting) = host(lua).signals.take_fire(signal);
    if handlers.is_empty() && waiting.is_empty() {
        return Ok(());
    }
    let args = args.into_lua_multi(lua)?;
    let mut threads = Vec::with_capacity(handlers.len() + waiting.len());
    for f in handlers {
        threads.push(lua.create_thread(f)?);
    }
    let mut host = host(lua);
    for thread in threads {
        host.sched.push(thread, Resume::Args(args.clone()));
    }
    for thread in waiting {
        host.sched.wake(thread, Resume::Args(args.clone()));
    }
    Ok(())
}

pub(crate) enum Resume {
    Args(MultiValue),
    /// `task.wait`: resume with the seconds since this tick.
    Elapsed(u64),
}

pub(crate) struct Job {
    pub thread: Thread,
    pub resume: Resume,
    token: u64,
}

struct Timer {
    at: u64,
    seq: u64,
    job: Job,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Timer {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.at, self.seq).cmp(&(other.at, other.seq))
    }
}

/// Threads waiting to run. A thread has at most one live job: scheduling it
/// again, resuming it, or `task.cancel` invalidates older ones, so a stale
/// timer can never resume a thread that moved on.
#[derive(Default)]
pub(crate) struct Sched {
    ready: VecDeque<Job>,
    timers: BinaryHeap<Reverse<Timer>>,
    /// Thread → token of its live job.
    live: HashMap<usize, u64>,
    next_token: u64,
    /// Set by a yielding API call (`task.wait`, `Signal:Wait`) to the thread
    /// that parked itself, so the scheduler doesn't treat it as finished.
    pub parked: Option<usize>,
}

impl Sched {
    fn job(&mut self, thread: Thread, resume: Resume) -> Job {
        self.next_token += 1;
        self.live.insert(thread_key(&thread), self.next_token);
        Job {
            thread,
            resume,
            token: self.next_token,
        }
    }

    /// Runs `thread` later this step (or next, if the slice runs out).
    pub fn push(&mut self, thread: Thread, resume: Resume) {
        let job = self.job(thread, resume);
        self.ready.push_back(job);
    }

    /// Like `push`, for a thread parked on a signal: skipped if it was
    /// cancelled or rescheduled since.
    fn wake(&mut self, thread: Thread, resume: Resume) {
        if self.live.contains_key(&thread_key(&thread)) {
            self.push(thread, resume);
        }
    }

    /// Marks a thread as parked on a signal, so `wake` accepts it.
    pub fn park(&mut self, thread: &Thread) {
        self.next_token += 1;
        self.live.insert(thread_key(thread), self.next_token);
        self.parked = Some(thread_key(thread));
    }

    pub fn push_front(&mut self, jobs: Vec<Job>) {
        for job in jobs.into_iter().rev() {
            self.ready.push_front(job);
        }
    }

    pub fn sleep(&mut self, thread: Thread, at: u64, resume: Resume) {
        let job = self.job(thread, resume);
        self.next_token += 1;
        self.timers.push(Reverse(Timer {
            at,
            seq: self.next_token,
            job,
        }));
    }

    /// Requeues a thread the budget interrupted, keeping its place.
    pub fn requeue(&mut self, thread: Thread) -> Job {
        self.job(thread, Resume::Args(MultiValue::new()))
    }

    pub fn cancel(&mut self, thread: &Thread) {
        self.live.remove(&thread_key(thread));
    }

    /// Moves timers due by `tick` to the ready queue.
    pub fn wake_timers(&mut self, tick: u64) {
        while self.timers.peek().is_some_and(|t| t.0.at <= tick) {
            let Reverse(timer) = self.timers.pop().expect("peeked");
            self.ready.push_back(timer.job);
        }
    }

    /// The next valid job, consuming its liveness.
    pub fn pop(&mut self) -> Option<Job> {
        while let Some(job) = self.ready.pop_front() {
            let key = thread_key(&job.thread);
            if self.live.get(&key) == Some(&job.token) {
                self.live.remove(&key);
                return Some(job);
            }
        }
        None
    }

    pub fn has_ready(&self) -> bool {
        self.ready.iter().any(|j| self.live.get(&thread_key(&j.thread)) == Some(&j.token))
    }

    pub fn next_timer(&mut self) -> Option<u64> {
        while let Some(Reverse(t)) = self.timers.peek() {
            if self.live.get(&thread_key(&t.job.thread)) == Some(&t.job.token) {
                return Some(t.at);
            }
            self.timers.pop();
        }
        None
    }
}
