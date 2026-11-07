//! The shard's connection to the lobby coordinator and its Gate registration.
//! Both run on tokio and talk to the ECS through channels.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chunkedge::prelude::*;
use mm_gate::GateClient;
use mm_proto::ipc::{self, InstanceStatus, LobbyMsg, ShardId, ShardMsg};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Gate forgets dynamically registered servers when it restarts, so the
/// registration is re-asserted on this interval.
const REREGISTER_EVERY: Duration = Duration::from_secs(15);

/// ECS end of the lobby link.
#[derive(Resource)]
pub struct LobbyLink {
    pub inbox: UnboundedReceiver<LobbyMsg>,
    outbox: UnboundedSender<ShardMsg>,
    /// Latest instance list, sent in `Hello` when the link (re)connects.
    pub snapshot: Arc<Mutex<Vec<InstanceStatus>>>,
}

impl LobbyLink {
    /// Queues a message. Messages queued while disconnected are dropped.
    pub fn send(&self, msg: ShardMsg) {
        let _ = self.outbox.send(msg);
    }
}

#[derive(Resource, Clone)]
pub struct GateLink {
    pub client: GateClient,
    pub lobby_server: String,
}

/// Sent by the signal handler; the ECS answers with every connected player
/// and the final saves it started.
#[derive(Resource)]
pub struct ShutdownRequests(pub UnboundedReceiver<oneshot::Sender<ShutdownReply>>);

pub struct ShutdownReply {
    pub players: Vec<Uuid>,
    pub saves: Vec<tokio::task::JoinHandle<()>>,
}

pub fn connect_lobby(runtime: &tokio::runtime::Handle, addr: SocketAddr, shard: ShardId, gate_name: String) -> LobbyLink {
    let (in_tx, in_rx) = mpsc::unbounded_channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let snapshot = Arc::new(Mutex::new(Vec::new()));
    runtime.spawn(run_link(addr, shard, gate_name, snapshot.clone(), out_rx, in_tx));
    LobbyLink {
        inbox: in_rx,
        outbox: out_tx,
        snapshot,
    }
}

async fn run_link(
    addr: SocketAddr,
    shard: ShardId,
    gate_name: String,
    snapshot: Arc<Mutex<Vec<InstanceStatus>>>,
    mut outbox: UnboundedReceiver<ShardMsg>,
    inbox: UnboundedSender<LobbyMsg>,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        let stream = match TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!("lobby coordinator at {addr} unreachable: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };
        backoff = Duration::from_millis(250);
        let _ = stream.set_nodelay(true);
        let (mut reader, mut writer) = stream.into_split();

        // Anything queued while offline (acks for tickets the lobby has
        // already given up on, stale heartbeats) is obsolete.
        while outbox.try_recv().is_ok() {}
        let hello = ShardMsg::Hello {
            shard,
            gate_name: gate_name.clone(),
            instances: snapshot.lock().unwrap().clone(),
        };
        if let Err(e) = ipc::write_frame(&mut writer, &hello).await {
            tracing::warn!("sending Hello: {e}");
            continue;
        }
        tracing::info!("connected to lobby coordinator at {addr}");

        let inbox = inbox.clone();
        let mut read_task = tokio::spawn(async move {
            loop {
                match ipc::read_frame::<_, LobbyMsg>(&mut reader).await {
                    Ok(Some(msg)) => {
                        if inbox.send(msg).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        tracing::warn!("reading from lobby: {e}");
                        return;
                    }
                }
            }
        });

        loop {
            tokio::select! {
                msg = outbox.recv() => {
                    let Some(msg) = msg else {
                        read_task.abort();
                        return;
                    };
                    if let Err(e) = ipc::write_frame(&mut writer, &msg).await {
                        tracing::warn!("writing to lobby: {e}");
                        break;
                    }
                }
                _ = &mut read_task => break,
            }
        }
        read_task.abort();
        tracing::warn!("lost connection to lobby coordinator; reconnecting");
    }
}

/// Registers with Gate (retrying until it's up) and keeps the registration alive.
pub async fn keep_registered(gate: GateClient, name: String, address: String) {
    let mut backoff = Duration::from_millis(250);
    loop {
        match gate.register_or_replace(&name, &address).await {
            Ok(()) => break,
            Err(e) => {
                tracing::warn!("registering {name} with Gate: {e}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
    tracing::info!("registered with Gate as {name} → {address}");
    loop {
        tokio::time::sleep(REREGISTER_EVERY).await;
        match gate.register_server(&name, &address).await {
            Ok(()) => tracing::warn!("Gate had forgotten {name}; registered again"),
            Err(e) if e.code() == Some("already_exists") => {}
            Err(e) => tracing::warn!("re-registering {name} with Gate: {e}"),
        }
    }
}

/// On SIGINT/SIGTERM: send every player to the lobby, unregister from Gate, exit.
pub async fn shutdown_on_signal(
    gate: GateLink,
    name: String,
    requests: UnboundedSender<oneshot::Sender<ShutdownReply>>,
) {
    wait_for_signal().await;
    tracing::info!("shutting down: saving drafts and returning players to the lobby");

    let (tx, rx) = oneshot::channel();
    let reply = match requests.send(tx) {
        Ok(()) => tokio::time::timeout(Duration::from_secs(2), rx).await.ok().and_then(Result::ok),
        Err(_) => None,
    };
    let ShutdownReply { players, saves } = reply.unwrap_or(ShutdownReply {
        players: Vec::new(),
        saves: Vec::new(),
    });
    let mut moves = tokio::task::JoinSet::new();
    for uuid in players {
        let gate = gate.clone();
        moves.spawn(async move {
            gate.client
                .connect_player(&uuid.hyphenated().to_string(), &gate.lobby_server)
                .await
        });
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), moves.join_all()).await;
    let pending = saves.len();
    if tokio::time::timeout(Duration::from_secs(10), async {
        for save in saves {
            let _ = save.await;
        }
    })
    .await
    .is_err()
    {
        tracing::error!("gave up waiting for {pending} draft save(s)");
    }

    if let Err(e) = gate.client.unregister_server(&name).await {
        tracing::warn!("unregistering {name} from Gate: {e}");
    }
    std::process::exit(0);
}

async fn wait_for_signal() {
    // A failed registration (e.g. SIGINT ignored in a background job) must
    // not count as a signal.
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::warn!("can't listen for Ctrl-C: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
