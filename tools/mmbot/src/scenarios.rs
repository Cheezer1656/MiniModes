use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use chunkedge::entity::EntityKind;
use mmbot::{Bot, BotEvent, bot_uuid};

use crate::mock_gate::{self, ConnectCall, MockGate};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);

async fn expect(bot: &mut Bot, what: &str, pred: impl FnMut(&BotEvent) -> bool) -> anyhow::Result<BotEvent> {
    tokio::time::timeout(STEP_TIMEOUT, bot.wait_for(pred))
        .await
        .with_context(|| format!("timed out waiting for {what}"))?
        .with_context(|| format!("waiting for {what}"))
}

/// Waits for a window titled `title` and returns its item names.
async fn expect_menu(bot: &mut Bot, title: &str) -> anyhow::Result<Vec<String>> {
    let BotEvent::ScreenOpened { window_id, .. } = expect(bot, title, |e| {
        matches!(e, BotEvent::ScreenOpened { title: t, .. } if t.contains(title))
    })
    .await?
    else {
        unreachable!()
    };
    let BotEvent::ScreenContent { items, .. } = expect(bot, "menu contents", |e| {
        matches!(e, BotEvent::ScreenContent { window_id: w, .. } if *w == window_id)
    })
    .await?
    else {
        unreachable!()
    };
    println!("  opened \"{title}\"");
    Ok(items)
}

async fn expect_chat(bot: &mut Bot, needle: &str) -> anyhow::Result<String> {
    let BotEvent::Chat(msg) = expect(bot, needle, |e| matches!(e, BotEvent::Chat(m) if m.contains(needle))).await? else {
        unreachable!()
    };
    println!("  chat: ...{needle}...");
    Ok(msg)
}

fn slot(items: &[String], idx: usize) -> &str {
    items.get(idx).map_or("", String::as_str)
}

pub async fn lobby_ui(addr: SocketAddr) -> anyhow::Result<()> {
    let mut bot = Bot::connect(addr, "uitest").await?;
    let mut npcs = Vec::new();
    expect(&mut bot, "spawn", |e| {
        if let BotEvent::EntitySpawned { entity_id, kind, .. } = e
            && *kind == EntityKind::PLAYER.get()
        {
            npcs.push(*entity_id);
        }
        matches!(e, BotEvent::Teleported(_))
    })
    .await?;
    println!("spawned; {} NPC(s) seen so far", npcs.len());

    println!("hotbar compass → browser");
    bot.use_item().await?;
    let items = expect_menu(&mut bot, "Game Browser").await?;
    anyhow::ensure!(slot(&items, 1).contains("Featured"), "tab missing: {:?}", slot(&items, 1));
    anyhow::ensure!(
        slot(&items, 10).starts_with("§a") || slot(&items, 22).contains("No games here yet"),
        "neither games nor the empty state shown"
    );

    println!("browser → favorites → recent → close");
    bot.click(50, 0).await?;
    let items = expect_menu(&mut bot, "Favorites").await?;
    anyhow::ensure!(slot(&items, 22).contains("favorite it"), "favorites empty state missing");
    bot.click(5, 0).await?;
    expect_menu(&mut bot, "Recently Played").await?;
    bot.click(49, 0).await?;
    expect(&mut bot, "close", |e| matches!(e, BotEvent::ScreenClosed { .. })).await?;

    println!("search prompt: cancel, then a real query");
    bot.select_hotbar(8).await?;
    bot.use_item().await?;
    expect_chat(&mut bot, "Type a game").await?;
    bot.chat("cancel").await?;
    expect_chat(&mut bot, "Cancelled").await?;
    bot.use_item().await?;
    expect_chat(&mut bot, "Type a game").await?;
    bot.chat("parkour").await?;
    let items = expect_menu(&mut bot, "Search Results").await?;
    anyhow::ensure!(slot(&items, 4).contains("parkour"), "query not echoed");
    bot.click(49, 0).await?;
    expect(&mut bot, "close", |e| matches!(e, BotEvent::ScreenClosed { .. })).await?;

    println!("my games → create");
    bot.select_hotbar(1).await?;
    bot.use_item().await?;
    expect_menu(&mut bot, "My Games").await?;
    bot.click(50, 0).await?;
    expect_chat(&mut bot, "isn't available yet").await?;

    if npcs.is_empty() {
        let event = expect(&mut bot, "an NPC", |e| {
            matches!(e, BotEvent::EntitySpawned { kind, .. } if *kind == EntityKind::PLAYER.get())
        })
        .await?;
        if let BotEvent::EntitySpawned { entity_id, .. } = event {
            npcs.push(entity_id);
        }
    }
    println!("NPC {} → menu", npcs[0]);
    bot.interact(npcs[0]).await?;
    let event = expect(&mut bot, "NPC menu", |e| matches!(e, BotEvent::ScreenOpened { .. })).await?;
    println!("  {event:?}");
    bot.click(49, 0).await?;

    println!("chat broadcast");
    bot.chat("hello world").await?;
    expect_chat(&mut bot, "uitest").await?;

    match tokio::time::timeout(Duration::from_millis(300), bot.next_event()).await {
        Ok(Ok(BotEvent::Disconnected(reason))) => bail!("disconnected at end: {reason}"),
        _ => {}
    }
    println!("lobby-ui scenario passed");
    Ok(())
}

