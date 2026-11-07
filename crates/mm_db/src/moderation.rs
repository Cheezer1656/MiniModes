//! Ranks, reports and the moderation queue, holds on games, creator bans,
//! and the log of text scripts showed players.

use mm_proto::Hold;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::{GAME_COLUMNS, GameRow};

/// SQL that is true while `$PLAYER` has a creator ban; substitute the
/// player's column or parameter for `$PLAYER`.
pub(crate) const BANNED: &str =
    "EXISTS (SELECT 1 FROM creator_bans b WHERE b.uuid = $PLAYER AND (b.until IS NULL OR b.until > now()))";

/// Finds a player who joined before, by name (case-insensitively).
pub async fn find_player(pool: &PgPool, name: &str) -> sqlx::Result<Option<(Uuid, String)>> {
    sqlx::query_as("SELECT uuid, username FROM players WHERE lower(username) = lower($1) ORDER BY last_seen DESC LIMIT 1")
        .bind(name)
        .fetch_optional(pool)
        .await
}

pub async fn rank(pool: &PgPool, player: Uuid) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT rank FROM ranks WHERE uuid = $1")
        .bind(player)
        .fetch_optional(pool)
        .await
}

/// The rank of `game`'s owner, if one was granted.
pub async fn owner_rank(pool: &PgPool, game: i64) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar("SELECT r.rank FROM games g JOIN ranks r ON r.uuid = g.owner WHERE g.id = $1")
        .bind(game)
        .fetch_optional(pool)
        .await
}

/// Grants `rank`, or with `None` returns the player to the default rank.
pub async fn set_rank(pool: &PgPool, player: Uuid, rank: Option<&str>, by: Option<Uuid>) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    match rank {
        Some(rank) => {
            sqlx::query(
                "INSERT INTO ranks (uuid, rank, granted_by) VALUES ($1, $2, $3)
                 ON CONFLICT (uuid) DO UPDATE SET rank = $2, granted_by = $3, granted_at = now()",
            )
            .bind(player)
            .bind(rank)
            .bind(by)
            .execute(&mut *tx)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM ranks WHERE uuid = $1").bind(player).execute(&mut *tx).await?;
        }
    }
    log(&mut tx, by, "rank", None, Some(player), rank.unwrap_or("(default)")).await?;
    tx.commit().await
}

async fn log(
    tx: &mut Transaction<'_, Postgres>,
    moderator: Option<Uuid>,
    action: &str,
    game: Option<i64>,
    target: Option<Uuid>,
    detail: &str,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO mod_actions (moderator, action, game, target, detail) VALUES ($1, $2, $3, $4, $5)")
        .bind(moderator)
        .bind(action)
        .bind(game)
        .bind(target)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub struct NewReport<'a> {
    pub game: i64,
    pub reporter: Uuid,
    pub reason: &'a str,
    pub version: Option<i32>,
}

#[derive(Debug, PartialEq)]
pub enum ReportOutcome {
    Filed {
        /// Open reports on the game, counting this one.
        open: i64,
        /// This report put the game on hold.
        held: bool,
    },
    /// The player already has an open report on the game.
    Duplicate,
    NoGame,
    OwnGame,
}

/// Files a report. When the game reaches `hold_at` open reports (from
/// different players), it goes on hold for review.
pub async fn report(pool: &PgPool, report: NewReport<'_>, hold_at: i64) -> sqlx::Result<ReportOutcome> {
    let mut tx = pool.begin().await?;
    // Serializes reports on one game, so exactly one crosses the threshold.
    let row: Option<(Uuid, Option<String>)> = sqlx::query_as("SELECT owner, hold FROM games WHERE id = $1 FOR UPDATE")
        .bind(report.game)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((owner, hold)) = row else {
        return Ok(ReportOutcome::NoGame);
    };
    if owner == report.reporter {
        return Ok(ReportOutcome::OwnGame);
    }
    let filed = sqlx::query(
        "INSERT INTO reports (game, reporter, reason, version) VALUES ($1, $2, $3, $4)
         ON CONFLICT (game, reporter) WHERE resolved_at IS NULL DO NOTHING",
    )
    .bind(report.game)
    .bind(report.reporter)
    .bind(report.reason)
    .bind(report.version)
    .execute(&mut *tx)
    .await?;
    if filed.rows_affected() == 0 {
        return Ok(ReportOutcome::Duplicate);
    }
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM reports WHERE game = $1 AND resolved_at IS NULL")
        .bind(report.game)
        .fetch_one(&mut *tx)
        .await?;
    let held = hold.is_none() && open >= hold_at;
    if held {
        sqlx::query("UPDATE games SET hold = 'review', hold_reason = $2 WHERE id = $1")
            .bind(report.game)
            .bind(format!("{open} reports"))
            .execute(&mut *tx)
            .await?;
        log(&mut tx, None, "auto_hold", Some(report.game), None, &format!("{open} reports")).await?;
    }
    tx.commit().await?;
    Ok(ReportOutcome::Filed { open, held })
}

