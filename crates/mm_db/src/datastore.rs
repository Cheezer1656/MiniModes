//! Storage behind scripts' `DataStoreService`. Every write locks the game's
//! usage row first, so writes to one game (and draft or live side) run one
//! at a time: compare-and-set and the quota check need no retries.

use sqlx::{PgPool, Postgres, Transaction};

/// Which store an operation targets.
#[derive(Clone, Copy, Debug)]
pub struct Store<'a> {
    pub game: i64,
    /// Drafts keep separate data from the published game.
    pub draft: bool,
    /// Ordered stores hold integers and can be listed by value.
    pub ordered: bool,
    pub name: &'a str,
}

impl Store<'_> {
    fn table(&self) -> &'static str {
        if self.ordered {
            "ordered_datastore_entries"
        } else {
            "datastore_entries"
        }
    }
}

/// A stored value: JSON text in a regular store, an integer in an ordered one.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Json(String),
    Int(i64),
}

impl Value {
    fn size(&self) -> i64 {
        match self {
            Self::Json(s) => s.len() as i64,
            Self::Int(_) => 8,
        }
    }
}

/// A value and its version, which every write bumps.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub value: Value,
    pub version: i64,
}

#[derive(Debug, PartialEq)]
pub enum Written {
    Ok(Entry),
    /// The entry didn't match `expect`; this is what's there now.
    Conflict(Option<Entry>),
    /// The write would take the game over its quota.
    OverQuota,
    /// `increment` on a value that isn't an integer.
    NotInteger,
}

async fn fetch(conn: &mut sqlx::PgConnection, store: Store<'_>, key: &str, lock: bool) -> sqlx::Result<Option<Entry>> {
    let sql = format!(
        "SELECT value, version FROM {} WHERE game = $1 AND draft = $2 AND store = $3 AND key = $4{}",
        store.table(),
        if lock { " FOR UPDATE" } else { "" },
    );
    let query = sqlx::query(&sql).bind(store.game).bind(store.draft).bind(store.name).bind(key);
    let Some(row) = query.fetch_optional(conn).await? else {
        return Ok(None);
    };
    use sqlx::Row;
    let value = if store.ordered {
        Value::Int(row.try_get(0)?)
    } else {
        Value::Json(row.try_get(0)?)
    };
    Ok(Some(Entry {
        value,
        version: row.try_get(1)?,
    }))
}

pub async fn get(pool: &PgPool, store: Store<'_>, key: &str) -> sqlx::Result<Option<Entry>> {
    fetch(&mut *pool.acquire().await?, store, key, false).await
}

/// Locks the game's usage row and returns its bytes.
async fn lock_usage(tx: &mut Transaction<'_, Postgres>, store: Store<'_>) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO datastore_usage (game, draft) VALUES ($1, $2)
         ON CONFLICT (game, draft) DO UPDATE SET bytes = datastore_usage.bytes RETURNING bytes",
    )
    .bind(store.game)
    .bind(store.draft)
    .fetch_one(&mut **tx)
    .await
}

