//! Publishing a draft from My Games. If the draft is open on a shard, that
//! shard saves its map first (`SaveDraft` → `DraftSaved`); then the version
//! is recorded in Postgres, the game is listed, and every shard hears about
//! it (`ApplyPublish`) so a running persistent world can update.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chunkedge::UniqueId;
use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_db::publish::PublishOutcome;
use mm_proto::ipc::LobbyMsg;
use mm_proto::{GameId, GameKind, GameSummary, LiveUpdate};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::account::Db;
use crate::catalog::{GameCatalog, PlayerGames};
use crate::coordinator::Coordinator;

/// How long the shard running the draft has to save it.
const SAVE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Message, Clone, Copy)]
pub struct PublishRequest {
    pub client: Entity,
    pub game: GameId,
    pub mode: LiveUpdate,
}

#[derive(Message, Clone, Copy)]
pub struct PlayerSavesSetting {
    pub game: GameId,
    pub on: bool,
}

struct Waiting {
    client: Entity,
    owner: Uuid,
    game: GameId,
    mode: LiveUpdate,
    deadline: Instant,
}

enum Done {
    Published { client: Entity, game: GameSummary, mode: LiveUpdate },
    Refused { client: Entity, message: &'static str },
}

#[derive(Resource)]
struct Publishes {
    next_request: u64,
    /// Waiting for a shard's `DraftSaved`, by request id.
    saving: HashMap<u64, Waiting>,
    tx: UnboundedSender<Done>,
    rx: UnboundedReceiver<Done>,
}

pub struct PublishPlugin;

impl Plugin for PublishPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(Publishes {
            next_request: 1,
            saving: HashMap::new(),
            tx,
            rx,
        })
        .add_message::<PublishRequest>()
        .add_message::<PlayerSavesSetting>()
        .add_systems(Update, (start_publishes, finish_saves, apply_results, save_settings).chain());
    }
}

fn start_publishes(
    mut requests: MessageReader<PublishRequest>,
    mut players: Query<(&mut Client, &UniqueId, &PlayerGames)>,
    coordinator: Res<Coordinator>,
    mut publishes: ResMut<Publishes>,
    db: Res<Db>,
    tokio: Res<Tokio>,
) {
    for request in requests.read() {
        let Ok((mut client, uuid, games)) = players.get_mut(request.client) else {
            continue;
        };
        let Some(game) = games.owned(request.game) else {
            client.send_chat_message("§cOnly the game's owner can publish it.");
            continue;
        };
        if publishes.saving.values().any(|w| w.game == request.game) {
            client.send_chat_message("§eAlready publishing; one moment...");
            continue;
        }
        client.send_chat_message(format!("§7Publishing §f{}§7 v{}...", game.name, game.version + 1));
        let waiting = Waiting {
            client: request.client,
            owner: uuid.0,
            game: request.game,
            mode: request.mode,
            deadline: Instant::now() + SAVE_TIMEOUT,
        };
        let request_id = publishes.next_request;
        if coordinator.save_draft(request.game, request_id) {
            publishes.next_request += 1;
            publishes.saving.insert(request_id, waiting);
        } else {
            run_publish(&db, &tokio, &publishes.tx, waiting);
        }
    }
}

fn run_publish(db: &Db, tokio: &Tokio, tx: &UnboundedSender<Done>, w: Waiting) {
    let (pool, tx) = (db.0.clone(), tx.clone());
    tokio.0.spawn(async move {
        let done = match mm_db::publish::publish(&pool, w.game.0, w.owner, w.mode).await {
            Ok(PublishOutcome::Published(row)) => Done::Published {
                client: w.client,
                game: row.summary(),
                mode: w.mode,
            },
            Ok(PublishOutcome::NotOwner) => Done::Refused {
                client: w.client,
                message: "§cOnly the game's owner can publish it.",
            },
            Ok(PublishOutcome::NoGame) => Done::Refused {
                client: w.client,
                message: "§cThat game no longer exists.",
            },
            Err(e) => {
                tracing::error!("publishing game {}: {e}", w.game.0);
                Done::Refused {
                    client: w.client,
                    message: "§cCouldn't publish right now. Try again later.",
                }
            }
        };
        let _ = tx.send(done);
    });
}

fn finish_saves(
    mut coordinator: ResMut<Coordinator>,
    mut publishes: ResMut<Publishes>,
    mut clients: Query<&mut Client>,
    db: Res<Db>,
    tokio: Res<Tokio>,
) {
    for (request, ok) in coordinator.take_draft_saves() {
        let Some(waiting) = publishes.saving.remove(&request) else {
            continue;
        };
        if ok {
            run_publish(&db, &tokio, &publishes.tx, waiting);
        } else if let Ok(mut client) = clients.get_mut(waiting.client) {
            client.send_chat_message("§cThe draft couldn't be saved, so it wasn't published. Try again.");
        }
    }
    let now = Instant::now();
    let expired: Vec<u64> = publishes
        .saving
        .iter()
        .filter(|(_, w)| now >= w.deadline)
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        if let Some(w) = publishes.saving.remove(&id)
            && let Ok(mut client) = clients.get_mut(w.client)
        {
            client.send_chat_message("§cThe server running your draft didn't answer. Try publishing again.");
        }
    }
}

fn apply_results(
    mut publishes: ResMut<Publishes>,
    mut players: Query<(&mut Client, &mut PlayerGames)>,
    mut catalog: ResMut<GameCatalog>,
    coordinator: Res<Coordinator>,
) {
    while let Ok(done) = publishes.rx.try_recv() {
        match done {
            Done::Refused { client, message } => {
                if let Ok((mut c, _)) = players.get_mut(client) {
                    c.send_chat_message(message);
                }
            }
            Done::Published { client, game, mode } => {
                tracing::info!("game {} published as v{}", game.id.0, game.version);
                coordinator.broadcast(LobbyMsg::ApplyPublish {
                    game: game.id,
                    version: game.version,
                });
                catalog.upsert(game.clone());
                let Ok((mut c, mut games)) = players.get_mut(client) else {
                    continue;
                };
                if let Some(owned) = games.owned.iter_mut().find(|g| g.id == game.id) {
                    *owned = game.clone();
                }
                c.send_chat_message(format!(
                    "§aPublished §f{}§a v{}! It's listed in the Game Browser.",
                    game.name, game.version
                ));
                let persistent = game.kind
                    == GameKind::OpenWorld {
                        persistent_world: true,
                    };
                if persistent && game.version > 1 {
                    c.send_chat_message(match mode {
                        LiveUpdate::Scripts => "§7The live world keeps its map and runs the new scripts.",
                        LiveUpdate::Regions => "§7The live world gets the new scripts and your marked regions.",
                        LiveUpdate::Reset => "§7The live world was backed up and resets to the new map.",
                    });
                }
            }
        }
    }
}

fn save_settings(
    mut requests: MessageReader<PlayerSavesSetting>,
    db: Res<Db>,
    tokio: Res<Tokio>,
) {
    for request in requests.read() {
        let (pool, game, on) = (db.0.clone(), request.game, request.on);
        tokio.0.spawn(async move {
            if let Err(e) = mm_db::set_game_setting(&pool, game.0, "save_player_state", serde_json::Value::Bool(on)).await {
                tracing::error!("saving settings of game {}: {e}", game.0);
            }
        });
    }
}