#[derive(FromRow, Clone, Debug)]
pub struct ReportRow {
    pub reporter: String,
    pub reason: String,
    pub version: Option<i32>,
    pub age_secs: i64,
}

/// A game's open reports, newest first.
pub async fn open_reports(pool: &PgPool, game: i64, limit: i64) -> sqlx::Result<Vec<ReportRow>> {
    sqlx::query_as(
        "SELECT p.username AS reporter, r.reason, r.version,
                extract(epoch FROM now() - r.created_at)::bigint AS age_secs
         FROM reports r JOIN players p ON p.uuid = r.reporter
         WHERE r.game = $1 AND r.resolved_at IS NULL ORDER BY r.created_at DESC LIMIT $2",
    )
    .bind(game)
    .bind(limit)
    .fetch_all(pool)
    .await
}

#[derive(FromRow, Clone, Debug)]
pub struct QueueEntry {
    #[sqlx(flatten)]
    pub game: GameRow,
    pub open_reports: i64,
    pub hold_reason: Option<String>,
    /// The newest open report's reason.
    pub latest_reason: Option<String>,
}

fn queue_select() -> String {
    format!(
        "SELECT {GAME_COLUMNS}, g.hold_reason,
                (SELECT count(*) FROM reports r WHERE r.game = g.id AND r.resolved_at IS NULL) AS open_reports,
                (SELECT r.reason FROM reports r WHERE r.game = g.id AND r.resolved_at IS NULL
                 ORDER BY r.created_at DESC LIMIT 1) AS latest_reason
         FROM games g JOIN players p ON p.uuid = g.owner"
    )
}

/// Games waiting for a moderator: on review hold or with open reports,
/// held ones first, then by open reports.
pub async fn queue(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<QueueEntry>> {
    sqlx::query_as(&format!(
        "SELECT * FROM ({} WHERE g.hold IS DISTINCT FROM 'removed') q
         WHERE q.hold = 'review' OR q.open_reports > 0
         ORDER BY q.hold IS NULL, q.open_reports DESC, q.id
         LIMIT $1",
        queue_select()
    ))
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One game as the moderation queue shows it, whatever its state.
pub async fn queue_entry(pool: &PgPool, game: i64) -> sqlx::Result<Option<QueueEntry>> {
    sqlx::query_as(&format!("{} WHERE g.id = $1", queue_select()))
        .bind(game)
        .fetch_optional(pool)
        .await
}

/// Resolves the game's open reports and lifts any hold. Returns the game
/// as it now is.
pub async fn clear(pool: &PgPool, game: i64, moderator: Uuid) -> sqlx::Result<Option<GameRow>> {
    let mut tx = pool.begin().await?;
    let resolved = sqlx::query(
        "UPDATE reports SET resolved_at = now(), resolved_by = $2 WHERE game = $1 AND resolved_at IS NULL",
    )
    .bind(game)
    .bind(moderator)
    .execute(&mut *tx)
    .await?;
    let updated = sqlx::query("UPDATE games SET hold = NULL, hold_reason = NULL WHERE id = $1")
        .bind(game)
        .execute(&mut *tx)
        .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    let detail = format!("{} reports resolved", resolved.rows_affected());
    log(&mut tx, Some(moderator), "clear", Some(game), None, &detail).await?;
    tx.commit().await?;
    crate::game(pool, game).await
}

/// Puts the game on hold. Returns false if it doesn't exist.
pub async fn set_hold(pool: &PgPool, game: i64, hold: Hold, reason: &str, moderator: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query("UPDATE games SET hold = $2, hold_reason = $3 WHERE id = $1")
        .bind(game)
        .bind(hold.as_str())
        .bind(reason)
        .execute(&mut *tx)
        .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    log(&mut tx, Some(moderator), hold.as_str(), Some(game), None, reason).await?;
    tx.commit().await?;
    Ok(true)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ban {
    pub reason: String,
    /// Unix seconds; `None` until lifted.
    pub until: Option<i64>,
}

pub async fn creator_ban(pool: &PgPool, player: Uuid) -> sqlx::Result<Option<Ban>> {
    let row: Option<(String, Option<i64>)> = sqlx::query_as(
        "SELECT reason, extract(epoch FROM until)::bigint FROM creator_bans
         WHERE uuid = $1 AND (until IS NULL OR until > now())",
    )
    .bind(player)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(reason, until)| Ban { reason, until }))
}

/// Bans a creator (for `days`, or until lifted) and puts each of their
/// games that isn't already held on review hold. Returns all their games.
pub async fn ban_creator(
    pool: &PgPool,
    player: Uuid,
    reason: &str,
    days: Option<i32>,
    by: Uuid,
) -> sqlx::Result<Vec<i64>> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO creator_bans (uuid, reason, banned_by, until)
         VALUES ($1, $2, $3, now() + make_interval(days => $4))
         ON CONFLICT (uuid) DO UPDATE SET reason = $2, banned_by = $3, banned_at = now(),
             until = now() + make_interval(days => $4)",
    )
    .bind(player)
    .bind(reason)
    .bind(by)
    .bind(days)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE games SET hold = 'review', hold_reason = 'creator banned' WHERE owner = $1 AND hold IS NULL")
        .bind(player)
        .execute(&mut *tx)
        .await?;
    let games = sqlx::query_scalar("SELECT id FROM games WHERE owner = $1")
        .bind(player)
        .fetch_all(&mut *tx)
        .await?;
    let detail = match days {
        Some(d) => format!("{d} days: {reason}"),
        None => reason.to_owned(),
    };
    log(&mut tx, Some(by), "ban", None, Some(player), &detail).await?;
    tx.commit().await?;
    Ok(games)
}

