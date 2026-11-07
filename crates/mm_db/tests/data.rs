//! Runs against `MINIMODES_TEST_DATABASE_URL` (a disposable database); skipped when unset.

use mm_db::NewGame;
use mm_db::datastore::{self, Entry, SortedQuery, Store, Value, Written};
use mm_db::library;
use mm_db::publish::{self, PublishOutcome};
use mm_proto::{GameKind, LiveUpdate};
use uuid::Uuid;

async fn pool() -> Option<mm_db::Pool> {
    let url = std::env::var("MINIMODES_TEST_DATABASE_URL").ok()?;
    Some(mm_db::connect(&url, 4).await.expect("connecting to test database"))
}

async fn player(pool: &mm_db::Pool) -> Uuid {
    let uuid = Uuid::new_v4();
    mm_db::player_joined(pool, uuid, &uuid.simple().to_string()[..12]).await.unwrap();
    uuid
}

async fn game(pool: &mm_db::Pool, owner: Uuid) -> i64 {
    let game = NewGame {
        owner,
        name: "Data test",
        icon: "stone",
        kind: GameKind::Match,
        max_players: 8,
        map_size: 128,
    };
    mm_db::create_game(pool, game, 10).await.unwrap().unwrap().id
}

fn store(game: i64, draft: bool, ordered: bool) -> Store<'static> {
    Store {
        game,
        draft,
        ordered,
        name: "s",
    }
}

fn json(s: &str) -> Value {
    Value::Json(s.to_owned())
}

