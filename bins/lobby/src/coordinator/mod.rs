//! The lobby side of shard IPC: accepts shard connections, tracks their
//! instances, and hands players off to them (`ExpectPlayer` → ack → Gate
//! `ConnectPlayer`).

mod directory;

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chunkedge::prelude::*;
use chunkedge::UniqueId;
use mm_core::Tokio;
use mm_gate::{GateClient, GateError};
use mm_proto::ipc::{
    self, DiagnosticLevel, GameVersion, InstanceId, InstanceSpec, LobbyMsg, ShardId, ShardMsg,
    Ticket,
};
use mm_proto::GameId;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

pub use directory::Placement;
use directory::{ConnId, Directory, Instance, Shard};

use crate::catalog::{GameCatalog, PlayerGames};

/// How long a handoff may take from `ExpectPlayer` to the player leaving.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// The owner plus collaborators.
const DRAFT_MAX_PLAYERS: u32 = 8;

#[derive(Resource, Clone)]
pub struct CoordinatorConfig {
    pub bind: SocketAddr,
    pub gate: GateClient,
}

enum NetEvent {
    Connected {
        conn: ConnId,
        shard: ShardId,
        gate_name: String,
        instances: Vec<ipc::InstanceStatus>,
        tx: UnboundedSender<LobbyMsg>,
    },
    Message {
        conn: ConnId,
        shard: ShardId,
        msg: ShardMsg,
    },
    Disconnected {
        conn: ConnId,
        shard: ShardId,
    },
}

struct PendingJoin {
    client: Entity,
    uuid: Uuid,
    game: GameId,
    /// For one automatic retry if the shard refuses (e.g. the match just
    /// started, before its heartbeat said so).
    retry: Option<(mm_proto::GameSummary, GameVersion)>,
    /// Building in the draft rather than playing.
    draft: bool,
    shard: ShardId,
    instance: InstanceId,
    deadline: Instant,
    /// Set once the shard accepted and the Gate call is in flight.
    connecting: bool,
}

#[derive(Resource)]
pub struct Coordinator {
    pub directory: Directory,
    net: UnboundedReceiver<NetEvent>,
    gate: GateClient,
    gate_results: (
        UnboundedSender<(Ticket, Result<(), GateError>)>,
        UnboundedReceiver<(Ticket, Result<(), GateError>)>,
    ),
    pending: HashMap<Ticket, PendingJoin>,
    /// Shard that last hosted each draft, so a reopened draft loads where
    /// its final save happened.
    draft_shards: HashMap<GameId, ShardId>,
    /// Players waiting for room in a full (or restarting) persistent world.
    queues: HashMap<GameId, VecDeque<Queued>>,
    /// `DraftSaved` answers for the publish flow.
    draft_saves: Vec<(u64, bool)>,
    next_ticket: u64,
    next_instance: u64,
}

struct Queued {
    client: Entity,
    uuid: Uuid,
}

/// How a join request went.
pub enum Joining {
    /// The handoff started.
    Sent,
    /// The world is full; the player is this far back in its queue.
    Queued(usize),
}

pub struct CoordinatorPlugin;

impl Plugin for CoordinatorPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, start)
            .add_systems(Update, (pump_network, finish_handoffs, expire_handoffs, serve_queues).chain());
    }
}

fn start(mut commands: Commands, config: Res<CoordinatorConfig>, tokio: Res<Tokio>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let bind = config.bind;
    tokio.0.spawn(async move {
        if let Err(e) = listen(bind, tx).await {
            tracing::error!("coordinator listener failed: {e:#}");
            std::process::exit(1);
        }
    });

    // Microseconds since the epoch: unique across lobby restarts, so
    // instances shards still run from a previous lobby never collide.
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    commands.insert_resource(Coordinator {
        directory: Directory::default(),
        net: rx,
        gate: config.gate.clone(),
        gate_results: mpsc::unbounded_channel(),
        pending: HashMap::new(),
        draft_shards: HashMap::new(),
        queues: HashMap::new(),
        draft_saves: Vec::new(),
        next_ticket: 1,
        next_instance: now.as_micros() as u64,
    });
}

