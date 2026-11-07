//! Publishing: immutable versions of a draft, the live world of a persistent
//! game with the update a publish leaves pending for it, the regions such an
//! update copies, and per-player state in open worlds.

use mm_proto::LiveUpdate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::GameRow;

/// Regions one game may mark for "scripts + regions" publishes.
pub const MAX_PUBLISH_REGIONS: i64 = 32;

/// A box with both corners inclusive.
pub type Corners = ([i32; 3], [i32; 3]);

pub enum PublishOutcome {
    /// The game as it now is (with its new version).
    Published(Box<GameRow>),
    NoGame,
    /// Only the owner may publish.
    NotOwner,
    /// The owner has a creator ban.
    Banned,
    /// Moderators took the game down.
    Removed,
}

/// What a publish left for a persistent world to apply on its next load.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Pending {
    version: u32,
    mode: String,
    #[serde(default)]
    regions: Vec<Corners>,
}

fn rank(mode: LiveUpdate) -> u8 {
    match mode {
        LiveUpdate::Scripts => 0,
        LiveUpdate::Regions => 1,
        LiveUpdate::Reset => 2,
    }
}

/// Two publishes before the world loads: the stronger mode wins, regions add up.
fn merge(old: Option<Pending>, new: Pending) -> Pending {
    let Some(old) = old else {
        return new;
    };
    let old_mode = LiveUpdate::parse(&old.mode).unwrap_or(LiveUpdate::Scripts);
    let new_mode = LiveUpdate::parse(&new.mode).unwrap_or(LiveUpdate::Scripts);
    let mode = if rank(old_mode) > rank(new_mode) { old_mode } else { new_mode };
    let mut regions = Vec::new();
    if mode == LiveUpdate::Regions {
        for r in old.regions.into_iter().chain(new.regions) {
            if !regions.contains(&r) {
                regions.push(r);
            }
        }
    }
    Pending {
        version: new.version,
        mode: mode.as_str().to_owned(),
        regions,
    }
}

