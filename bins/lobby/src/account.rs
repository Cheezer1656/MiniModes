//! Per-player database state: rank and owned games, loaded on join, and game
//! creation. Queries run on tokio; results come back through a channel.

use std::collections::{HashMap, HashSet};

use chunkedge::UniqueId;
use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_db::{NewGame, Pool};
use mm_proto::ipc::{LobbyMsg, RankQuota};
use mm_proto::{GameId, GameKind, GameSummary};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::catalog::PlayerGames;

pub const DEFAULT_RANK: &str = "member";

/// What each rank may build, and staff powers.
#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(deny_unknown_fields)]
pub struct RankLimits {
    /// Build area side length: 128, 256 or 512.
    pub map_size: i32,
    pub max_drafts: i64,
    /// Luau heap per instance of the player's games.
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,
    /// What each of the player's games may keep in DataStores.
    #[serde(default = "default_datastore_mb")]
    pub datastore_mb: u32,
    /// May use `/mod`, file holds and take games down, and ban creators.
    #[serde(default)]
    pub moderator: bool,
    /// May grant ranks.
    #[serde(default)]
    pub admin: bool,
}

fn default_memory_mb() -> u32 {
    64
}

fn default_datastore_mb() -> u32 {
    16
}

impl RankLimits {
    pub fn quota(&self) -> RankQuota {
        RankQuota {
            memory_mb: self.memory_mb,
            datastore_mb: self.datastore_mb,
        }
    }
}

#[derive(Resource, Clone)]
pub struct Ranks(pub HashMap<String, RankLimits>);

impl Ranks {
    pub fn limits(&self, rank: Option<&str>) -> RankLimits {
        rank.and_then(|r| self.0.get(r))
            .or_else(|| self.0.get(DEFAULT_RANK))
            .copied()
            .unwrap_or(RankLimits {
                map_size: 128,
                max_drafts: 1,
                memory_mb: default_memory_mb(),
                datastore_mb: default_datastore_mb(),
                moderator: false,
                admin: false,
            })
    }

    /// For shards: quotas by rank.
    pub fn quotas_msg(&self) -> LobbyMsg {
        LobbyMsg::RankQuotas {
            default: self.limits(None).quota(),
            ranks: self.0.iter().map(|(name, l)| (name.clone(), l.quota())).collect(),
        }
    }
}

pub fn default_ranks() -> HashMap<String, RankLimits> {
    let rank = |map_size, max_drafts, memory_mb, datastore_mb, moderator, admin| RankLimits {
        map_size,
        max_drafts,
        memory_mb,
        datastore_mb,
        moderator,
        admin,
    };
    HashMap::from([
        (DEFAULT_RANK.to_owned(), rank(128, 3, 64, 16, false, false)),
        ("plus".to_owned(), rank(256, 6, 96, 32, false, false)),
        ("creator".to_owned(), rank(512, 12, 128, 64, false, false)),
        ("moderator".to_owned(), rank(256, 6, 96, 32, true, false)),
        ("admin".to_owned(), rank(512, 12, 128, 64, true, true)),
    ])
}

#[derive(Resource, Clone)]
pub struct Db(pub Pool);

/// Reload a player's rank and games (e.g. they were invited to collaborate).
#[derive(Message, Clone, Copy)]
pub struct RefreshAccount(pub Entity);

/// A player finished the create flow.
#[derive(Message, Clone)]
pub struct CreateGameRequest {
    pub client: Entity,
    pub name: String,
    pub icon: String,
    pub kind: GameKind,
}

enum DbEvent {
    Loaded {
        client: Entity,
        rank: Option<String>,
        creator_ban: Option<mm_db::moderation::Ban>,
        owned: Vec<GameSummary>,
        shared: Vec<GameSummary>,
        player_saves: HashSet<GameId>,
        library: mm_db::library::Library,
    },
    Created {
        client: Entity,
        game: Option<GameSummary>,
    },
    Failed {
        client: Entity,
        what: &'static str,
    },
}

#[derive(Resource)]
struct DbEvents {
    tx: UnboundedSender<DbEvent>,
    rx: UnboundedReceiver<DbEvent>,
}

pub struct AccountPlugin;

impl Plugin for AccountPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(DbEvents { tx, rx })
            .add_message::<CreateGameRequest>()
            .add_message::<RefreshAccount>()
            .add_systems(Update, (load_accounts, create_games, apply_db_events));
    }
}

