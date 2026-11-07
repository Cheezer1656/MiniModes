//! Integration-test client.
//!
//! - `mmbot join <addr> [count] [hold_secs]`: join `count` bots and hold them.
//! - `mmbot lobby-ui <addr>`: drive the lobby hotbar, menus, search, NPCs and chat.
//! - `mmbot handoff <lobby_addr> [gate_api_bind]`: lobby → shard → lobby against
//!   the real lobby and gamehost, with a mock Gate API (see `mock_gate`).
//! - `mmbot drafts <lobby_addr> [gate_api_bind]`: create a game, build in its
//!   draft, and check the build survives an unload (same setup, plus Postgres).

mod mock_gate;
mod scenarios;

use std::net::SocketAddr;
use std::time::Duration;

use mmbot::{Bot, BotEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "join".into());
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
        other => anyhow::bail!("unknown mode `{other}` (expected join | lobby-ui | handoff | drafts)"),
    }
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

            let _ = tokio::time::timeout(hold, async {
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