/// Snapshots the draft (map pointer and scripts) as the next version and
/// lists the game (a game on hold stays out of the browser until cleared).
/// Save a running draft's map first.
pub async fn publish(pool: &PgPool, game: i64, by: Uuid, mode: LiveUpdate) -> sqlx::Result<PublishOutcome> {
    let mut tx = pool.begin().await?;
    let row: Option<(Uuid, String, Option<i32>, Option<String>, Option<String>, bool)> = sqlx::query_as(&format!(
        "SELECT owner, kind, published_version, draft_map_hash, hold, {} FROM games WHERE id = $1 FOR UPDATE",
        crate::moderation::BANNED.replace("$PLAYER", "owner")
    ))
    .bind(game)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner, kind, published, map_hash, hold, banned)) = row else {
        return Ok(PublishOutcome::NoGame);
    };
    if owner != by {
        return Ok(PublishOutcome::NotOwner);
    }
    if banned {
        return Ok(PublishOutcome::Banned);
    }
    if hold.as_deref() == Some("removed") {
        return Ok(PublishOutcome::Removed);
    }
    let version = published.unwrap_or(0) + 1;
    sqlx::query("INSERT INTO game_versions (game, version, map_hash, published_by) VALUES ($1, $2, $3, $4)")
        .bind(game)
        .bind(version)
        .bind(&map_hash)
        .bind(by)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO game_version_files (game, version, path, source)
         SELECT game, $2, path, source FROM draft_files WHERE game = $1",
    )
    .bind(game)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE games SET published_version = $2, listed = true, updated_at = now(),
             first_published_at = COALESCE(first_published_at, now())
         WHERE id = $1",
    )
    .bind(game)
    .bind(version)
    .execute(&mut *tx)
    .await?;

    if kind == "persistent_world" {
        let live: Option<(Option<serde_json::Value>,)> =
            sqlx::query_as("SELECT pending FROM live_worlds WHERE game = $1 FOR UPDATE")
                .bind(game)
                .fetch_optional(&mut *tx)
                .await?;
        // No live world yet: its first load starts from this version anyway.
        if let Some((old,)) = live {
            let regions = if mode == LiveUpdate::Regions {
                publish_regions_in(&mut *tx, game).await?.into_iter().map(|(_, c)| c).collect()
            } else {
                Vec::new()
            };
            let new = Pending {
                version: version as u32,
                mode: mode.as_str().to_owned(),
                regions,
            };
            let old = old.and_then(|v| serde_json::from_value(v).ok());
            let pending = serde_json::to_value(merge(old, new)).expect("serializable");
            sqlx::query("UPDATE live_worlds SET pending = $2 WHERE game = $1")
                .bind(game)
                .bind(pending)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(match crate::game(pool, game).await? {
        Some(row) => PublishOutcome::Published(Box::new(row)),
        None => PublishOutcome::NoGame,
    })
}

pub struct Version {
    pub map_hash: Option<String>,
    pub files: Vec<(String, String)>,
}

pub async fn version(pool: &PgPool, game: i64, version: u32) -> sqlx::Result<Option<Version>> {
    let map: Option<(Option<String>,)> =
        sqlx::query_as("SELECT map_hash FROM game_versions WHERE game = $1 AND version = $2")
            .bind(game)
            .bind(version as i32)
            .fetch_optional(pool)
            .await?;
    let Some((map_hash,)) = map else {
        return Ok(None);
    };
    let files = sqlx::query_as("SELECT path, source FROM game_version_files WHERE game = $1 AND version = $2 ORDER BY path")
        .bind(game)
        .bind(version as i32)
        .fetch_all(pool)
        .await?;
    Ok(Some(Version { map_hash, files }))
}

/// A persistent world as its shard should run it.
#[derive(Debug, PartialEq)]
pub struct LiveWorld {
    /// The version whose scripts to run.
    pub version: u32,
    /// The world's map (NULL: a fresh flat map).
    pub map_hash: Option<String>,
    /// The update this claim applied, if any. For `Reset`, `map_hash` is
    /// already the published map; for `Regions`, copy `regions` from
    /// `version_map` over the world.
    pub update: Option<LiveUpdate>,
    pub regions: Vec<Corners>,
    pub version_map: Option<String>,
    /// Unix seconds of the last unload.
    pub last_shutdown: Option<i64>,
}

/// Loads (creating on first use) the game's live world, applying and
/// clearing any pending publish. Called when the world loads and when a
/// publish reaches it while running; returns `None` for an unpublished game.
pub async fn claim_live_world(pool: &PgPool, game: i64) -> sqlx::Result<Option<LiveWorld>> {
    let mut tx = pool.begin().await?;
    let published: Option<(Option<i32>,)> = sqlx::query_as("SELECT published_version FROM games WHERE id = $1")
        .bind(game)
        .fetch_optional(&mut *tx)
        .await?;
    let Some((Some(latest),)) = published else {
        return Ok(None);
    };
    let version_map = |v: i32| {
        sqlx::query_scalar::<_, Option<String>>("SELECT map_hash FROM game_versions WHERE game = $1 AND version = $2")
            .bind(game)
            .bind(v)
    };
    let row: Option<(i32, Option<String>, Option<serde_json::Value>, Option<i64>)> = sqlx::query_as(
        "SELECT version, map_hash, pending, extract(epoch FROM last_shutdown)::bigint
         FROM live_worlds WHERE game = $1 FOR UPDATE",
    )
    .bind(game)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((version, mut map_hash, pending, last_shutdown)) = row else {
        let map_hash = version_map(latest).fetch_optional(&mut *tx).await?.flatten();
        sqlx::query("INSERT INTO live_worlds (game, version, map_hash) VALUES ($1, $2, $3)")
            .bind(game)
            .bind(latest)
            .bind(&map_hash)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(Some(LiveWorld {
            version: latest as u32,
            version_map: map_hash.clone(),
            map_hash,
            update: None,
            regions: Vec::new(),
            last_shutdown: None,
        }));
    };
    let pending: Option<Pending> = pending.and_then(|v| serde_json::from_value(v).ok());
    let Some(pending) = pending else {
        tx.commit().await?;
        return Ok(Some(LiveWorld {
            version: version as u32,
            version_map: None,
            map_hash,
            update: None,
            regions: Vec::new(),
            last_shutdown,
        }));
    };
    let mode = LiveUpdate::parse(&pending.mode).unwrap_or(LiveUpdate::Scripts);
    let new_map = version_map(pending.version as i32).fetch_optional(&mut *tx).await?.flatten();
    if mode == LiveUpdate::Reset {
        sqlx::query("INSERT INTO live_world_backups (game, version, map_hash) VALUES ($1, $2, $3)")
            .bind(game)
            .bind(version)
            .bind(&map_hash)
            .execute(&mut *tx)
            .await?;
        map_hash = new_map.clone();
    }
    sqlx::query("UPDATE live_worlds SET version = $2, map_hash = $3, pending = NULL WHERE game = $1")
        .bind(game)
        .bind(pending.version as i32)
        .bind(&map_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(LiveWorld {
        version: pending.version,
        map_hash,
        update: Some(mode),
        regions: if mode == LiveUpdate::Regions { pending.regions } else { Vec::new() },
        version_map: new_map,
        last_shutdown,
    }))
}

/// Points the live world at a newly saved map; `shutdown` also records the
/// unload time.
pub async fn save_live_world(pool: &PgPool, game: i64, map_hash: &str, shutdown: bool) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE live_worlds SET map_hash = $2, saved_at = now(),
             last_shutdown = CASE WHEN $3 THEN now() ELSE last_shutdown END
         WHERE game = $1",
    )
    .bind(game)
    .bind(map_hash)
    .bind(shutdown)
    .execute(pool)
    .await?;
    Ok(())
}

