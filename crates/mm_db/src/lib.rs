//! Postgres access shared by the lobby and shards. Queries are checked at
//! runtime (no `DATABASE_URL` needed to build); migrations are embedded.

pub mod datastore;
pub mod editor;
pub mod library;
pub mod publish;

use std::time::Duration;

use mm_proto::{GameId, GameKind, GameSummary};
use sqlx::postgres::PgPoolOptions;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

pub use sqlx::Error;
pub use sqlx::PgPool as Pool;

/// Environment variable holding the Postgres URL.
pub const DATABASE_URL_ENV: &str = "MINIMODES_DATABASE_URL";

/// Connects and applies pending migrations. Every process calls this;
/// sqlx serializes concurrent migrators with an advisory lock.
pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url)
        .await?;
    sqlx::migrate!().run(&pool).await?;
    Ok(pool)
}

pub fn url_from_env() -> anyhow::Result<String> {
    std::env::var(DATABASE_URL_ENV).map_err(|_| anyhow::anyhow!("{DATABASE_URL_ENV} must be set"))
}

pub fn kind_to_str(kind: GameKind) -> &'static str {
    match kind {
        GameKind::Match => "match",
        GameKind::OpenWorld {
            persistent_world: false,
        } => "open_world",
        GameKind::OpenWorld {
            persistent_world: true,
        } => "persistent_world",
    }
}

pub fn kind_from_str(kind: &str) -> Option<GameKind> {
    Some(match kind {
        "match" => GameKind::Match,
        "open_world" => GameKind::OpenWorld {
            persistent_world: false,
        },
        "persistent_world" => GameKind::OpenWorld {
            persistent_world: true,
        },
        _ => return None,
    })
}

#[derive(FromRow, Clone, Debug)]
pub struct GameRow {
    pub id: i64,
    pub owner: Uuid,
    pub owner_name: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub kind: String,
    pub max_players: i32,
    pub map_size: i32,
    pub draft_map_hash: Option<String>,
    pub created_unix: i64,
    pub published_version: Option<i32>,
    pub published_unix: Option<i64>,
    /// Open worlds: save each player's inventory and position (settings
    /// `save_player_state`, default on).
    pub save_player_state: bool,
    pub rating_avg: Option<f64>,
    pub rating_count: i64,
    /// Distinct players who joined the published game.
    pub play_count: i64,
}

impl GameRow {
    pub fn kind(&self) -> GameKind {
        kind_from_str(&self.kind).unwrap_or(GameKind::Match)
    }

    pub fn summary(&self) -> GameSummary {
        GameSummary {
            id: GameId(self.id),
            name: self.name.clone(),
            creator: self.owner_name.clone(),
            description: self.description.clone(),
            icon: self.icon.clone(),
            kind: self.kind(),
            max_players: self.max_players as u32,
            featured: false,
            published_at: self.published_unix.unwrap_or(self.created_unix),
            version: self.published_version.unwrap_or(0) as u32,
            rating: self.rating_avg.map(|r| r as f32),
            ratings: self.rating_count as u32,
            plays: self.play_count as u32,
        }
    }
}

pub(crate) const GAME_COLUMNS: &str = "g.id, g.owner, p.username AS owner_name, g.name, g.description, g.icon, g.kind, \
     g.max_players, g.map_size, g.draft_map_hash, extract(epoch FROM g.created_at)::bigint AS created_unix, \
     g.published_version, extract(epoch FROM g.first_published_at)::bigint AS published_unix, \
     COALESCE((g.settings->>'save_player_state')::boolean, true) AS save_player_state, \
     (SELECT avg(r.stars)::float8 FROM ratings r WHERE r.game = g.id) AS rating_avg, \
     (SELECT count(*) FROM ratings r WHERE r.game = g.id) AS rating_count, \
     (SELECT count(*) FROM plays pl WHERE pl.game = g.id) AS play_count";

/// Records a login and returns the player's rank, if one was granted.
pub async fn player_joined(pool: &PgPool, uuid: Uuid, username: &str) -> sqlx::Result<Option<String>> {
    sqlx::query(
        "INSERT INTO players (uuid, username) VALUES ($1, $2)
         ON CONFLICT (uuid) DO UPDATE SET username = EXCLUDED.username, last_seen = now()",
    )
    .bind(uuid)
    .bind(username)
    .execute(pool)
    .await?;
    sqlx::query_scalar("SELECT rank FROM ranks WHERE uuid = $1")
        .bind(uuid)
        .fetch_optional(pool)
        .await
}