fn load_accounts(
    joined: Query<(Entity, &UniqueId, &Username), Added<Client>>,
    clients: Query<(Entity, &UniqueId, &Username), With<Client>>,
    mut refreshes: MessageReader<RefreshAccount>,
    db: Res<Db>,
    events: Res<DbEvents>,
    tokio: Res<Tokio>,
) {
    let refreshed = refreshes.read().filter_map(|r| clients.get(r.0).ok());
    for (client, uuid, username) in joined.iter().chain(refreshed) {
        let (pool, tx, uuid, name) = (db.0.clone(), events.tx.clone(), uuid.0, username.0.clone());
        tokio.0.spawn(async move {
            let result = async {
                let rank = mm_db::player_joined(&pool, uuid, &name).await?;
                let owned = mm_db::owned_games(&pool, uuid).await?;
                let shared = mm_db::editor::shared_games(&pool, uuid).await?;
                let library = mm_db::library::library(&pool, uuid).await?;
                let ban = mm_db::moderation::creator_ban(&pool, uuid).await?;
                Ok::<_, sqlx::Error>((rank, ban, owned, shared, library))
            }
            .await;
            let _ = tx.send(match result {
                Ok((rank, creator_ban, owned, shared, library)) => DbEvent::Loaded {
                    library,
                    client,
                    rank,
                    creator_ban,
                    player_saves: owned.iter().filter(|g| g.save_player_state).map(|g| GameId(g.id)).collect(),
                    owned: owned.iter().map(|g| g.summary()).collect(),
                    shared: shared.iter().map(|g| g.summary()).collect(),
                },
                Err(e) => {
                    tracing::error!("loading account {uuid}: {e}");
                    DbEvent::Failed {
                        client,
                        what: "load your games",
                    }
                }
            });
        });
    }
}

fn create_games(
    mut requests: MessageReader<CreateGameRequest>,
    mut players: Query<(&mut Client, &UniqueId, &PlayerGames)>,
    db: Res<Db>,
    ranks: Res<Ranks>,
    events: Res<DbEvents>,
    tokio: Res<Tokio>,
) {
    for request in requests.read() {
        let Ok((mut client, uuid, games)) = players.get_mut(request.client) else {
            continue;
        };
        if !games.loaded {
            client.send_chat_message("§cStill loading your account; try again in a moment.");
            continue;
        }
        if let Some(ban) = &games.creator_ban {
            client.send_chat_message(crate::moderation::ban_message(ban));
            continue;
        }
        let limits = ranks.limits(games.rank.as_deref());
        let max_players = match request.kind {
            GameKind::Match => 16,
            GameKind::OpenWorld { .. } => 50,
        };
        let (pool, tx, owner, req) = (db.0.clone(), events.tx.clone(), uuid.0, request.clone());
        tokio.0.spawn(async move {
            let game = NewGame {
                owner,
                name: &req.name,
                icon: &req.icon,
                kind: req.kind,
                max_players,
                map_size: limits.map_size,
            };
            let _ = tx.send(match mm_db::create_game(&pool, game, limits.max_drafts).await {
                Ok(game) => DbEvent::Created {
                    client: req.client,
                    game: game.map(|g| g.summary()),
                },
                Err(e) => {
                    tracing::error!("creating game for {owner}: {e}");
                    DbEvent::Failed {
                        client: req.client,
                        what: "create your game",
                    }
                }
            });
        });
    }
}

fn apply_db_events(mut events: ResMut<DbEvents>, mut players: Query<(&mut Client, &mut PlayerGames)>, ranks: Res<Ranks>) {
    while let Ok(event) = events.rx.try_recv() {
        match event {
            DbEvent::Loaded {
                client,
                rank,
                creator_ban,
                owned,
                shared,
                player_saves,
                library,
            } => {
                if let Ok((_, mut games)) = players.get_mut(client) {
                    // A refresh keeps what changed since the first load.
                    if !games.loaded {
                        games.set_library(library);
                    }
                    games.rank = rank;
                    games.creator_ban = creator_ban;
                    games.player_saves = player_saves;
                    games.owned = owned;
                    games.shared = shared;
                    games.loaded = true;
                }
            }
            DbEvent::Created { client, game } => {
                let Ok((mut c, mut games)) = players.get_mut(client) else {
                    continue;
                };
                match game {
                    Some(game) => {
                        c.send_chat_message(format!(
                            "§aCreated §f{}§a. Open §bMy Games§a and click it to start building.",
                            game.name
                        ));
                        games.player_saves.insert(game.id);
                        games.owned.insert(0, game);
                    }
                    None => {
                        let limit = ranks.limits(games.rank.as_deref()).max_drafts;
                        c.send_chat_message(format!("§cYou already have {limit} games, the most your rank allows."));
                    }
                }
            }
            DbEvent::Failed { client, what } => {
                if let Ok((mut c, _)) = players.get_mut(client) {
                    c.send_chat_message(format!("§cCouldn't {what} right now. Try again later."));
                }
            }
        }
    }
}