/// Records an unload without a new map (nothing changed since the last save).
pub async fn mark_live_shutdown(pool: &PgPool, game: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE live_worlds SET last_shutdown = now() WHERE game = $1")
        .bind(game)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn live_world_backups(pool: &PgPool, game: i64) -> sqlx::Result<Vec<(i32, Option<String>)>> {
    sqlx::query_as("SELECT version, map_hash FROM live_world_backups WHERE game = $1 ORDER BY id")
        .bind(game)
        .fetch_all(pool)
        .await
}

async fn publish_regions_in<'e>(
    db: impl sqlx::PgExecutor<'e>,
    game: i64,
) -> sqlx::Result<Vec<(String, Corners)>> {
    let rows: Vec<(String, Vec<i32>, Vec<i32>)> =
        sqlx::query_as("SELECT name, min_pos, max_pos FROM publish_regions WHERE game = $1 ORDER BY name")
            .bind(game)
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(name, min, max)| Some((name, (min.try_into().ok()?, max.try_into().ok()?))))
        .collect())
}

pub async fn publish_regions(pool: &PgPool, game: i64) -> sqlx::Result<Vec<(String, Corners)>> {
    publish_regions_in(pool, game).await
}

/// Adds or replaces a region. Returns false when the game already has the
/// most regions allowed.
pub async fn set_publish_region(pool: &PgPool, game: i64, name: &str, (min, max): Corners) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO publish_regions (game, name, min_pos, max_pos)
         SELECT $1, $2, $3, $4
         WHERE EXISTS (SELECT 1 FROM publish_regions WHERE game = $1 AND name = $2)
            OR (SELECT count(*) FROM publish_regions WHERE game = $1) < $5
         ON CONFLICT (game, name) DO UPDATE SET min_pos = $3, max_pos = $4",
    )
    .bind(game)
    .bind(name)
    .bind(min.to_vec())
    .bind(max.to_vec())
    .bind(MAX_PUBLISH_REGIONS)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn delete_publish_region(pool: &PgPool, game: i64, name: &str) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM publish_regions WHERE game = $1 AND name = $2")
        .bind(game)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn player_state(pool: &PgPool, game: i64, player: Uuid) -> sqlx::Result<Option<Vec<u8>>> {
    sqlx::query_scalar("SELECT state FROM player_game_state WHERE game = $1 AND player = $2")
        .bind(game)
        .bind(player)
        .fetch_optional(pool)
        .await
}

pub async fn save_player_state(pool: &PgPool, game: i64, player: Uuid, state: &[u8]) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO player_game_state (game, player, state) VALUES ($1, $2, $3)
         ON CONFLICT (game, player) DO UPDATE SET state = $3, saved_at = now()",
    )
    .bind(game)
    .bind(player)
    .bind(state)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(version: u32, mode: LiveUpdate, regions: &[Corners]) -> Pending {
        Pending {
            version,
            mode: mode.as_str().to_owned(),
            regions: regions.to_vec(),
        }
    }

    #[test]
    fn merging_keeps_the_stronger_update() {
        let a = ([0, 0, 0], [1, 1, 1]);
        let b = ([5, 5, 5], [6, 6, 6]);
        let m = merge(Some(pending(2, LiveUpdate::Regions, &[a])), pending(3, LiveUpdate::Regions, &[a, b]));
        assert_eq!(m, pending(3, LiveUpdate::Regions, &[a, b]));
        let m = merge(Some(pending(2, LiveUpdate::Reset, &[])), pending(3, LiveUpdate::Regions, &[b]));
        assert_eq!(m, pending(3, LiveUpdate::Reset, &[]));
        let m = merge(Some(pending(2, LiveUpdate::Regions, &[a])), pending(3, LiveUpdate::Scripts, &[]));
        assert_eq!(m, pending(3, LiveUpdate::Regions, &[a]));
        assert_eq!(merge(None, pending(4, LiveUpdate::Scripts, &[])), pending(4, LiveUpdate::Scripts, &[]));
    }
}