pub struct NewGame<'a> {
    pub owner: Uuid,
    pub name: &'a str,
    pub icon: &'a str,
    pub kind: GameKind,
    pub max_players: i32,
    pub map_size: i32,
}

/// Creates a game unless the owner already has `max_drafts`. Returns `None`
/// when over quota.
pub async fn create_game(pool: &PgPool, game: NewGame<'_>, max_drafts: i64) -> sqlx::Result<Option<GameRow>> {
    let mut tx = pool.begin().await?;
    // Serialize creates per owner so two concurrent requests can't both pass the quota.
    sqlx::query("SELECT 1 FROM players WHERE uuid = $1 FOR UPDATE")
        .bind(game.owner)
        .execute(&mut *tx)
        .await?;
    let owned: i64 = sqlx::query_scalar("SELECT count(*) FROM games WHERE owner = $1")
        .bind(game.owner)
        .fetch_one(&mut *tx)
        .await?;
    if owned >= max_drafts {
        return Ok(None);
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO games (owner, name, icon, kind, max_players, map_size)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(game.owner)
    .bind(game.name)
    .bind(game.icon)
    .bind(kind_to_str(game.kind))
    .bind(game.max_players)
    .bind(game.map_size)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    self::game(pool, id).await
}

pub async fn game(pool: &PgPool, id: i64) -> sqlx::Result<Option<GameRow>> {
    sqlx::query_as(&format!("SELECT {GAME_COLUMNS} FROM games g JOIN players p ON p.uuid = g.owner WHERE g.id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn owned_games(pool: &PgPool, owner: Uuid) -> sqlx::Result<Vec<GameRow>> {
    sqlx::query_as(&format!(
        "SELECT {GAME_COLUMNS} FROM games g JOIN players p ON p.uuid = g.owner
         WHERE g.owner = $1 ORDER BY g.created_at DESC"
    ))
    .bind(owner)
    .fetch_all(pool)
    .await
}

/// Published games anyone can play, for the lobby's browser.
pub async fn listed_games(pool: &PgPool) -> sqlx::Result<Vec<GameRow>> {
    sqlx::query_as(&format!(
        "SELECT {GAME_COLUMNS} FROM games g JOIN players p ON p.uuid = g.owner
         WHERE g.listed AND g.published_version IS NOT NULL ORDER BY g.first_published_at DESC"
    ))
    .fetch_all(pool)
    .await
}

/// Changes one key of a game's `settings`.
pub async fn set_game_setting(pool: &PgPool, game: i64, key: &str, value: serde_json::Value) -> sqlx::Result<()> {
    sqlx::query("UPDATE games SET settings = jsonb_set(settings, ARRAY[$2], $3), updated_at = now() WHERE id = $1")
        .bind(game)
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// Points the draft at a newly saved map.
pub async fn set_draft_map(pool: &PgPool, game: i64, hash: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE games SET draft_map_hash = $2, updated_at = now() WHERE id = $1")
        .bind(game)
        .bind(hash)
        .execute(pool)
        .await?;
    Ok(())
}

/// A draft's scripts as `(path, source)`, sorted by path.
pub async fn draft_files(pool: &PgPool, game: i64) -> sqlx::Result<Vec<(String, String)>> {
    sqlx::query_as("SELECT path, source FROM draft_files WHERE game = $1 ORDER BY path")
        .bind(game)
        .fetch_all(pool)
        .await
}

/// Writes a file without the editor's checks or history (tests and tools).
pub async fn put_draft_file(pool: &PgPool, game: i64, path: &str, source: &str, by: Option<Uuid>) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO draft_files (game, path, source, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (game, path) DO UPDATE
         SET source = $3, updated_by = $4, updated_at = now(), version = draft_files.version + 1",
    )
    .bind(game)
    .bind(path)
    .bind(source)
    .bind(by)
    .execute(pool)
    .await?;
    Ok(())
}