#[tokio::test]
async fn datastore_sets_compares_and_counts_usage() {
    let Some(pool) = pool().await else {
        return;
    };
    let id = game(&pool, player(&pool).await).await;
    let live = store(id, false, false);
    assert_eq!(datastore::get(&pool, live, "k").await.unwrap(), None);

    let w = datastore::set(&pool, live, "k", json(r#"{"a":1}"#), None, 1000).await.unwrap();
    let Written::Ok(Entry { version: v1, .. }) = w else { panic!("{w:?}") };
    // The draft side is separate.
    assert_eq!(datastore::get(&pool, store(id, true, false), "k").await.unwrap(), None);
    assert_eq!(datastore::usage(&pool, id, false).await.unwrap(), 1 + 1 + 7);

    // Compare-and-set: a stale version or "must be missing" is refused.
    let stale = datastore::set(&pool, live, "k", json("2"), Some(Some(v1 - 1)), 1000).await.unwrap();
    assert!(matches!(&stale, Written::Conflict(Some(e)) if e.version == v1), "{stale:?}");
    let missing = datastore::set(&pool, live, "k", json("2"), Some(None), 1000).await.unwrap();
    assert!(matches!(missing, Written::Conflict(Some(_))));
    let ok = datastore::set(&pool, live, "k", json("2"), Some(Some(v1)), 1000).await.unwrap();
    assert!(matches!(&ok, Written::Ok(e) if e.version == v1 + 1), "{ok:?}");
    assert_eq!(datastore::usage(&pool, id, false).await.unwrap(), 3);

    // Increments start from 0 and need a whole number.
    let inc = datastore::increment(&pool, live, "n", 5, 1000).await.unwrap();
    assert!(matches!(&inc, Written::Ok(e) if e.value == json("5")), "{inc:?}");
    datastore::set(&pool, live, "f", json("1.5"), None, 1000).await.unwrap();
    assert_eq!(datastore::increment(&pool, live, "f", 1, 1000).await.unwrap(), Written::NotInteger);

    // The quota counts store + key + value bytes.
    let used = datastore::usage(&pool, id, false).await.unwrap();
    let big = json(&format!("\"{}\"", "x".repeat(100)));
    assert_eq!(datastore::set(&pool, live, "big", big.clone(), None, used + 50).await.unwrap(), Written::OverQuota);
    assert!(matches!(datastore::set(&pool, live, "big", big, None, used + 200).await.unwrap(), Written::Ok(_)));

    let removed = datastore::remove(&pool, live, "big").await.unwrap();
    assert!(removed.is_some());
    assert_eq!(datastore::usage(&pool, id, false).await.unwrap(), used);
    assert_eq!(datastore::remove(&pool, live, "big").await.unwrap(), None);
}

#[tokio::test]
async fn concurrent_increments_all_land() {
    let Some(pool) = pool().await else {
        return;
    };
    let id = game(&pool, player(&pool).await).await;
    let s = store(id, false, true);
    let tasks: Vec<_> = (0..20)
        .map(|_| {
            let pool = pool.clone();
            tokio::spawn(async move { datastore::increment(&pool, s, "hits", 1, 1 << 20).await.unwrap() })
        })
        .collect();
    for t in tasks {
        assert!(matches!(t.await.unwrap(), Written::Ok(_)));
    }
    let entry = datastore::get(&pool, s, "hits").await.unwrap().unwrap();
    assert_eq!(entry.value, Value::Int(20));
}

#[tokio::test]
async fn ordered_stores_page_in_value_order() {
    let Some(pool) = pool().await else {
        return;
    };
    let id = game(&pool, player(&pool).await).await;
    let s = store(id, false, true);
    for (key, value) in [("a", 30), ("b", 10), ("c", 30), ("d", 20), ("e", 50)] {
        datastore::set(&pool, s, key, Value::Int(value), None, 1 << 20).await.unwrap();
    }
    let page = |after: Option<(i64, &'static str)>, ascending| SortedQuery {
        ascending,
        limit: 2,
        after,
        min: None,
        max: None,
    };
    let first = datastore::sorted(&pool, s, page(None, false)).await.unwrap();
    assert_eq!(first, [("e".to_owned(), 50), ("c".to_owned(), 30)]);
    let second = datastore::sorted(&pool, s, page(Some((30, "c")), false)).await.unwrap();
    assert_eq!(second, [("a".to_owned(), 30), ("d".to_owned(), 20)]);
    let up = datastore::sorted(&pool, s, page(Some((20, "d")), true)).await.unwrap();
    assert_eq!(up, [("a".to_owned(), 30), ("c".to_owned(), 30)]);
    let ranged = SortedQuery {
        ascending: true,
        limit: 10,
        after: None,
        min: Some(15),
        max: Some(30),
    };
    let keys: Vec<_> = datastore::sorted(&pool, s, ranged).await.unwrap().into_iter().map(|e| e.0).collect();
    assert_eq!(keys, ["d", "a", "c"]);
}

#[tokio::test]
async fn library_tracks_favorites_plays_and_ratings() {
    let Some(pool) = pool().await else {
        return;
    };
    let owner = player(&pool).await;
    let id = game(&pool, owner).await;
    let PublishOutcome::Published(_) = publish::publish(&pool, id, owner, LiveUpdate::Reset).await.unwrap() else {
        panic!("publish refused");
    };
    let fan = player(&pool).await;

    library::set_favorite(&pool, fan, id, true).await.unwrap();
    library::set_favorite(&pool, fan, id, true).await.unwrap();
    assert_eq!(library::library(&pool, fan).await.unwrap().favorites, [id]);

    assert_eq!(library::rate(&pool, fan, id, 4).await.unwrap(), None, "rated without playing");
    assert!(library::record_play(&pool, fan, id).await.unwrap());
    assert!(!library::record_play(&pool, fan, id).await.unwrap());
    let rating = library::rate(&pool, fan, id, 4).await.unwrap().unwrap();
    assert_eq!((rating.mean, rating.count), (Some(4.0), 1));
    let rating = library::rate(&pool, fan, id, 2).await.unwrap().unwrap();
    assert_eq!((rating.mean, rating.count), (Some(2.0), 1), "re-rating replaces");

    let lib = library::library(&pool, fan).await.unwrap();
    assert_eq!(lib.played, [id]);
    assert_eq!(lib.ratings, [(id, 2)]);
    let summary = mm_db::game(&pool, id).await.unwrap().unwrap().summary();
    assert_eq!((summary.rating, summary.ratings, summary.plays), (Some(2.0), 1, 1));

    library::set_favorite(&pool, fan, id, false).await.unwrap();
    assert!(library::library(&pool, fan).await.unwrap().favorites.is_empty());
}