async fn listen(bind: SocketAddr, events: UnboundedSender<NetEvent>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!("coordinator listening on {bind}");
    let mut next_conn: ConnId = 0;
    loop {
        let (stream, peer) = listener.accept().await?;
        next_conn += 1;
        let conn = next_conn;
        let events = events.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_shard(stream, conn, events).await {
                tracing::warn!("shard connection from {peer}: {e:#}");
            }
        });
    }
}

async fn serve_shard(stream: TcpStream, conn: ConnId, events: UnboundedSender<NetEvent>) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();

    let hello = tokio::time::timeout(HELLO_TIMEOUT, ipc::read_frame::<_, ShardMsg>(&mut reader)).await??;
    let Some(ShardMsg::Hello {
        shard,
        gate_name,
        instances,
    }) = hello
    else {
        anyhow::bail!("expected Hello");
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = events.send(NetEvent::Connected {
        conn,
        shard,
        gate_name,
        instances,
        tx,
    });

    // Ends when the coordinator drops `tx` (replaced or removed shard); dropping
    // the write half then closes the connection from our side.
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = ipc::write_frame(&mut writer, &msg).await {
                tracing::warn!("writing to shard {}: {e}", shard.0);
                break;
            }
        }
    });

    let result = async {
        while let Some(msg) = ipc::read_frame(&mut reader).await? {
            let _ = events.send(NetEvent::Message { conn, shard, msg });
        }
        anyhow::Ok(())
    }
    .await;
    let _ = events.send(NetEvent::Disconnected { conn, shard });
    result
}

impl Coordinator {
    /// Finds or creates an instance for `game` and asks its shard to expect
    /// the player, or queues them for a full persistent world. Returns the
    /// user-facing error on failure.
    pub fn begin_join(
        &mut self,
        client: Entity,
        uuid: Uuid,
        game: &mm_proto::GameSummary,
        version: GameVersion,
    ) -> Result<Joining, &'static str> {
        if self.pending.values().any(|p| p.client == client) {
            return Err("§eYou're already being sent to a game...");
        }
        if let Some(pos) = self.queue_position(client, game.id) {
            return Ok(Joining::Queued(pos));
        }
        self.leave_queues(client);

        let preferred = (version == GameVersion::Draft)
            .then(|| self.draft_shards.get(&game.id).copied())
            .flatten();
        let (shard_id, instance) = match self.directory.place(game.id, version, game.kind, preferred) {
            Placement::Existing(shard, instance) => (shard, instance),
            Placement::New(shard) => {
                self.next_instance += 1;
                let spec = InstanceSpec {
                    id: InstanceId(self.next_instance),
                    game: game.id,
                    version,
                    kind: game.kind,
                    max_players: if version == GameVersion::Draft {
                        DRAFT_MAX_PLAYERS
                    } else {
                        game.max_players
                    },
                };
                if version == GameVersion::Draft {
                    self.draft_shards.insert(game.id, shard);
                }
                let entry = self.directory.shards.get_mut(&shard).expect("placed on a live shard");
                tracing::info!("creating instance {} of game {} on shard {}", spec.id.0, game.id.0, shard.0);
                let _ = entry.tx.send(LobbyMsg::CreateInstance(spec.clone()));
                entry.instances.insert(
                    spec.id,
                    Instance {
                        status: ipc::InstanceStatus {
                            spec,
                            players: 0,
                            expected: 0,
                            started: false,
                        },
                        reserved: 0,
                    },
                );
                (shard, InstanceId(self.next_instance))
            }
            Placement::Full | Placement::Closing if version != GameVersion::Draft => {
                let queue = self.queues.entry(game.id).or_default();
                queue.push_back(Queued { client, uuid });
                return Ok(Joining::Queued(queue.len()));
            }
            Placement::Full => return Err("§cThat draft is full."),
            Placement::Closing => return Err("§eThe draft is still saving from last time. Try again in a moment."),
            Placement::NoShards => return Err("§cNo game servers are online right now. Try again soon."),
        };

