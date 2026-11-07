//! Runs against `MINIMODES_TEST_DATABASE_URL` (a disposable database); skipped when unset.

use mm_db::NewGame;
use mm_proto::GameKind;
use uuid::Uuid;

async fn pool() -> Option<mm_db::Pool> {
    let url = std::env::var("MINIMODES_TEST_DATABASE_URL").ok()?;
    Some(mm_db::connect(&url, 2).await.expect("connecting to test database"))
}

#[tokio::test]
async fn games_lifecycle() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner = Uuid::new_v4();
    assert_eq!(mm_db::player_joined(&pool, owner, "builder").await.unwrap(), None);
    // Rejoining with a new name updates it.
    mm_db::player_joined(&pool, owner, "builder2").await.unwrap();

    let new = |name| NewGame {
        owner,
        name,
        icon: "grass_block",
        kind: GameKind::OpenWorld {
            persistent_world: true,
        },
        max_players: 20,
        map_size: 256,
    };
    let a = mm_db::create_game(&pool, new("A"), 2).await.unwrap().unwrap();
    assert_eq!(a.owner_name, "builder2");
    assert_eq!(a.kind(), GameKind::OpenWorld { persistent_world: true });
    assert!(a.draft_map_hash.is_none());
    mm_db::create_game(&pool, new("B"), 2).await.unwrap().unwrap();
    assert!(mm_db::create_game(&pool, new("C"), 2).await.unwrap().is_none(), "quota");

    let owned = mm_db::owned_games(&pool, owner).await.unwrap();
    assert_eq!(owned.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["B", "A"]);

    mm_db::set_draft_map(&pool, a.id, "abc").await.unwrap();
    let a = mm_db::game(&pool, a.id).await.unwrap().unwrap();
    assert_eq!(a.draft_map_hash.as_deref(), Some("abc"));
}
