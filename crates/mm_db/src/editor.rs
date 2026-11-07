//! Web editor data: who may edit a draft, one-time links and sessions, and
//! the draft's script files with their history.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use mm_proto::scripts::{MAX_FILES, MAX_TOTAL_BYTES};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

use crate::{GAME_COLUMNS, GameRow};

/// How long a `/script edit` link works.
pub const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);
pub const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Revisions kept per file.
pub const HISTORY_PER_FILE: i64 = 50;
/// The owner plus these many fill a draft instance (8 players).
pub const MAX_COLLABORATORS: i64 = 7;

/// A random secret for a link or cookie, and the hash stored in its place.
pub fn new_secret() -> (String, Vec<u8>) {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).expect("the OS random source failed");
    let secret = URL_SAFE_NO_PAD.encode(bytes);
    let hash = hash_secret(&secret);
    (secret, hash)
}

pub fn hash_secret(secret: &str) -> Vec<u8> {
    blake3::hash(secret.as_bytes()).as_bytes().to_vec()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Owner,
    Collaborator,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Collaborator => "collaborator",
        }
    }
}

/// What `player` may do with `game`'s draft, if anything. Banned creators
/// may do nothing.
pub async fn role(pool: &PgPool, game: i64, player: Uuid) -> sqlx::Result<Option<Role>> {
    let row: Option<(bool,)> = sqlx::query_as(&format!(
        "SELECT g.owner = $2 FROM games g
         WHERE g.id = $1 AND (g.owner = $2 OR EXISTS (
             SELECT 1 FROM game_collaborators c WHERE c.game = g.id AND c.player = $2))
         AND NOT {}",
        crate::moderation::BANNED.replace("$PLAYER", "$2")
    ))
    .bind(game)
    .bind(player)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(owner,)| if owner { Role::Owner } else { Role::Collaborator }))
}

/// Games `player` collaborates on (not their own), newest first.
pub async fn shared_games(pool: &PgPool, player: Uuid) -> sqlx::Result<Vec<GameRow>> {
    sqlx::query_as(&format!(
        "SELECT {GAME_COLUMNS} FROM games g JOIN players p ON p.uuid = g.owner
         JOIN game_collaborators c ON c.game = g.id
         WHERE c.player = $1 ORDER BY g.created_at DESC"
    ))
    .bind(player)
    .fetch_all(pool)
    .await
}

pub async fn create_token(pool: &PgPool, hash: &[u8], player: Uuid, game: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM editor_tokens WHERE expires_at < now()").execute(pool).await?;
    sqlx::query("INSERT INTO editor_tokens (hash, player, game, expires_at) VALUES ($1, $2, $3, now() + $4)")
        .bind(hash)
        .bind(player)
        .bind(game)
        .bind(TOKEN_TTL)
        .execute(pool)
        .await?;
    Ok(())
}

/// Uses up a link, returning whose it was and for which game.
pub async fn redeem_token(pool: &PgPool, hash: &[u8]) -> sqlx::Result<Option<(Uuid, i64)>> {
    sqlx::query_as("DELETE FROM editor_tokens WHERE hash = $1 AND expires_at > now() RETURNING player, game")
        .bind(hash)
        .fetch_optional(pool)
        .await
}

pub async fn create_session(pool: &PgPool, hash: &[u8], player: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM editor_sessions WHERE expires_at < now()").execute(pool).await?;
    sqlx::query("INSERT INTO editor_sessions (hash, player, expires_at) VALUES ($1, $2, now() + $3)")
        .bind(hash)
        .bind(player)
        .bind(SESSION_TTL)
        .execute(pool)
        .await?;
    Ok(())
}

/// The session's player and their username.
pub async fn session(pool: &PgPool, hash: &[u8]) -> sqlx::Result<Option<(Uuid, String)>> {
    sqlx::query_as(
        "SELECT s.player, p.username FROM editor_sessions s JOIN players p ON p.uuid = s.player
         WHERE s.hash = $1 AND s.expires_at > now()",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await
}

pub async fn delete_session(pool: &PgPool, hash: &[u8]) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM editor_sessions WHERE hash = $1").bind(hash).execute(pool).await?;
    Ok(())
}

#[derive(FromRow, Clone, Debug)]
pub struct FileMeta {
    pub path: String,
    pub size: i64,
    pub version: i64,
    pub updated_unix: i64,
    pub updated_by: Option<String>,
}

#[derive(FromRow, Clone, Debug)]
pub struct FileRow {
    pub source: String,
    pub version: i64,
    pub updated_unix: i64,
    pub updated_by: Option<String>,
}

