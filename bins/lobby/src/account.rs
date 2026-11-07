//! Per-player database state: rank and owned games, loaded on join, and game
//! creation. Queries run on tokio; results come back through a channel.

use std::collections::HashMap;

use chunkedge::UniqueId;
use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_db::{NewGame, Pool};
use mm_proto::{GameKind, GameSummary};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::catalog::PlayerGames;

pub const DEFAULT_RANK: &str = "member";

/// What each rank may build.
#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(deny_unknown_fields)]
pub struct RankLimits {
    /// Build area side length: 128, 256 or 512.
    pub map_size: i32,
    pub max_drafts: i64,
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
            })
    }
}

pub fn default_ranks() -> HashMap<String, RankLimits> {
    HashMap::from([
        (DEFAULT_RANK.to_owned(), RankLimits { map_size: 128, max_drafts: 3 }),
        ("plus".to_owned(), RankLimits { map_size: 256, max_drafts: 6 }),
        ("creator".to_owned(), RankLimits { map_size: 512, max_drafts: 12 }),
    ])
}

#[derive(Resource, Clone)]
pub struct Db(pub Pool);

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
        owned: Vec<GameSummary>,
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
            .add_systems(Update, (load_accounts, create_games, apply_db_events));
    }
}

fn load_accounts(
    clients: Query<(Entity, &UniqueId, &Username), Added<Client>>,
    db: Res<Db>,
    events: Res<DbEvents>,
    tokio: Res<Tokio>,
) {
    for (client, uuid, username) in &clients {
        let (pool, tx, uuid, name) = (db.0.clone(), events.tx.clone(), uuid.0, username.0.clone());
        tokio.0.spawn(async move {
            let result = async {
                let rank = mm_db::player_joined(&pool, uuid, &name).await?;
                let owned = mm_db::owned_games(&pool, uuid).await?;
                Ok::<_, sqlx::Error>((rank, owned))
            }
            .await;
            let _ = tx.send(match result {
                Ok((rank, owned)) => DbEvent::Loaded {
                    client,
                    rank,
                    owned: owned.iter().map(|g| g.summary()).collect(),
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
            DbEvent::Loaded { client, rank, owned } => {
                if let Ok((_, mut games)) = players.get_mut(client) {
                    games.rank = rank;
                    games.owned = owned;
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