        let ticket = Ticket(self.next_ticket);
        self.next_ticket += 1;
        self.directory
            .instance_mut(shard_id, instance)
            .expect("placed in a known instance")
            .reserved += 1;
        let _ = self.directory.shards[&shard_id].tx.send(LobbyMsg::ExpectPlayer { uuid, instance, ticket });
        self.pending.insert(
            ticket,
            PendingJoin {
                client,
                uuid,
                game: game.id,
                retry: Some((game.clone(), version)),
                draft: version == GameVersion::Draft,
                shard: shard_id,
                instance,
                deadline: Instant::now() + HANDOFF_TIMEOUT,
                connecting: false,
            },
        );
        Ok(Joining::Sent)
    }

    /// 1-based place in `game`'s queue.
    fn queue_position(&self, client: Entity, game: GameId) -> Option<usize> {
        self.queues.get(&game)?.iter().position(|q| q.client == client).map(|i| i + 1)
    }

    /// Removes the player from every queue; returns whether they were in one.
    pub fn leave_queues(&mut self, client: Entity) -> bool {
        let mut left = false;
        for queue in self.queues.values_mut() {
            let before = queue.len();
            queue.retain(|q| q.client != client);
            left |= queue.len() != before;
        }
        self.queues.retain(|_, q| !q.is_empty());
        left
    }

    /// Asks the shard running the game's draft to save it now. Returns
    /// false when no draft is running (Postgres already has the latest map).
    pub fn save_draft(&self, game: GameId, request: u64) -> bool {
        for shard in self.directory.shards.values() {
            if shard
                .instances
                .values()
                .any(|i| i.status.spec.game == game && i.status.spec.version == GameVersion::Draft)
            {
                let _ = shard.tx.send(LobbyMsg::SaveDraft { game, request });
                return true;
            }
        }
        false
    }

    pub fn take_draft_saves(&mut self) -> Vec<(u64, bool)> {
        std::mem::take(&mut self.draft_saves)
    }

    pub fn broadcast(&self, msg: LobbyMsg) {
        for shard in self.directory.shards.values() {
            let _ = shard.tx.send(msg.clone());
        }
    }

    /// Restarts the scripts of the game's running draft, if any. Returns
    /// whether one was running.
    pub fn reload_draft(&self, game: GameId, by: &str) -> bool {
        let mut running = false;
        for shard in self.directory.shards.values() {
            if shard
                .instances
                .values()
                .any(|i| i.status.spec.game == game && i.status.spec.version == GameVersion::Draft)
            {
                running = true;
                let _ = shard.tx.send(LobbyMsg::ReloadDraft {
                    game,
                    by: by.to_owned(),
                });
            }
        }
        running
    }

    fn release(&mut self, ticket: Ticket) -> Option<PendingJoin> {
        let pending = self.pending.remove(&ticket)?;
        if !pending.connecting
            && let Some(instance) = self.directory.instance_mut(pending.shard, pending.instance)
        {
            instance.reserved = instance.reserved.saturating_sub(1);
        }
        Some(pending)
    }

    /// Fails every handoff to `shard` (it disconnected or was replaced).
    fn fail_shard(&mut self, shard: ShardId) -> Vec<Entity> {
        let tickets: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.shard == shard)
            .map(|(t, _)| *t)
            .collect();
        tickets
            .into_iter()
            .filter_map(|t| self.pending.remove(&t))
            .map(|p| p.client)
            .collect()
    }
}

