//! Runs against `MINIMODES_TEST_DATABASE_URL` (a disposable database); skipped when unset.

use mm_db::NewGame;
use mm_db::publish::{self, PublishOutcome};
use mm_proto::{GameKind, LiveUpdate};
use uuid::Uuid;

async fn pool() -> Option<mm_db::Pool> {
    let url = std::env::var("MINIMODES_TEST_DATABASE_URL").ok()?;
    Some(mm_db::connect(&url, 2).await.expect("connecting to test database"))
}

async fn player(pool: &mm_db::Pool) -> Uuid {
    let uuid = Uuid::new_v4();
    mm_db::player_joined(pool, uuid, &uuid.simple().to_string()[..12]).await.unwrap();
    uuid
}

async fn game(pool: &mm_db::Pool, owner: Uuid, kind: GameKind) -> i64 {
    let game = NewGame {
        owner,
        name: "Publish test",
        icon: "stone",
        kind,
        max_players: 8,
        map_size: 128,
    };
    mm_db::create_game(pool, game, 10).await.unwrap().unwrap().id
}

async fn publish_ok(pool: &mm_db::Pool, game: i64, owner: Uuid, mode: LiveUpdate) -> u32 {
    match publish::publish(pool, game, owner, mode).await.unwrap() {
        PublishOutcome::Published(row) => row.published_version.unwrap() as u32,
        _ => panic!("publish refused"),
    }
}

#[tokio::test]
async fn versions_snapshot_the_draft_and_list_the_game() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner = player(&pool).await;
    let id = game(&pool, owner, GameKind::Match).await;
    assert!(!mm_db::listed_games(&pool).await.unwrap().iter().any(|g| g.id == id));

    let stranger = player(&pool).await;
    assert!(matches!(
        publish::publish(&pool, id, stranger, LiveUpdate::Reset).await.unwrap(),
        PublishOutcome::NotOwner
    ));

    mm_db::put_draft_file(&pool, id, "main.luau", "print(1)", None).await.unwrap();
    assert_eq!(publish_ok(&pool, id, owner, LiveUpdate::Reset).await, 1);
    mm_db::put_draft_file(&pool, id, "main.luau", "print(2)", None).await.unwrap();
    mm_db::set_draft_map(&pool, id, "ab").await.unwrap();
    assert_eq!(publish_ok(&pool, id, owner, LiveUpdate::Reset).await, 2);

    let v1 = publish::version(&pool, id, 1).await.unwrap().unwrap();
    assert_eq!((v1.map_hash, v1.files), (None, vec![("main.luau".into(), "print(1)".into())]));
    let v2 = publish::version(&pool, id, 2).await.unwrap().unwrap();
    assert_eq!(v2.map_hash.as_deref(), Some("ab"));
    assert!(publish::version(&pool, id, 3).await.unwrap().is_none());

    let listed = mm_db::listed_games(&pool).await.unwrap();
    let row = listed.iter().find(|g| g.id == id).expect("listed after publishing");
    assert_eq!(row.summary().version, 2);
}

#[tokio::test]
async fn live_worlds_apply_pending_publishes_once() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner = player(&pool).await;
    let kind = GameKind::OpenWorld {
        persistent_world: true,
    };
    let id = game(&pool, owner, kind).await;
    assert!(publish::claim_live_world(&pool, id).await.unwrap().is_none(), "unpublished");

    mm_db::set_draft_map(&pool, id, "v1map").await.unwrap();
    publish_ok(&pool, id, owner, LiveUpdate::Reset).await;
    let live = publish::claim_live_world(&pool, id).await.unwrap().unwrap();
    assert_eq!((live.version, live.map_hash.as_deref(), live.update), (1, Some("v1map"), None));

    // Players changed the world; it unloaded.
    publish::save_live_world(&pool, id, "played", true).await.unwrap();

    // Scripts-only, then regions: regions wins; the region list comes from the draft.
    assert!(publish::set_publish_region(&pool, id, "mine", ([0, 60, 0], [9, 70, 9])).await.unwrap());
    mm_db::set_draft_map(&pool, id, "v3map").await.unwrap();
    publish_ok(&pool, id, owner, LiveUpdate::Scripts).await;
    publish_ok(&pool, id, owner, LiveUpdate::Regions).await;
    let live = publish::claim_live_world(&pool, id).await.unwrap().unwrap();
    assert_eq!(live.version, 3);
    assert_eq!(live.update, Some(LiveUpdate::Regions));
    assert_eq!(live.map_hash.as_deref(), Some("played"), "regions keep the world");
    assert_eq!(live.version_map.as_deref(), Some("v3map"));
    assert_eq!(live.regions, [([0, 60, 0], [9, 70, 9])]);
    assert!(live.last_shutdown.is_some());

    // Claimed once only.
    let again = publish::claim_live_world(&pool, id).await.unwrap().unwrap();
    assert_eq!((again.version, again.update), (3, None));

    // A reset backs the world up and points it at the new map.
    mm_db::set_draft_map(&pool, id, "v4map").await.unwrap();
    publish_ok(&pool, id, owner, LiveUpdate::Reset).await;
    let live = publish::claim_live_world(&pool, id).await.unwrap().unwrap();
    assert_eq!((live.version, live.update, live.map_hash.as_deref()), (4, Some(LiveUpdate::Reset), Some("v4map")));
    assert_eq!(publish::live_world_backups(&pool, id).await.unwrap(), [(3, Some("played".to_owned()))]);

    assert!(publish::delete_publish_region(&pool, id, "mine").await.unwrap());
    assert!(!publish::delete_publish_region(&pool, id, "mine").await.unwrap());
}

#[tokio::test]
async fn player_state_and_settings() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner = player(&pool).await;
    let id = game(&pool, owner, GameKind::OpenWorld { persistent_world: false }).await;
    assert!(publish::player_state(&pool, id, owner).await.unwrap().is_none());
    publish::save_player_state(&pool, id, owner, b"one").await.unwrap();
    publish::save_player_state(&pool, id, owner, b"two").await.unwrap();
    assert_eq!(publish::player_state(&pool, id, owner).await.unwrap().as_deref(), Some(&b"two"[..]));

    assert!(mm_db::game(&pool, id).await.unwrap().unwrap().save_player_state, "on by default");
    mm_db::set_game_setting(&pool, id, "save_player_state", serde_json::Value::Bool(false)).await.unwrap();
    assert!(!mm_db::game(&pool, id).await.unwrap().unwrap().save_player_state);
}