pub async fn list_files(pool: &PgPool, game: i64) -> sqlx::Result<Vec<FileMeta>> {
    sqlx::query_as(
        "SELECT f.path, octet_length(f.source)::bigint AS size, f.version,
                extract(epoch FROM f.updated_at)::bigint AS updated_unix, p.username AS updated_by
         FROM draft_files f LEFT JOIN players p ON p.uuid = f.updated_by
         WHERE f.game = $1 ORDER BY f.path",
    )
    .bind(game)
    .fetch_all(pool)
    .await
}

pub async fn file(pool: &PgPool, game: i64, path: &str) -> sqlx::Result<Option<FileRow>> {
    sqlx::query_as(
        "SELECT f.source, f.version, extract(epoch FROM f.updated_at)::bigint AS updated_unix,
                p.username AS updated_by
         FROM draft_files f LEFT JOIN players p ON p.uuid = f.updated_by
         WHERE f.game = $1 AND f.path = $2",
    )
    .bind(game)
    .bind(path)
    .fetch_optional(pool)
    .await
}

#[derive(PartialEq, Eq, Debug)]
pub enum SaveOutcome {
    Saved { version: i64 },
    /// Someone saved since `base`; nothing was written.
    Conflict { version: i64, by: Option<String> },
    /// The game is gone.
    NoGame,
    TooManyFiles,
    TooLarge { total: usize },
}

/// Writes one file, keeping every file of the game within the bundle
/// limits. With `base`, the save only happens if the file is still at that
/// version (0: it must not exist yet); without, the last write wins.
pub async fn save_file(
    pool: &PgPool,
    game: i64,
    path: &str,
    source: &str,
    by: Uuid,
    base: Option<i64>,
) -> sqlx::Result<SaveOutcome> {
    let mut tx = pool.begin().await?;
    // Serializes saves per game so the size check can't race.
    if sqlx::query("SELECT 1 FROM games WHERE id = $1 FOR UPDATE")
        .bind(game)
        .fetch_optional(&mut *tx)
        .await?
        .is_none()
    {
        return Ok(SaveOutcome::NoGame);
    }
    let current: Option<(i64, Option<String>)> = sqlx::query_as(
        "SELECT f.version, p.username FROM draft_files f LEFT JOIN players p ON p.uuid = f.updated_by
         WHERE f.game = $1 AND f.path = $2",
    )
    .bind(game)
    .bind(path)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(base) = base {
        let now = current.as_ref().map_or(0, |c| c.0);
        if now != base {
            return Ok(SaveOutcome::Conflict {
                version: now,
                by: current.and_then(|c| c.1),
            });
        }
    }
    let (others, other_bytes): (i64, Option<i64>) = sqlx::query_as(
        "SELECT count(*), sum(octet_length(path) + octet_length(source))::bigint
         FROM draft_files WHERE game = $1 AND path <> $2",
    )
    .bind(game)
    .bind(path)
    .fetch_one(&mut *tx)
    .await?;
    if others as usize + 1 > MAX_FILES {
        return Ok(SaveOutcome::TooManyFiles);
    }
    let total = other_bytes.unwrap_or(0) as usize + path.len() + source.len();
    if total > MAX_TOTAL_BYTES {
        return Ok(SaveOutcome::TooLarge { total });
    }
    let version: i64 = sqlx::query_scalar(
        "INSERT INTO draft_files (game, path, source, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT (game, path) DO UPDATE
         SET source = $3, updated_by = $4, updated_at = now(), version = draft_files.version + 1
         RETURNING version",
    )
    .bind(game)
    .bind(path)
    .bind(source)
    .bind(by)
    .fetch_one(&mut *tx)
    .await?;
    record_history(&mut tx, game, path, Some(source), Some(by)).await?;
    tx.commit().await?;
    Ok(SaveOutcome::Saved { version })
}

/// Returns false if there was no such file.
pub async fn delete_file(pool: &PgPool, game: i64, path: &str, by: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let deleted = sqlx::query("DELETE FROM draft_files WHERE game = $1 AND path = $2")
        .bind(game)
        .bind(path)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        > 0;
    if deleted {
        record_history(&mut tx, game, path, None, Some(by)).await?;
    }
    tx.commit().await?;
    Ok(deleted)
}

#[derive(PartialEq, Eq, Debug)]
pub enum RenameOutcome {
    Renamed,
    Missing,
    Exists,
}

pub async fn rename_file(pool: &PgPool, game: i64, from: &str, to: &str, by: Uuid) -> sqlx::Result<RenameOutcome> {
    let mut tx = pool.begin().await?;
    let source: Option<String> =
        sqlx::query_scalar("SELECT source FROM draft_files WHERE game = $1 AND path = $2 FOR UPDATE")
            .bind(game)
            .bind(from)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(source) = source else {
        return Ok(RenameOutcome::Missing);
    };
    let moved = sqlx::query(
        "INSERT INTO draft_files (game, path, source, updated_by) VALUES ($1, $2, $3, $4)
         ON CONFLICT DO NOTHING",
    )
    .bind(game)
    .bind(to)
    .bind(&source)
    .bind(by)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved == 0 {
        return Ok(RenameOutcome::Exists);
    }
    sqlx::query("DELETE FROM draft_files WHERE game = $1 AND path = $2")
        .bind(game)
        .bind(from)
        .execute(&mut *tx)
        .await?;
    record_history(&mut tx, game, from, None, Some(by)).await?;
    record_history(&mut tx, game, to, Some(&source), Some(by)).await?;
    tx.commit().await?;
    Ok(RenameOutcome::Renamed)
}