type LobbyClients<'w, 's> = Query<'w, 's, (&'static mut Client, &'static UniqueId, &'static mut PlayerGames)>;

fn notify(clients: &mut LobbyClients, client: Entity, msg: &str) {
    if let Ok((mut c, ..)) = clients.get_mut(client) {
        c.send_chat_message(msg.to_owned());
    }
}

#[allow(clippy::too_many_arguments)]
fn pump_network(
    mut coordinator: ResMut<Coordinator>,
    mut catalog: ResMut<GameCatalog>,
    mut clients: LobbyClients,
    tokio: Res<Tokio>,
    console: Res<crate::web::Console>,
    mut played: MessageWriter<crate::library::Played>,
    mut reports: MessageWriter<crate::moderation::FileReport>,
    ranks: Res<crate::account::Ranks>,
) {
    let coordinator = &mut *coordinator;
    let mut changed = false;

    while let Ok(event) = coordinator.net.try_recv() {
        changed = true;
        match event {
            NetEvent::Connected {
                conn,
                shard,
                gate_name,
                instances,
                tx,
            } => {
                tracing::info!("shard {} ({gate_name}) connected with {} instance(s)", shard.0, instances.len());
                let _ = tx.send(ranks.quotas_msg());
                let replaced = coordinator.directory.connect(
                    shard,
                    Shard {
                        conn,
                        gate_name,
                        tx,
                        instances: HashMap::new(),
                        tick_p99_ms: 0.0,
                    },
                );
                if replaced.is_some() {
                    tracing::warn!("shard {} reconnected; dropping its old connection", shard.0);
                    for client in coordinator.fail_shard(shard) {
                        notify(&mut clients, client, "§cThe game server restarted. Try again.");
                    }
                }
                coordinator.directory.update(shard, instances, 0.0);
            }
            NetEvent::Disconnected { conn, shard } => {
                if coordinator.directory.disconnect(shard, conn).is_some() {
                    tracing::warn!("shard {} disconnected", shard.0);
                    for client in coordinator.fail_shard(shard) {
                        notify(&mut clients, client, "§cThe game server went offline. Try again soon.");
                    }
                }
            }
            NetEvent::Message { conn, shard, msg } => {
                if coordinator.directory.shards.get(&shard).is_none_or(|s| s.conn != conn) {
                    continue;
                }
                match msg {
                    ShardMsg::Hello { .. } => tracing::warn!("shard {} sent a second Hello", shard.0),
                    ShardMsg::Heartbeat {
                        instances,
                        tick_p99_ms,
                    } => coordinator.directory.update(shard, instances, tick_p99_ms),
                    ShardMsg::ExpectAck { ticket, accepted } => {
                        let Some(pending) = coordinator.pending.get(&ticket) else {
                            continue;
                        };
                        if pending.shard != shard {
                            continue;
                        }
                        if !accepted {
                            let instance = pending.instance;
                            if let Some(i) = coordinator.directory.instance_mut(shard, instance) {
                                // Until the next heartbeat corrects it.
                                i.status.started = true;
                            }
                            let Some(p) = coordinator.release(ticket) else {
                                continue;
                            };
                            let retried = p.retry.and_then(|(game, version)| {
                                coordinator.begin_join(p.client, p.uuid, &game, version).ok().map(|j| (j, game))
                            });
                            match retried {
                                Some((Joining::Sent, _)) => {
                                    // The new attempt never retries again.
                                    if let Some(again) = coordinator.pending.values_mut().find(|q| q.client == p.client) {
                                        again.retry = None;
                                    }
                                }
                                Some((Joining::Queued(pos), game)) => notify(
                                    &mut clients,
                                    p.client,
                                    &format!("§e{} is full. You're §f#{pos}§e in the queue §7(/leavequeue to leave)", game.name),
                                ),
                                None => notify(&mut clients, p.client, "§cThat game is full or just ended. Try again."),
                            }
                            continue;
                        }
                        let (client, game, instance, draft) = (pending.client, pending.game, pending.instance, pending.draft);
                        let Ok((_, uuid, mut games)) = clients.get_mut(client) else {
                            coordinator.release(ticket);
                            continue;
                        };
                        // The shard now counts the player as expected.
                        if let Some(i) = coordinator.directory.instance_mut(shard, instance) {
                            i.reserved = i.reserved.saturating_sub(1);
                            i.status.expected += 1;
                        }
                        if !draft {
                            games.played(game);
                            played.write(crate::library::Played { client, uuid: uuid.0, game });
                        }
                        let pending = coordinator.pending.get_mut(&ticket).expect("checked above");
                        pending.connecting = true;
                        pending.deadline = Instant::now() + HANDOFF_TIMEOUT;

                        let gate = coordinator.gate.clone();
                        let server = coordinator.directory.shards[&shard].gate_name.clone();
                        let player = uuid.0.hyphenated().to_string();
                        let results = coordinator.gate_results.0.clone();
                        tokio.0.spawn(async move {
                            let result = gate.connect_player(&player, &server).await;
                            let _ = results.send((ticket, result));
                        });
                    }
                    ShardMsg::InstanceEnded { instance } => {
                        if let Some(s) = coordinator.directory.shards.get_mut(&shard) {
                            s.instances.remove(&instance);
                        }
                    }
                    ShardMsg::DraftSaved { request, ok } => coordinator.draft_saves.push((request, ok)),
                    ShardMsg::Report {
                        game,
                        version,
                        reporter,
                        reason,
                    } => {
                        reports.write(crate::moderation::FileReport {
                            client: None,
                            reporter,
                            game,
                            version,
                            reason,
                        });
                    }
                    ShardMsg::Diagnostic { game, level, message } => {
                        match level {
                            DiagnosticLevel::Info => {}
                            DiagnosticLevel::System => tracing::info!("game {}: {message}", game.0),
                            DiagnosticLevel::Warn => tracing::warn!("game {}: {message}", game.0),
                            DiagnosticLevel::Error => tracing::error!("game {}: {message}", game.0),
                        }
                        console.0.push(game, crate::web::Line::new(level, &message));
                    }
                }
            }
        }
    }

    if changed {
        catalog.set_live(coordinator.directory.live_stats());
    }
}

fn finish_handoffs(mut coordinator: ResMut<Coordinator>, mut clients: Query<&mut Client>) {
    while let Ok((ticket, result)) = coordinator.gate_results.1.try_recv() {
        match result {
            // Gate is switching the player; the pending entry goes away when
            // their lobby connection closes or the timeout passes.
            Ok(()) => {}
            Err(e) => {
                tracing::warn!("ConnectPlayer failed: {e}");
                if let Some(p) = coordinator.release(ticket)
                    && let Ok(mut client) = clients.get_mut(p.client)
                {
                    client.send_chat_message("§cCouldn't connect you to the game server. Try again.");
                }
            }
        }
    }
}

fn expire_handoffs(mut coordinator: ResMut<Coordinator>, mut clients: Query<&mut Client>) {
    let now = Instant::now();
    let expired: Vec<_> = coordinator
        .pending
        .iter()
        .filter(|(_, p)| now >= p.deadline || !clients.contains(p.client))
        .map(|(t, _)| *t)
        .collect();
    for ticket in expired {
        if let Some(p) = coordinator.release(ticket)
            && let Ok(mut client) = clients.get_mut(p.client)
        {
            client.send_chat_message("§cThe game server didn't respond. Try again.");
        }
    }
}

/// Moves the front of each queue in as soon as its world has room.
fn serve_queues(
    mut coordinator: ResMut<Coordinator>,
    catalog: Res<GameCatalog>,
    mut clients: Query<&mut Client>,
    server: Res<Server>,
) {
    if server.current_tick() % 10 != 0 || coordinator.queues.is_empty() {
        return;
    }
    let games: Vec<GameId> = coordinator.queues.keys().copied().collect();
    for game in games {
        if let Some(queue) = coordinator.queues.get_mut(&game) {
            queue.retain(|q| clients.contains(q.client));
        }
        let Some(summary) = catalog.get(game) else {
            if let Some(queue) = coordinator.queues.remove(&game) {
                for q in queue {
                    if let Ok(mut c) = clients.get_mut(q.client) {
                        c.send_chat_message("§cThat game is no longer available.");
                    }
                }
            }
            continue;
        };
        let version = GameVersion::Published(summary.version);
        loop {
            let has_room = matches!(
                coordinator.directory.place(game, version, summary.kind, None),
                Placement::Existing(..) | Placement::New(_)
            );
            let Some(front) = coordinator.queues.get_mut(&game).filter(|_| has_room).and_then(|q| q.pop_front()) else {
                break;
            };
            match coordinator.begin_join(front.client, front.uuid, summary, version) {
                Ok(Joining::Sent) => {
                    if let Ok(mut c) = clients.get_mut(front.client) {
                        c.send_chat_message(format!("§aA spot opened up! Sending you to §f{}§a...", summary.name));
                    }
                }
                Ok(Joining::Queued(_)) => break,
                Err(msg) => {
                    if let Ok(mut c) = clients.get_mut(front.client) {
                        c.send_chat_message(msg);
                    }
                }
            }
        }
        coordinator.queues.retain(|_, q| !q.is_empty());
    }
}