/// Lifts a creator ban. Their games stay held until a moderator clears
/// each. Returns false if they weren't banned.
pub async fn unban_creator(pool: &PgPool, player: Uuid, by: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let lifted = sqlx::query("DELETE FROM creator_bans WHERE uuid = $1 AND (until IS NULL OR until > now())")
        .bind(player)
        .execute(&mut *tx)
        .await?;
    if lifted.rows_affected() == 0 {
        return Ok(false);
    }
    log(&mut tx, Some(by), "unban", None, Some(player), "").await?;
    tx.commit().await?;
    Ok(true)
}

/// Records a moderator stopping a game's instances.
pub async fn log_kill(pool: &PgPool, game: i64, moderator: Uuid) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    log(&mut tx, Some(moderator), "kill", Some(game), None, "").await?;
    tx.commit().await
}

/// Open reports and held games, for metrics.
pub async fn counts(pool: &PgPool) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM reports WHERE resolved_at IS NULL),
                (SELECT count(*) FROM games WHERE hold = 'review')",
    )
    .fetch_one(pool)
    .await
}

/// One player-facing string from a script.
#[derive(Clone, Debug)]
pub struct TextLine {
    pub game: i64,
    pub version: Option<i32>,
    pub instance: i64,
    pub player: Option<Uuid>,
    pub kind: &'static str,
    pub text: String,
}

pub async fn write_text_log(pool: &PgPool, lines: &[TextLine]) -> sqlx::Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut games = Vec::with_capacity(lines.len());
    let mut versions = Vec::with_capacity(lines.len());
    let mut instances = Vec::with_capacity(lines.len());
    let mut players = Vec::with_capacity(lines.len());
    let mut kinds = Vec::with_capacity(lines.len());
    let mut texts = Vec::with_capacity(lines.len());
    for l in lines {
        games.push(l.game);
        versions.push(l.version);
        instances.push(l.instance);
        players.push(l.player);
        kinds.push(l.kind);
        texts.push(l.text.as_str());
    }
    sqlx::query(
        // clock_timestamp() rather than now(), so lines keep their order.
        "INSERT INTO script_text_log (at, game, version, instance, player, kind, text)
         SELECT clock_timestamp(), * FROM UNNEST($1::bigint[], $2::integer[], $3::bigint[], $4::uuid[], $5::text[], $6::text[])",
    )
    .bind(games)
    .bind(versions)
    .bind(instances)
    .bind(players)
    .bind(kinds)
    .bind(texts)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(FromRow, Clone, Debug)]
pub struct LoggedText {
    pub age_secs: i64,
    pub version: Option<i32>,
    pub player: Option<String>,
    pub kind: String,
    pub text: String,
}

/// The game's most recent logged text, newest first.
pub async fn text_log(pool: &PgPool, game: i64, limit: i64) -> sqlx::Result<Vec<LoggedText>> {
    sqlx::query_as(
        "SELECT extract(epoch FROM now() - l.at)::bigint AS age_secs, l.version, p.username AS player, l.kind, l.text
         FROM script_text_log l LEFT JOIN players p ON p.uuid = l.player
         WHERE l.game = $1 ORDER BY l.at DESC LIMIT $2",
    )
    .bind(game)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Creates the coming days' log partitions and drops those past `keep_days`.
pub async fn maintain_text_log(pool: &PgPool, keep_days: i32) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    // Without this, every partition that already exists logs a notice.
    sqlx::query("SET LOCAL client_min_messages = warning").execute(&mut *tx).await?;
    sqlx::query("SELECT script_text_log_maintain($1)").bind(keep_days).execute(&mut *tx).await?;
    tx.commit().await
}