async fn record_history(
    tx: &mut sqlx::PgConnection,
    game: i64,
    path: &str,
    source: Option<&str>,
    by: Option<Uuid>,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO draft_file_history (game, path, source, updated_by) VALUES ($1, $2, $3, $4)")
        .bind(game)
        .bind(path)
        .bind(source)
        .bind(by)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM draft_file_history WHERE game = $1 AND path = $2 AND id <= (
             SELECT id FROM draft_file_history WHERE game = $1 AND path = $2
             ORDER BY id DESC OFFSET $3 LIMIT 1)",
    )
    .bind(game)
    .bind(path)
    .bind(HISTORY_PER_FILE)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(FromRow, Clone, Debug)]
pub struct Revision {
    pub id: i64,
    /// `None` where the file was deleted.
    pub size: Option<i64>,
    pub updated_unix: i64,
    pub updated_by: Option<String>,
}

/// A file's revisions, newest first.
pub async fn history(pool: &PgPool, game: i64, path: &str) -> sqlx::Result<Vec<Revision>> {
    sqlx::query_as(
        "SELECT h.id, octet_length(h.source)::bigint AS size,
                extract(epoch FROM h.updated_at)::bigint AS updated_unix, p.username AS updated_by
         FROM draft_file_history h LEFT JOIN players p ON p.uuid = h.updated_by
         WHERE h.game = $1 AND h.path = $2 ORDER BY h.id DESC",
    )
    .bind(game)
    .bind(path)
    .fetch_all(pool)
    .await
}

/// One revision's path and source.
pub async fn revision(pool: &PgPool, game: i64, id: i64) -> sqlx::Result<Option<(String, Option<String>)>> {
    sqlx::query_as("SELECT path, source FROM draft_file_history WHERE game = $1 AND id = $2")
        .bind(game)
        .bind(id)
        .fetch_optional(pool)
        .await
}

#[derive(FromRow, Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub uuid: Uuid,
    pub username: String,
}

pub async fn collaborators(pool: &PgPool, game: i64) -> sqlx::Result<Vec<Member>> {
    sqlx::query_as(
        "SELECT p.uuid, p.username FROM game_collaborators c JOIN players p ON p.uuid = c.player
         WHERE c.game = $1 ORDER BY c.added_at",
    )
    .bind(game)
    .fetch_all(pool)
    .await
}

#[derive(PartialEq, Eq, Debug)]
pub enum AddOutcome {
    Added(Member),
    /// Nobody by that name has joined the network.
    UnknownPlayer,
    IsOwner,
    Already,
    Full,
}

/// Adds a collaborator by username (case-insensitive).
pub async fn add_collaborator(pool: &PgPool, game: i64, username: &str, by: Uuid) -> sqlx::Result<AddOutcome> {
    let mut tx = pool.begin().await?;
    let Some(owner): Option<Uuid> = sqlx::query_scalar("SELECT owner FROM games WHERE id = $1 FOR UPDATE")
        .bind(game)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(AddOutcome::UnknownPlayer);
    };
    let member: Option<Member> = sqlx::query_as(
        "SELECT uuid, username FROM players WHERE lower(username) = lower($1) ORDER BY last_seen DESC LIMIT 1",
    )
    .bind(username)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(member) = member else {
        return Ok(AddOutcome::UnknownPlayer);
    };
    if member.uuid == owner {
        return Ok(AddOutcome::IsOwner);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM game_collaborators WHERE game = $1")
        .bind(game)
        .fetch_one(&mut *tx)
        .await?;
    let added = sqlx::query(
        "INSERT INTO game_collaborators (game, player, added_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
    )
    .bind(game)
    .bind(member.uuid)
    .bind(by)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if added == 0 {
        return Ok(AddOutcome::Already);
    }
    if count >= MAX_COLLABORATORS {
        return Ok(AddOutcome::Full);
    }
    tx.commit().await?;
    Ok(AddOutcome::Added(member))
}

pub async fn remove_collaborator(pool: &PgPool, game: i64, player: Uuid) -> sqlx::Result<bool> {
    Ok(sqlx::query("DELETE FROM game_collaborators WHERE game = $1 AND player = $2")
        .bind(game)
        .bind(player)
        .execute(pool)
        .await?
        .rows_affected()
        > 0)
}
