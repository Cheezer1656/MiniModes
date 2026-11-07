//! Runs against `MINIMODES_TEST_DATABASE_URL` (a disposable database); skipped when unset.

use mm_db::NewGame;
use mm_db::editor;
use mm_db::moderation::{self, NewReport, ReportOutcome, TextLine};
use mm_db::publish::{self, PublishOutcome};
use mm_proto::{GameKind, Hold, LiveUpdate};
use uuid::Uuid;

async fn pool() -> Option<mm_db::Pool> {
    let url = std::env::var("MINIMODES_TEST_DATABASE_URL").ok()?;
    Some(mm_db::connect(&url, 8).await.expect("connecting to test database"))
}

async fn player(pool: &mm_db::Pool) -> Uuid {
    let uuid = Uuid::new_v4();
    mm_db::player_joined(pool, uuid, &uuid.simple().to_string()[..12]).await.unwrap();
    uuid
}

async fn published_game(pool: &mm_db::Pool, owner: Uuid) -> i64 {
    let game = NewGame {
        owner,
        name: "Moderation test",
        icon: "stone",
        kind: GameKind::Match,
        max_players: 8,
        map_size: 128,
    };
    let id = mm_db::create_game(pool, game, 10).await.unwrap().unwrap().id;
    assert!(matches!(
        publish::publish(pool, id, owner, LiveUpdate::Scripts).await.unwrap(),
        PublishOutcome::Published(_)
    ));
    id
}

fn listed(rows: &[mm_db::GameRow], id: i64) -> bool {
    rows.iter().any(|g| g.id == id)
}

#[tokio::test]
async fn reports_hold_a_game_at_the_threshold_once() {
    let Some(pool) = pool().await else { return };
    let owner = player(&pool).await;
    let id = published_game(&pool, owner).await;
    let report = |reporter, reason| NewReport {
        game: id,
        reporter,
        reason,
        version: Some(1),
    };

    assert_eq!(moderation::report(&pool, report(owner, "mine"), 3).await.unwrap(), ReportOutcome::OwnGame);
    let first = player(&pool).await;
    assert_eq!(
        moderation::report(&pool, report(first, "spam"), 3).await.unwrap(),
        ReportOutcome::Filed { open: 1, held: false }
    );
    assert_eq!(moderation::report(&pool, report(first, "again"), 3).await.unwrap(), ReportOutcome::Duplicate);

    // The rest arrive at once; exactly one of them crosses the threshold.
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let (pool, reporter) = (pool.clone(), player(&pool).await);
        tasks.push(tokio::spawn(async move {
            let r = NewReport {
                game: id,
                reporter,
                reason: "bad",
                version: None,
            };
            moderation::report(&pool, r, 3).await.unwrap()
        }));
    }
    let mut held = 0;
    for t in tasks {
        if let ReportOutcome::Filed { held: true, open } = t.await.unwrap() {
            assert_eq!(open, 3);
            held += 1;
        }
    }
    assert_eq!(held, 1);
    assert!(!listed(&mm_db::listed_games(&pool).await.unwrap(), id));
    assert_eq!(mm_db::game(&pool, id).await.unwrap().unwrap().hold(), Some(Hold::Review));

    let queue = moderation::queue(&pool, 500).await.unwrap();
    let entry = queue.iter().find(|q| q.game.id == id).expect("in the queue");
    assert_eq!(entry.open_reports, 5);
    assert_eq!(entry.hold_reason.as_deref(), Some("3 reports"));
    assert_eq!(moderation::open_reports(&pool, id, 2).await.unwrap().len(), 2);

    // Clearing resolves the reports and relists; the same players may report again.
    let moderator = player(&pool).await;
    let row = moderation::clear(&pool, id, moderator).await.unwrap().unwrap();
    assert_eq!(row.hold(), None);
    assert!(listed(&mm_db::listed_games(&pool).await.unwrap(), id));
    assert!(!moderation::queue(&pool, 500).await.unwrap().iter().any(|q| q.game.id == id));
    assert_eq!(
        moderation::report(&pool, report(first, "back"), 3).await.unwrap(),
        ReportOutcome::Filed { open: 1, held: false }
    );
}