/// Lobby → shard → lobby through the real coordinator and shard, with
/// `mock_gate` in place of Gate's API and the bots switching servers
/// themselves. Needs the lobby and a gamehost running against `gate_bind`.
pub async fn handoff(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;

    println!("waiting for a shard to register with the mock Gate");
    let shard = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let found = gate.servers.lock().unwrap().iter().find(|(n, _)| n.starts_with("gh-")).map(|(_, a)| *a);
            if let Some(addr) = found {
                return addr;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("no shard registered")?;
    println!("  shard at {shard}");
    tokio::time::timeout(Duration::from_secs(20), async {
        while tokio::net::TcpStream::connect(lobby).await.is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("lobby not listening")?;

    let (alice, alice_instance) = join_game(&mut gate, lobby, "alice", "Test Match", &secret).await?;
    let (bob, bob_instance) = join_game(&mut gate, lobby, "bob", "Test Match", &secret).await?;
    anyhow::ensure!(alice_instance == bob_instance, "match players split: {alice_instance} vs {bob_instance}");
    println!("  both players share instance {alice_instance}");
    drop(bob);

    println!("alice: /lobby");
    let mut alice = alice;
    alice.command("lobby").await?;
    let call = expect_connect(&mut gate, "alice", "lobby").await?;
    drop(alice);
    let mut alice = Bot::connect_forwarded(call.address.context("lobby address")?, "alice", &secret).await?;
    expect(&mut alice, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    println!("  back in the lobby");

    println!("persistent world: one instance shared by everyone");
    let (_carol, carol_instance) = join_game(&mut gate, lobby, "carol", "Test Prison", &secret).await?;
    let (_dave, dave_instance) = join_game(&mut gate, lobby, "dave", "Test Prison", &secret).await?;
    anyhow::ensure!(carol_instance == dave_instance, "persistent world split");

    println!("unexpected arrival is bounced to the lobby");
    let mut mallory = Bot::connect_forwarded(shard, "mallory", &secret).await?;
    expect_chat(&mut mallory, "weren't expected").await?;
    expect_connect(&mut gate, "mallory", "lobby").await?;

    println!("handoff scenario passed");
    Ok(())
}

/// Opens the browser in the lobby, clicks `game`, and follows the handoff.
/// Returns the bot (now on the shard) and its instance id.
async fn join_game(gate: &mut MockGate, lobby: SocketAddr, name: &str, game: &str, secret: &str) -> anyhow::Result<(Bot, String)> {
    println!("{name}: lobby → {game}");
    let mut bot = Bot::connect_forwarded(lobby, name, secret).await?;
    expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    // The shard may have registered with Gate before reaching the coordinator.
    for attempt in 1.. {
        bot.use_item().await?;
        let items = expect_menu(&mut bot, "Game Browser").await?;
        let idx = items
            .iter()
            .position(|i| i.contains(game))
            .with_context(|| format!("{game} not listed: {items:?}"))?;
        bot.click(idx as i16, 0).await?;
        let reply = expect(&mut bot, "join reply", |e| {
            matches!(e, BotEvent::Chat(m) if m.contains("Sending you to") || m.contains("No game servers"))
        })
        .await?;
        if matches!(&reply, BotEvent::Chat(m) if m.contains("Sending you to")) {
            break;
        }
        anyhow::ensure!(attempt < 20, "shard never reached the lobby");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!("  chat: ...Sending you to...");

    let call = expect_connect(gate, name, "gh-").await?;
    drop(bot);
    let mut bot = Bot::connect_forwarded(call.address.context("shard address")?, name, secret).await?;
    let msg = expect_chat(&mut bot, "Joined game").await?;
    let instance = msg
        .split("(instance ")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .context("no instance id in welcome")?
        .to_owned();
    Ok((bot, instance))
}

async fn expect_connect(gate: &mut MockGate, name: &str, server_prefix: &str) -> anyhow::Result<ConnectCall> {
    let call = tokio::time::timeout(STEP_TIMEOUT, gate.connects.recv())
        .await
        .with_context(|| format!("no ConnectPlayer for {name}"))?
        .context("mock Gate stopped")?;
    let uuid = bot_uuid(name).hyphenated().to_string();
    anyhow::ensure!(call.player == uuid, "ConnectPlayer for {} (expected {name} = {uuid})", call.player);
    anyhow::ensure!(call.server.starts_with(server_prefix), "{name} sent to {} (expected {server_prefix}*)", call.server);
    println!("  Gate: ConnectPlayer({name} → {})", call.server);
    Ok(call)
}

/// Create a game, build in its draft, leave, let it unload, reopen it, and
/// check the build survived. Needs the lobby, a gamehost and Postgres.
pub async fn drafts(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;
    wait_for_network(&gate, lobby).await?;
    // Unique per run so repeated runs don't hit the draft quota.
    let name = format!("b{}", std::process::id() % 100_000);
    let game = format!("Draft {name}");

    let bot = create_game(lobby, &name, &game, &secret).await?;
    let mut bot = open_draft(&mut gate, bot, &name, &game, &secret).await?;
    println!("build: 5×5 glass at y=65");
    select_square(&mut bot).await?;
    bot.command("/set glass").await?;
    expect_chat(&mut bot, "Changed 25 blocks to glass").await?;

    println!("leave; wait for the draft to unload and save");
    bot.command("lobby").await?;
    let call = expect_connect(&mut gate, &name, "lobby").await?;
    drop(bot);
    let mut bot = Bot::connect_forwarded(call.address.context("lobby address")?, &name, &secret).await?;
    expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    idle(&mut bot, Duration::from_secs(35)).await?;

    let mut bot = open_draft(&mut gate, bot, &name, &game, &secret).await?;
    println!("reopened: the glass should still be there");
    select_square(&mut bot).await?;
    bot.command("/replace glass stone").await?;
    expect_chat(&mut bot, "Changed 25 blocks to stone").await?;
    bot.command("/undo").await?;
    expect_chat(&mut bot, "Undid 25 block changes").await?;
    println!("copy the square and paste it 6 blocks east");
    bot.command("/copy").await?;
    expect_chat(&mut bot, "Copied 25 blocks").await?;
    bot.move_to(chunkedge::protocol::math::DVec3::new(10.5, 65.0, 4.5)).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    bot.command("/paste").await?;
    expect_chat(&mut bot, "Pasted; 25 blocks changed").await?;
    bot.command("/undo").await?;
    expect_chat(&mut bot, "Undid 25 block changes").await?;
    bot.command("/set nonsense_block").await?;
    expect_chat(&mut bot, "Unknown block").await?;

    println!("drafts scenario passed");
    Ok(())
}

/// Joins the lobby as `name` and creates a game called `game` through the
/// My Games menu.
async fn create_game(lobby: SocketAddr, name: &str, game: &str, secret: &str) -> anyhow::Result<Bot> {
    println!("{name}: create \"{game}\"");
    let mut bot = Bot::connect_forwarded(lobby, name, secret).await?;
    expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    // The account loads asynchronously after join.
    tokio::time::sleep(Duration::from_millis(500)).await;
    bot.select_hotbar(1).await?;
    bot.use_item().await?;
    expect_menu(&mut bot, "My Games").await?;
    bot.click(50, 0).await?;
    expect_menu(&mut bot, "Create a Game").await?;
    bot.click(13, 0).await?;
    expect_menu(&mut bot, "Pick an Icon").await?;
    bot.click(10, 0).await?;
    expect_chat(&mut bot, "Type a name").await?;
    bot.chat(game).await?;
    expect_chat(&mut bot, "Created").await?;
    Ok(bot)
}

const SCRIPT_MAIN: &str = r#"
local util = require("lib/util")

-- Only the grass in the 3x3 "mine" may be broken, by survival players.
local mine = Region.new(Vector3.new(2, 64, 2), Vector3.new(4, 64, 4))
mine.Rules.CanBreak = true
mine.Rules.BreakableBlocks = { "grass_block" }

game.Players.PlayerAdded:Connect(function(player)
    player:SendMessage(util.greet(player.Name))
    player.Chatted:Connect(function(message)
        if message == "ping" then
            task.wait(0.25)
            player:SendMessage("pong at y=" .. math.floor(player.Position.Y))
        elseif message == "survival" then
            player.GameMode = "survival"
            task.wait()
            player:SendMessage("mode " .. player.GameMode)
        end
    end)
end)

game.World.BlockBroken:Connect(function(event)
    event.Player:SendMessage("broke " .. event.Block .. " at " .. tostring(event.Position))
end)
"#;

const SCRIPT_UTIL: &str = r#"
local util = {}
function util.greet(name) return "Welcome to the script test, " .. name .. "!" end
return util
"#;

const SCRIPT_RUNAWAY: &str = r#"
print("v2 loaded")
task.spawn(function() while true do end end)
"#;

/// Scripts in a draft: load from Postgres, greet the player, answer chat,
/// see block breaks, enforce region rules, reload, and stop a runaway loop
/// without stalling the shard. Needs the lobby, a gamehost and Postgres.
pub async fn scripts(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let db = mm_db::connect(&mm_db::url_from_env()?, 2).await?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;
    wait_for_network(&gate, lobby).await?;
    let name = format!("s{}", std::process::id() % 100_000);
    let game = format!("Scripts {name}");

    let bot = create_game(lobby, &name, &game, &secret).await?;
    let owned = mm_db::owned_games(&db, bot_uuid(&name)).await?;
    let id = owned.iter().find(|g| g.name == game).context("created game not in the database")?.id;
    println!("upload main.luau + lib/util.luau to game {id}");
    mm_db::put_draft_file(&db, id, "main.luau", SCRIPT_MAIN, None).await?;
    mm_db::put_draft_file(&db, id, "lib/util.luau", SCRIPT_UTIL, None).await?;

    let mut bot = open_draft(&mut gate, bot, &name, &game, &secret).await?;
    expect_chat(&mut bot, "Scripts started").await?;
    expect_chat(&mut bot, &format!("Welcome to the script test, {name}!")).await?;

    bot.chat("ping").await?;
    expect_chat(&mut bot, "pong at y=65").await?;

    println!("creative break outside the mine: allowed, and the script sees it");
    bot.dig([1, 64, 1], false).await?;
    expect_chat(&mut bot, "broke grass_block at 1, 64, 1").await?;

    println!("survival: rules forbid breaking outside the mine");
    bot.chat("survival").await?;
    expect_chat(&mut bot, "mode survival").await?;
    bot.dig([6, 64, 6], true).await?;
    bot.chat("ping").await?;
    let reply = expect(&mut bot, "pong or break", |e| {
        matches!(e, BotEvent::Chat(m) if m.contains("pong") || m.contains("broke"))
    })
    .await?;
    anyhow::ensure!(matches!(&reply, BotEvent::Chat(m) if m.contains("pong")), "forbidden break went through: {reply:?}");
    bot.dig([3, 64, 3], true).await?;
    expect_chat(&mut bot, "broke grass_block at 3, 64, 3").await?;

    println!("reload with a runaway loop; the shard keeps responding and stops it");
    mm_db::put_draft_file(&db, id, "main.luau", SCRIPT_RUNAWAY, None).await?;
    bot.command("script reload").await?;
    expect_chat(&mut bot, "v2 loaded").await?;
    let started = std::time::Instant::now();
    bot.chat("still here").await?;
    expect_chat(&mut bot, "still here").await?;
    anyhow::ensure!(started.elapsed() < Duration::from_secs(2), "chat stalled behind the runaway script");
    // 100 ticks of overrun, so longer than one step's timeout.
    let stopped = tokio::time::timeout(
        Duration::from_secs(15),
        bot.wait_for(|e| matches!(e, BotEvent::Chat(m) if m.contains("Scripts stopped"))),
    )
    .await
    .context("the runaway script was never stopped")??;
    println!("  after {:.1}s: {stopped:?}", started.elapsed().as_secs_f32());

    println!("scripts scenario passed");
    Ok(())
}

/// Stays connected (answering keepalives) for `time`.
async fn idle(bot: &mut Bot, time: Duration) -> anyhow::Result<()> {
    tokio::time::timeout(time, async {
        loop {
            if let BotEvent::Disconnected(reason) = bot.next_event().await? {
                anyhow::bail!("disconnected while idle: {reason}");
            }
        }
    })
    .await
    .unwrap_or(Ok(()))
}

async fn wait_for_network(gate: &MockGate, lobby: SocketAddr) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !gate.servers.lock().unwrap().keys().any(|n| n.starts_with("gh-")) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        while tokio::net::TcpStream::connect(lobby).await.is_err() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("lobby or shard didn't come up")
}

/// From the lobby, opens My Games, clicks `game`, and follows the handoff
/// into the draft. Retries while the previous instance is still shutting down.
async fn open_draft(gate: &mut MockGate, mut bot: Bot, name: &str, game: &str, secret: &str) -> anyhow::Result<Bot> {
    for attempt in 1.. {
        bot.select_hotbar(1).await?;
        bot.use_item().await?;
        let items = expect_menu(&mut bot, "My Games").await?;
        let idx = items
            .iter()
            .position(|i| i.contains(game))
            .with_context(|| format!("{game} not in My Games: {items:?}"))?;
        bot.click(idx as i16, 0).await?;
        expect_chat(&mut bot, "Opening the draft").await?;
        let call = expect_connect(gate, name, "gh-").await?;
        let mut shard = Bot::connect_forwarded(call.address.context("shard address")?, name, secret).await?;
        let reply = expect(&mut shard, "draft welcome", |e| {
            matches!(e, BotEvent::Chat(m) if m.contains("Editing the draft") || m.contains("weren't expected"))
        })
        .await?;
        if matches!(&reply, BotEvent::Chat(m) if m.contains("Editing the draft")) {
            return Ok(shard);
        }
        // The old instance was ending; the shard bounced us. Back to the lobby.
        anyhow::ensure!(attempt < 10, "draft never reopened");
        let call = expect_connect(gate, name, "lobby").await?;
        drop(shard);
        bot = Bot::connect_forwarded(call.address.context("lobby address")?, name, secret).await?;
        expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
        idle(&mut bot, Duration::from_secs(2)).await?;
    }
    unreachable!()
}

/// Selects the 5×5 square from the spawn block to +4,+4 at feet level,
/// retrying while the map is still loading.
async fn select_square(bot: &mut Bot) -> anyhow::Result<()> {
    // A new draft's spawn: on the grass at y = 64, centered.
    let spawn = chunkedge::protocol::math::DVec3::new(0.5, 65.0, 0.5);
    for _ in 0..50 {
        bot.command("/pos1").await?;
        let msg = expect(bot, "pos1 reply", |e| {
            matches!(e, BotEvent::Chat(m) if m.contains("Position 1") || m.contains("still loading"))
        })
        .await?;
        if matches!(&msg, BotEvent::Chat(m) if m.contains("Position 1")) {
            bot.move_to(spawn + chunkedge::protocol::math::DVec3::new(4.0, 0.0, 4.0)).await?;
            tokio::time::sleep(Duration::from_millis(200)).await;
            bot.command("/pos2").await?;
            expect_chat(bot, "(25 blocks)").await?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("draft map never loaded")
}
