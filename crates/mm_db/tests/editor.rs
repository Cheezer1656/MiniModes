//! Runs against `MINIMODES_TEST_DATABASE_URL` (a disposable database); skipped when unset.

use mm_db::NewGame;
use mm_db::editor::{self, AddOutcome, RenameOutcome, Role, SaveOutcome};
use mm_proto::GameKind;
use uuid::Uuid;

async fn pool() -> Option<mm_db::Pool> {
    let url = std::env::var("MINIMODES_TEST_DATABASE_URL").ok()?;
    Some(mm_db::connect(&url, 2).await.expect("connecting to test database"))
}

async fn player(pool: &mm_db::Pool, name: &str) -> Uuid {
    let uuid = Uuid::new_v4();
    mm_db::player_joined(pool, uuid, name).await.unwrap();
    uuid
}

async fn game(pool: &mm_db::Pool, owner: Uuid) -> i64 {
    let game = NewGame {
        owner,
        name: "Editor test",
        icon: "stone",
        kind: GameKind::Match,
        max_players: 8,
        map_size: 128,
    };
    mm_db::create_game(pool, game, 10).await.unwrap().unwrap().id
}

fn unique(prefix: &str) -> String {
    format!("{prefix}{}", &Uuid::new_v4().simple().to_string()[..10])
}

#[tokio::test]
async fn links_are_single_use_and_sessions_resolve() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner = player(&pool, &unique("own")).await;
    let id = game(&pool, owner).await;
    let (secret, hash) = editor::new_secret();
    assert_eq!(hash, editor::hash_secret(&secret));
    editor::create_token(&pool, &hash, owner, id).await.unwrap();
    assert_eq!(editor::redeem_token(&pool, &hash).await.unwrap(), Some((owner, id)));
    assert_eq!(editor::redeem_token(&pool, &hash).await.unwrap(), None, "used twice");

    let (_, session) = editor::new_secret();
    editor::create_session(&pool, &session, owner).await.unwrap();
    assert_eq!(editor::session(&pool, &session).await.unwrap().map(|s| s.0), Some(owner));
    editor::delete_session(&pool, &session).await.unwrap();
    assert_eq!(editor::session(&pool, &session).await.unwrap(), None);
}

#[tokio::test]
async fn saves_check_versions_limits_and_keep_history() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner = player(&pool, &unique("own")).await;
    let id = game(&pool, owner).await;
    let save = |path: &'static str, source: String, base| {
        let pool = pool.clone();
        async move { editor::save_file(&pool, id, path, &source, owner, base).await.unwrap() }
    };

    assert_eq!(save("main.luau", "print(1)".into(), Some(0)).await, SaveOutcome::Saved { version: 1 });
    assert!(matches!(save("main.luau", "x".into(), Some(0)).await, SaveOutcome::Conflict { version: 1, .. }));
    assert_eq!(save("main.luau", "print(2)".into(), Some(1)).await, SaveOutcome::Saved { version: 2 });
    assert_eq!(save("main.luau", "print(3)".into(), None).await, SaveOutcome::Saved { version: 3 });
    let file = editor::file(&pool, id, "main.luau").await.unwrap().unwrap();
    assert_eq!((file.source.as_str(), file.version), ("print(3)", 3));

    let big = "-".repeat(mm_proto::scripts::MAX_TOTAL_BYTES);
    assert!(matches!(save("big.luau", big, None).await, SaveOutcome::TooLarge { .. }));
    assert!(editor::file(&pool, id, "big.luau").await.unwrap().is_none());

    let history = editor::history(&pool, id, "main.luau").await.unwrap();
    assert_eq!(history.len(), 3);
    let (path, source) = editor::revision(&pool, id, history[2].id).await.unwrap().unwrap();
    assert_eq!((path.as_str(), source.as_deref()), ("main.luau", Some("print(1)")));

    for i in 0..editor::HISTORY_PER_FILE + 5 {
        save("main.luau", format!("print({i})"), None).await;
    }
    let history = editor::history(&pool, id, "main.luau").await.unwrap();
    assert_eq!(history.len() as i64, editor::HISTORY_PER_FILE, "history is pruned");

    save("a.luau", "return 1".into(), None).await;
    assert_eq!(editor::rename_file(&pool, id, "a.luau", "main.luau", owner).await.unwrap(), RenameOutcome::Exists);
    assert_eq!(editor::rename_file(&pool, id, "a.luau", "lib/a.luau", owner).await.unwrap(), RenameOutcome::Renamed);
    assert_eq!(editor::rename_file(&pool, id, "a.luau", "b.luau", owner).await.unwrap(), RenameOutcome::Missing);
    assert!(editor::delete_file(&pool, id, "lib/a.luau", owner).await.unwrap());
    assert!(!editor::delete_file(&pool, id, "lib/a.luau", owner).await.unwrap());
    let deleted = editor::history(&pool, id, "lib/a.luau").await.unwrap();
    assert!(deleted[0].size.is_none(), "the delete is recorded");
    let paths: Vec<_> = editor::list_files(&pool, id).await.unwrap().into_iter().map(|f| f.path).collect();
    assert_eq!(paths, ["main.luau"]);
}

#[tokio::test]
async fn collaborators_get_access_until_removed() {
    let Some(pool) = pool().await else {
        eprintln!("MINIMODES_TEST_DATABASE_URL unset; skipping");
        return;
    };
    let owner_name = unique("own");
    let owner = player(&pool, &owner_name).await;
    let helper_name = unique("Help");
    let helper = player(&pool, &helper_name).await;
    let stranger = player(&pool, &unique("str")).await;
    let id = game(&pool, owner).await;

    assert_eq!(editor::role(&pool, id, owner).await.unwrap(), Some(Role::Owner));
    assert_eq!(editor::role(&pool, id, helper).await.unwrap(), None);
    let added = editor::add_collaborator(&pool, id, &helper_name.to_lowercase(), owner).await.unwrap();
    assert!(matches!(&added, AddOutcome::Added(m) if m.uuid == helper && m.username == helper_name));
    assert_eq!(editor::add_collaborator(&pool, id, &helper_name, owner).await.unwrap(), AddOutcome::Already);
    assert_eq!(editor::add_collaborator(&pool, id, &owner_name, owner).await.unwrap(), AddOutcome::IsOwner);
    assert_eq!(editor::add_collaborator(&pool, id, "no-such-player-x", owner).await.unwrap(), AddOutcome::UnknownPlayer);
    assert_eq!(editor::role(&pool, id, helper).await.unwrap(), Some(Role::Collaborator));
    assert_eq!(editor::role(&pool, id, stranger).await.unwrap(), None);
    assert_eq!(editor::shared_games(&pool, helper).await.unwrap().iter().map(|g| g.id).collect::<Vec<_>>(), [id]);

    for i in 1..editor::MAX_COLLABORATORS {
        let name = unique(&format!("f{i}"));
        player(&pool, &name).await;
        assert!(matches!(editor::add_collaborator(&pool, id, &name, owner).await.unwrap(), AddOutcome::Added(_)));
    }
    let extra = unique("x");
    player(&pool, &extra).await;
    assert_eq!(editor::add_collaborator(&pool, id, &extra, owner).await.unwrap(), AddOutcome::Full);
    assert_eq!(editor::collaborators(&pool, id).await.unwrap().len() as i64, editor::MAX_COLLABORATORS);

    assert!(editor::remove_collaborator(&pool, id, helper).await.unwrap());
    assert_eq!(editor::role(&pool, id, helper).await.unwrap(), None);
    assert!(!editor::remove_collaborator(&pool, id, helper).await.unwrap());
}
