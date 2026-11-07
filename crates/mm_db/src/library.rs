//! What players keep across visits to the browser: favorites, the games
//! they played, and their ratings.

use sqlx::PgPool;
use uuid::Uuid;

/// Recently played games the browser lists.
pub const RECENT_LIMIT: i64 = 28;

#[derive(Default, Debug)]
pub struct Library {
    /// Oldest first.
    pub favorites: Vec<i64>,
    /// Every published game the player joined, most recent first.
    pub played: Vec<i64>,
    pub ratings: Vec<(i64, i16)>,
}

pub async fn library(pool: &PgPool, player: Uuid) -> sqlx::Result<Library> {
    let favorites = sqlx::query_scalar("SELECT game FROM favorites WHERE player = $1 ORDER BY added_at")
        .bind(player)
        .fetch_all(pool)
        .await?;
    let played = sqlx::query_scalar("SELECT game FROM plays WHERE player = $1 ORDER BY last_played DESC")
        .bind(player)
        .fetch_all(pool)
        .await?;
    let ratings = sqlx::query_as("SELECT game, stars FROM ratings WHERE player = $1")
        .bind(player)
        .fetch_all(pool)
        .await?;
    Ok(Library {
        favorites,
        played,
        ratings,
    })
}

pub async fn set_favorite(pool: &PgPool, player: Uuid, game: i64, on: bool) -> sqlx::Result<()> {
    let query = if on {
        "INSERT INTO favorites (player, game) VALUES ($1, $2) ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM favorites WHERE player = $1 AND game = $2"
    };
    sqlx::query(query).bind(player).bind(game).execute(pool).await?;
    Ok(())
}

/// Returns whether it was the player's first time.
pub async fn record_play(pool: &PgPool, player: Uuid, game: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "INSERT INTO plays (player, game) VALUES ($1, $2)
         ON CONFLICT (player, game) DO UPDATE SET count = plays.count + 1, last_played = now()
         RETURNING count = 1",
    )
    .bind(player)
    .bind(game)
    .fetch_one(pool)
    .await
}

/// The game's mean rating and how many players rated it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RatingSummary {
    pub mean: Option<f64>,
    pub count: i64,
}

/// Rates a game the player has played (`None` if they haven't) and returns
/// the game's new rating.
pub async fn rate(pool: &PgPool, player: Uuid, game: i64, stars: i16) -> sqlx::Result<Option<RatingSummary>> {
    let rated = sqlx::query(
        "INSERT INTO ratings (player, game, stars)
         SELECT $1, $2, $3 WHERE EXISTS (SELECT 1 FROM plays WHERE player = $1 AND game = $2)
         ON CONFLICT (player, game) DO UPDATE SET stars = $3, rated_at = now()",
    )
    .bind(player)
    .bind(game)
    .bind(stars)
    .execute(pool)
    .await?;
    if rated.rows_affected() == 0 {
        return Ok(None);
    }
    let (mean, count) = sqlx::query_as("SELECT avg(stars)::float8, count(*) FROM ratings WHERE game = $1")
        .bind(game)
        .fetch_one(pool)
        .await?;
    Ok(Some(RatingSummary { mean, count }))
}
