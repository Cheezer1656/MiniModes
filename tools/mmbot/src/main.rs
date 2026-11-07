//! Integration-test client.
//!
//! - `mmbot join <addr> [count] [hold_secs]`: join `count` bots and hold them.
//! - `mmbot lobby-ui <addr>`: drive the lobby hotbar, menus, search, NPCs and chat.
//! - `mmbot handoff <lobby_addr> [gate_api_bind]`: lobby → shard → lobby against
//!   the real lobby and gamehost, with a mock Gate API (see `mock_gate`).
//! - `mmbot drafts <lobby_addr> [gate_api_bind]`: create a game, build in its
//!   draft, and check the build survives an unload (same setup, plus Postgres).
//! - `mmbot scripts <lobby_addr> [gate_api_bind]`: run Luau scripts in a draft:
//!   events, chat, rules, reload, and a runaway script being stopped.
//! - `mmbot editor <lobby_addr> [gate_api_bind]`: the web editor: login link,
//!   file API, hot reload, console stream, history and collaborators.
//! - `mmbot editor-link <game_id> [editor_url]`: print a sign-in link to the
//!   web editor as the game's owner, without joining (needs Postgres).

mod mock_gate;
mod scenarios;

use std::net::SocketAddr;
use std::time::Duration;

use mmbot::{Bot, BotEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "join".into());
    if mode == "editor-link" {
        let game: i64 = args.next().unwrap_or_default().parse()?;
        let base = args.next().unwrap_or_else(|| "http://127.0.0.1:8081".into());
        return editor_link(game, &base).await;
    }
    let addr: SocketAddr = args.next().unwrap_or_else(|| "127.0.0.1:25577".into()).parse()?;
    match mode.as_str() {
        "join" => {
            let count: usize = args.next().map_or(Ok(1), |s| s.parse())?;
            let hold = Duration::from_secs(args.next().map_or(Ok(5), |s| s.parse())?);
            join(addr, count, hold).await
        }
        "lobby-ui" => scenarios::lobby_ui(addr).await,
        "handoff" => {
            let gate: SocketAddr = args.next().unwrap_or_else(|| "127.0.0.1:8090".into()).parse()?;
            scenarios::handoff(addr, gate).await
        }
        "drafts" => {
            let gate: SocketAddr = args.next().unwrap_or_else(|| "127.0.0.1:8090".into()).parse()?;
            scenarios::drafts(addr, gate).await
        }
        "scripts" => {
            let gate: SocketAddr = args.next().unwrap_or_else(|| "127.0.0.1:8090".into()).parse()?;
            scenarios::scripts(addr, gate).await
        }
        "editor" => {
            let gate: SocketAddr = args.next().unwrap_or_else(|| "127.0.0.1:8090".into()).parse()?;
            scenarios::editor(addr, gate).await
        }
        other => anyhow::bail!("unknown mode `{other}` (expected join | lobby-ui | handoff | drafts | scripts | editor)"),
    }
}

async fn editor_link(game: i64, base: &str) -> anyhow::Result<()> {
    let db = mm_db::connect(&mm_db::url_from_env()?, 1).await?;
    let owner = mm_db::game(&db, game).await?.ok_or_else(|| anyhow::anyhow!("no game {game}"))?.owner;
    let (secret, hash) = mm_db::editor::new_secret();
    mm_db::editor::create_token(&db, &hash, owner, game).await?;
    println!("{}/edit?t={secret}", base.trim_end_matches('/'));
    Ok(())
}

async fn join(addr: SocketAddr, count: usize, hold: Duration) -> anyhow::Result<()> {
    let mut tasks = Vec::new();
    for i in 0..count {
        tasks.push(tokio::spawn(async move {
            let name = format!("bot{i}");
            let mut bot = Bot::connect(addr, &name).await?;
            let spawned = tokio::time::timeout(
                Duration::from_secs(15),
                bot.wait_for(|e| matches!(e, BotEvent::Teleported(_))),
            )
            .await??;
            println!("{name}: {spawned:?}");

            tokio::time::timeout(hold, async {
                loop {
                    match bot.next_event().await? {
                        BotEvent::Disconnected(reason) => anyhow::bail!("disconnected: {reason}"),
                        BotEvent::Chat(msg) => println!("{name}: chat: {msg}"),
                        _ => {}
                    }
                }
            })
            .await
            .unwrap_or(Ok(()))?;
            anyhow::Ok(())
        }));
    }

    let mut failed = 0;
    for task in tasks {
        if let Err(e) = task.await? {
            eprintln!("bot failed: {e:#}");
            failed += 1;
        }
    }
    anyhow::ensure!(failed == 0, "{failed}/{count} bots failed");
    println!("all {count} bots ok");
    Ok(())
}