async fn add_usage(tx: &mut Transaction<'_, Postgres>, store: Store<'_>, delta: i64) -> sqlx::Result<()> {
    if delta != 0 {
        sqlx::query("UPDATE datastore_usage SET bytes = bytes + $3 WHERE game = $1 AND draft = $2")
            .bind(store.game)
            .bind(store.draft)
            .bind(delta)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

fn entry_size(store: Store<'_>, key: &str, value: &Value) -> i64 {
    (store.name.len() + key.len()) as i64 + value.size()
}

/// What a write turns the current entry into.
enum Change {
    Put(Value),
    Delete,
    Fail(Written),
}

/// Applies `change` to the entry under the game's write lock.
async fn write(
    pool: &PgPool,
    store: Store<'_>,
    key: &str,
    quota: i64,
    change: impl FnOnce(Option<&Entry>) -> Change,
) -> sqlx::Result<(Written, Option<Entry>)> {
    let mut tx = pool.begin().await?;
    let used = lock_usage(&mut tx, store).await?;
    let old = fetch(&mut tx, store, key, true).await?;
    let old_size = old.as_ref().map_or(0, |e| entry_size(store, key, &e.value));
    let value = match change(old.as_ref()) {
        Change::Put(value) => value,
        Change::Delete => {
            if old.is_some() {
                let sql = format!(
                    "DELETE FROM {} WHERE game = $1 AND draft = $2 AND store = $3 AND key = $4",
                    store.table()
                );
                sqlx::query(&sql)
                    .bind(store.game)
                    .bind(store.draft)
                    .bind(store.name)
                    .bind(key)
                    .execute(&mut *tx)
                    .await?;
                add_usage(&mut tx, store, -old_size).await?;
            }
            tx.commit().await?;
            // Callers of a delete only look at the old entry.
            return Ok((Written::Conflict(None), old));
        }
        Change::Fail(written) => return Ok((written, old)),
    };
    let delta = entry_size(store, key, &value) - old_size;
    if delta > 0 && used + delta > quota {
        return Ok((Written::OverQuota, old));
    }
    let sql = format!(
        "INSERT INTO {} (game, draft, store, key, value) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (game, draft, store, key)
         DO UPDATE SET value = $5, version = {0}.version + 1, updated_at = now() RETURNING version",
        store.table()
    );
    let query = sqlx::query_scalar(&sql).bind(store.game).bind(store.draft).bind(store.name).bind(key);
    let version: i64 = match &value {
        Value::Json(s) => query.bind(s),
        Value::Int(n) => query.bind(n),
    }
    .fetch_one(&mut *tx)
    .await?;
    add_usage(&mut tx, store, delta).await?;
    tx.commit().await?;
    Ok((Written::Ok(Entry { value, version }), old))
}

/// Writes `value`. With `expect`, only if the entry's version is still
/// `Some(version)`, or the entry is still missing for `None`.
pub async fn set(
    pool: &PgPool,
    store: Store<'_>,
    key: &str,
    value: Value,
    expect: Option<Option<i64>>,
    quota: i64,
) -> sqlx::Result<Written> {
    let (written, _) = write(pool, store, key, quota, |old| match expect {
        Some(version) if old.map(|e| e.version) != version => Change::Fail(Written::Conflict(old.cloned())),
        _ => Change::Put(value),
    })
    .await?;
    Ok(written)
}

/// Adds `delta` to an integer entry (a missing one counts as 0).
pub async fn increment(pool: &PgPool, store: Store<'_>, key: &str, delta: i64, quota: i64) -> sqlx::Result<Written> {
    let (written, _) = write(pool, store, key, quota, |old| {
        let current = match old.map(|e| &e.value) {
            None => 0,
            Some(Value::Int(n)) => *n,
            Some(Value::Json(s)) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(serde_json::Value::Number(n)) => match n.as_i64() {
                    Some(n) => n,
                    // Integral floats, as Luau numbers always are.
                    None => match n.as_f64() {
                        Some(f) if f.fract() == 0.0 && f.abs() < 2f64.powi(53) => f as i64,
                        _ => return Change::Fail(Written::NotInteger),
                    },
                },
                _ => return Change::Fail(Written::NotInteger),
            },
        };
        let Some(next) = current.checked_add(delta) else {
            return Change::Fail(Written::NotInteger);
        };
        Change::Put(if store.ordered {
            Value::Int(next)
        } else {
            Value::Json(next.to_string())
        })
    })
    .await?;
    Ok(written)
}

/// Deletes the entry and returns what it held.
pub async fn remove(pool: &PgPool, store: Store<'_>, key: &str) -> sqlx::Result<Option<Entry>> {
    let (_, old) = write(pool, store, key, i64::MAX, |_| Change::Delete).await?;
    Ok(old)
}

/// One page of an ordered store, sorted by value then key.
pub struct SortedQuery<'a> {
    pub ascending: bool,
    pub limit: i64,
    /// Start after this `(value, key)`: the last entry of the previous page.
    pub after: Option<(i64, &'a str)>,
    pub min: Option<i64>,
    pub max: Option<i64>,
}

pub async fn sorted(pool: &PgPool, store: Store<'_>, query: SortedQuery<'_>) -> sqlx::Result<Vec<(String, i64)>> {
    let (cmp, order) = if query.ascending { (">", "ASC") } else { ("<", "DESC") };
    let sql = format!(
        "SELECT key, value FROM ordered_datastore_entries
         WHERE game = $1 AND draft = $2 AND store = $3
           AND ($4::bigint IS NULL OR value >= $4) AND ($5::bigint IS NULL OR value <= $5)
           AND ($6::bigint IS NULL OR (value, key) {cmp} ($6, $7::text))
         ORDER BY value {order}, key {order} LIMIT $8"
    );
    sqlx::query_as(&sql)
        .bind(store.game)
        .bind(store.draft)
        .bind(store.name)
        .bind(query.min)
        .bind(query.max)
        .bind(query.after.map(|a| a.0))
        .bind(query.after.map(|a| a.1))
        .bind(query.limit)
        .fetch_all(pool)
        .await
}

/// Bytes a game's stores use.
pub async fn usage(pool: &PgPool, game: i64, draft: bool) -> sqlx::Result<i64> {
    let bytes = sqlx::query_scalar("SELECT bytes FROM datastore_usage WHERE game = $1 AND draft = $2")
        .bind(game)
        .bind(draft)
        .fetch_optional(pool)
        .await?;
    Ok(bytes.unwrap_or(0))
}