#[tokio::test]
async fn removed_games_cant_be_republished() {
    let Some(pool) = pool().await else { return };
    let owner = player(&pool).await;
    let moderator = player(&pool).await;
    let id = published_game(&pool, owner).await;
    assert!(moderation::set_hold(&pool, id, Hold::Removed, "malware", moderator).await.unwrap());
    assert!(!listed(&mm_db::listed_games(&pool).await.unwrap(), id));
    // Removed games aren't in the queue either.
    assert!(!moderation::queue(&pool, 500).await.unwrap().iter().any(|q| q.game.id == id));
    let entry = moderation::queue_entry(&pool, id).await.unwrap().unwrap();
    assert_eq!((entry.game.hold(), entry.hold_reason.as_deref()), (Some(Hold::Removed), Some("malware")));
    assert!(matches!(
        publish::publish(&pool, id, owner, LiveUpdate::Scripts).await.unwrap(),
        PublishOutcome::Removed
    ));

    // A game on review may publish fixes but stays unlisted.
    moderation::set_hold(&pool, id, Hold::Review, "check it", moderator).await.unwrap();
    assert!(matches!(
        publish::publish(&pool, id, owner, LiveUpdate::Scripts).await.unwrap(),
        PublishOutcome::Published(_)
    ));
    assert!(!listed(&mm_db::listed_games(&pool).await.unwrap(), id));
}

#[tokio::test]
async fn creator_bans_hold_games_and_lock_the_draft() {
    let Some(pool) = pool().await else { return };
    let owner = player(&pool).await;
    let admin = player(&pool).await;
    let id = published_game(&pool, owner).await;
    assert_eq!(editor::role(&pool, id, owner).await.unwrap(), Some(editor::Role::Owner));

    let games = moderation::ban_creator(&pool, owner, "griefing", None, admin).await.unwrap();
    assert_eq!(games, vec![id]);
    assert_eq!(
        moderation::creator_ban(&pool, owner).await.unwrap(),
        Some(moderation::Ban {
            reason: "griefing".into(),
            until: None
        })
    );
    assert_eq!(editor::role(&pool, id, owner).await.unwrap(), None);
    assert!(matches!(
        publish::publish(&pool, id, owner, LiveUpdate::Scripts).await.unwrap(),
        PublishOutcome::Banned
    ));
    let row = mm_db::game(&pool, id).await.unwrap().unwrap();
    assert_eq!(row.hold(), Some(Hold::Review));

    assert!(moderation::unban_creator(&pool, owner, admin).await.unwrap());
    assert!(!moderation::unban_creator(&pool, owner, admin).await.unwrap());
    assert_eq!(editor::role(&pool, id, owner).await.unwrap(), Some(editor::Role::Owner));

    // A timed ban expires on its own.
    moderation::ban_creator(&pool, owner, "brief", Some(0), admin).await.unwrap();
    assert_eq!(moderation::creator_ban(&pool, owner).await.unwrap(), None);
    moderation::ban_creator(&pool, owner, "week", Some(7), admin).await.unwrap();
    let ban = moderation::creator_ban(&pool, owner).await.unwrap().unwrap();
    assert!(ban.until.is_some());
}

#[tokio::test]
async fn ranks_and_lookup() {
    let Some(pool) = pool().await else { return };
    let p = player(&pool).await;
    let (found, username) = moderation::find_player(&pool, &p.simple().to_string()[..12].to_uppercase())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found, p);
    assert_eq!(username, p.simple().to_string()[..12]);
    assert_eq!(moderation::rank(&pool, p).await.unwrap(), None);
    moderation::set_rank(&pool, p, Some("admin"), None).await.unwrap();
    assert_eq!(mm_db::player_joined(&pool, p, &username).await.unwrap().as_deref(), Some("admin"));
    let id = published_game(&pool, p).await;
    assert_eq!(moderation::owner_rank(&pool, id).await.unwrap().as_deref(), Some("admin"));
    moderation::set_rank(&pool, p, None, None).await.unwrap();
    assert_eq!(moderation::rank(&pool, p).await.unwrap(), None);
}

#[tokio::test]
async fn text_log_round_trip() {
    let Some(pool) = pool().await else { return };
    moderation::maintain_text_log(&pool, 30).await.unwrap();
    let owner = player(&pool).await;
    let id = published_game(&pool, owner).await;
    let line = |kind, text: &str, player| TextLine {
        game: id,
        version: Some(1),
        instance: 9,
        player,
        kind,
        text: text.to_owned(),
    };
    moderation::write_text_log(&pool, &[line("chat", "hello", Some(owner)), line("title", "Round 1", None)])
        .await
        .unwrap();
    let log = moderation::text_log(&pool, id, 10).await.unwrap();
    assert_eq!(log.len(), 2);
    let chat = log.iter().find(|l| l.kind == "chat").unwrap();
    assert_eq!(chat.text, "hello");
    assert_eq!(chat.player.as_deref(), Some(&owner.simple().to_string()[..12]));
    assert!(log.iter().any(|l| l.kind == "title" && l.player.is_none()));
}
