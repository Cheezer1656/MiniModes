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

    if let Ok(Ok(BotEvent::Disconnected(reason))) =
        tokio::time::timeout(Duration::from_millis(300), bot.next_event()).await
    {
        bail!("disconnected at end: {reason}");
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

    let pid = std::process::id() % 100_000;
    let (match_game, world_game) = (format!("Match {pid}"), format!("Prison {pid}"));
    let owner = format!("o{pid}");
    let mut bot = create_kind(lobby, &owner, &match_game, &secret, MATCH).await?;
    publish_game(&mut bot, &match_game, None).await?;
    drop(bot);
    let mut bot = create_kind(lobby, &format!("p{pid}"), &world_game, &secret, PERSISTENT).await?;
    publish_game(&mut bot, &world_game, None).await?;
    drop(bot);

    let (alice, alice_instance) = join_game(&mut gate, lobby, "alice", &match_game, &secret).await?;
    let (bob, bob_instance) = join_game(&mut gate, lobby, "bob", &match_game, &secret).await?;
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
    let (_carol, carol_instance) = join_game(&mut gate, lobby, "carol", &world_game, &secret).await?;
    let (_dave, dave_instance) = join_game(&mut gate, lobby, "dave", &world_game, &secret).await?;
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
    create_kind(lobby, name, game, secret, OPEN_WORLD).await
}

/// "Create a Game" menu slots.
const MATCH: i16 = 11;
const OPEN_WORLD: i16 = 13;
const PERSISTENT: i16 = 15;

/// Like [`create_game`], picking the kind by its menu slot.
async fn create_kind(lobby: SocketAddr, name: &str, game: &str, secret: &str, kind: i16) -> anyhow::Result<Bot> {
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
    bot.click(kind, 0).await?;
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
        let reply = expect(&mut bot, "draft reply", |e| {
            matches!(e, BotEvent::Chat(m) if m.contains("Opening the draft") || m.contains("still saving"))
        })
        .await?;
        if matches!(&reply, BotEvent::Chat(m) if m.contains("still saving")) {
            anyhow::ensure!(attempt < 20, "draft never finished saving");
            idle(&mut bot, Duration::from_secs(1)).await?;
            continue;
        }
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

const EDITOR_MAIN_V1: &str = r#"
local util = require("lib/util")

game.Players.PlayerAdded:Connect(function(player)
    print("hello from the editor, " .. player.Name)
    player:SendMessage(util.tag .. " editor script v1")
    task.delay(0.1, function()
        error("deliberate failure")
    end)
end)
"#;

const EDITOR_MAIN_V2: &str = r#"
game.Players.PlayerAdded:Connect(function(player)
    player:SendMessage("editor script v2")
end)
"#;

const EDITOR_UTIL: &str = "return { tag = \"[util]\" }\n";

/// A signed-in editor client: plain HTTP with the session cookie.
struct Editor {
    http: reqwest::Client,
    base: String,
    cookie: String,
    game: i64,
}

impl Editor {
    /// Exchanges a `/edit?t=` link for a session, as a browser would.
    async fn sign_in(http: &reqwest::Client, link: &str) -> anyhow::Result<Self> {
        let response = http.get(link).send().await?;
        anyhow::ensure!(response.status() == 303, "redeeming the link gave {}", response.status());
        let location = response.headers()["location"].to_str()?.to_owned();
        let game = location
            .strip_prefix("/editor/?game=")
            .with_context(|| format!("unexpected redirect to {location}"))?
            .parse()?;
        let cookie = response.headers()["set-cookie"].to_str()?;
        anyhow::ensure!(cookie.contains("HttpOnly"), "session cookie isn't HttpOnly: {cookie}");
        let cookie = cookie.split(';').next().unwrap_or_default().to_owned();
        let base = link.split("/edit?").next().unwrap_or_default().to_owned();
        Ok(Self {
            http: http.clone(),
            base,
            cookie,
            game,
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/api{}", self.base, path.replace("{game}", &self.game.to_string()));
        self.http
            .request(method, url)
            .header("cookie", &self.cookie)
            .header("x-minimodes", "1")
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<(u16, serde_json::Value)> {
        let mut request = self.request(method, path);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        Ok((status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)))
    }

    async fn expect(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
        status: u16,
    ) -> anyhow::Result<serde_json::Value> {
        let (got, value) = self.call(method.clone(), path, body).await?;
        anyhow::ensure!(got == status, "{method} {path}: expected {status}, got {got}: {value}");
        Ok(value)
    }

    async fn console(&self) -> anyhow::Result<Console> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = format!("{}/api/games/{}/console", self.base.replacen("http", "ws", 1), self.game);
        let mut request = url.into_client_request()?;
        request.headers_mut().insert("cookie", self.cookie.parse()?);
        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        Ok(Console { socket })
    }
}

struct Console {
    socket: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl Console {
    async fn next(&mut self) -> anyhow::Result<serde_json::Value> {
        use futures_util::StreamExt;
        loop {
            let msg = tokio::time::timeout(STEP_TIMEOUT, self.socket.next())
                .await
                .context("console went quiet")?
                .context("console closed")??;
            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                return Ok(serde_json::from_str(&text)?);
            }
        }
    }

    /// Waits for a line of `level` containing `needle`.
    async fn expect(&mut self, level: &str, needle: &str) -> anyhow::Result<String> {
        loop {
            let msg = self.next().await.with_context(|| format!("waiting for console {level} `{needle}`"))?;
            if msg["type"] == "line" && msg["level"] == level {
                let text = msg["text"].as_str().unwrap_or_default();
                if text.contains(needle) {
                    println!("  console {level}: {}", text.lines().next().unwrap_or_default());
                    return Ok(text.to_owned());
                }
            }
        }
    }
}

fn find_link(chat: &str) -> Option<String> {
    let start = chat.find("http")?;
    let link: String = chat[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || ":/.?=_-".contains(*c))
        .collect();
    link.contains("/edit?t=").then_some(link)
}

/// The web editor end to end: a `/script edit` link, file saves with
/// conflicts and limits, hot reload into the running draft, the console
/// stream, history, and collaborators. Needs the lobby (with `[web]`), a
/// gamehost (with `editor_url`) and Postgres.
pub async fn editor(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    use reqwest::Method;
    use serde_json::json;

    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let db = mm_db::connect(&mm_db::url_from_env()?, 2).await?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;
    wait_for_network(&gate, lobby).await?;
    let pid = std::process::id() % 100_000;
    let (name, helper) = (format!("e{pid}"), format!("h{pid}"));
    let game = format!("Editor {name}");

    // The collaborator has to have joined once; they stay in the lobby.
    let mut helper_bot = Bot::connect_forwarded(lobby, &helper, &secret).await?;
    expect(&mut helper_bot, "helper spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    let owner_bot = create_game(lobby, &name, &game, &secret).await?;
    let mut bot = open_draft(&mut gate, owner_bot, &name, &game, &secret).await?;

    println!("/script edit gives a one-time link");
    bot.command("script edit").await?;
    let chat = expect_chat(&mut bot, "/edit?t=").await?;
    let link = find_link(&chat).with_context(|| format!("no link in {chat:?}"))?;
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
    let owner = Editor::sign_in(&http, &link).await?;
    let again = http.get(&link).send().await?;
    anyhow::ensure!(again.status() == 400, "a used link worked again ({})", again.status());
    let session = owner.expect(Method::GET, "/session", None, 200).await?;
    anyhow::ensure!(
        session["games"].as_array().is_some_and(|g| g.iter().any(|g| g["id"] == owner.game && g["role"] == "owner")),
        "session doesn't list the game: {session}"
    );

    println!("requests without the session, the header, or from another origin are refused");
    let anonymous = http.get(format!("{}/api/session", owner.base)).send().await?;
    anyhow::ensure!(anonymous.status() == 401, "anonymous session gave {}", anonymous.status());
    let no_header = http
        .put(format!("{}/api/games/{}/files/main.luau", owner.base, owner.game))
        .header("cookie", &owner.cookie)
        .json(&json!({ "source": "" }))
        .send()
        .await?;
    anyhow::ensure!(no_header.status() == 403, "save without X-Minimodes gave {}", no_header.status());
    let foreign = owner
        .request(Method::PUT, "/games/{game}/files/main.luau")
        .header("origin", "https://evil.example")
        .json(&json!({ "source": "" }))
        .send()
        .await?;
    anyhow::ensure!(foreign.status() == 403, "cross-origin save gave {}", foreign.status());

    let mut console = owner.console().await?;
    let history = console.next().await?;
    anyhow::ensure!(history["type"] == "history", "console opened with {history}");

    println!("save files: create, conflict, bad paths, size limit");
    owner
        .expect(Method::PUT, "/games/{game}/files/lib/util.luau", Some(json!({ "source": EDITOR_UTIL, "base": 0 })), 200)
        .await?;
    let saved = owner
        .expect(Method::PUT, "/games/{game}/files/main.luau", Some(json!({ "source": EDITOR_MAIN_V1, "base": 0 })), 200)
        .await?;
    anyhow::ensure!(saved["version"] == 1, "first save is version {}", saved["version"]);
    owner
        .expect(Method::PUT, "/games/{game}/files/main.luau", Some(json!({ "source": "-- stale", "base": 0 })), 409)
        .await?;
    owner
        .expect(Method::PUT, "/games/{game}/files/bad%20name.luau", Some(json!({ "source": "" })), 400)
        .await?;
    owner
        .expect(Method::PUT, "/games/{game}/files/notes.txt", Some(json!({ "source": "" })), 400)
        .await?;
    let huge = "-".repeat(mm_proto::scripts::MAX_TOTAL_BYTES + 1);
    owner
        .expect(Method::PUT, "/games/{game}/files/huge.luau", Some(json!({ "source": huge })), 413)
        .await?;
    let files = owner.expect(Method::GET, "/games/{game}/files", None, 200).await?;
    let paths: Vec<_> = files.as_array().into_iter().flatten().filter_map(|f| f["path"].as_str()).collect();
    anyhow::ensure!(paths == ["lib/util.luau", "main.luau"], "files are {paths:?}");

    println!("run: the draft reloads and its output reaches the console");
    let reload = owner.expect(Method::POST, "/games/{game}/reload", None, 200).await?;
    anyhow::ensure!(reload["running"] == true, "reload says the draft isn't running");
    expect_chat(&mut bot, "reloaded the scripts from the editor").await?;
    expect_chat(&mut bot, "[util] editor script v1").await?;
    console.expect("system", &format!("{name} reloaded the scripts")).await?;
    console.expect("system", "scripts started").await?;
    console.expect("info", &format!("hello from the editor, {name}")).await?;
    let error = console.expect("error", "deliberate failure").await?;
    anyhow::ensure!(error.contains("main.luau:"), "error doesn't name its module: {error}");

    println!("history, revisions, rename and delete");
    owner
        .expect(Method::PUT, "/games/{game}/files/main.luau", Some(json!({ "source": EDITOR_MAIN_V2, "base": 1 })), 200)
        .await?;
    let revisions = owner.expect(Method::GET, "/games/{game}/history/main.luau", None, 200).await?;
    let revisions = revisions.as_array().context("history isn't a list")?;
    anyhow::ensure!(revisions.len() == 2, "main.luau has {} revisions", revisions.len());
    let oldest = revisions[1]["id"].as_i64().context("revision id")?;
    let old = owner.expect(Method::GET, &format!("/games/{{game}}/revisions/{oldest}"), None, 200).await?;
    anyhow::ensure!(old["source"] == EDITOR_MAIN_V1, "oldest revision is {old}");
    owner
        .expect(Method::PUT, "/games/{game}/files/scratch.luau", Some(json!({ "source": "return 1" })), 200)
        .await?;
    owner
        .expect(Method::POST, "/games/{game}/rename", Some(json!({ "from": "scratch.luau", "to": "lib/util.luau" })), 409)
        .await?;
    owner
        .expect(Method::POST, "/games/{game}/rename", Some(json!({ "from": "scratch.luau", "to": "old/scratch.luau" })), 204)
        .await?;
    owner.expect(Method::GET, "/games/{game}/files/old/scratch.luau", None, 200).await?;
    owner.expect(Method::DELETE, "/games/{game}/files/old/scratch.luau", None, 204).await?;
    owner.expect(Method::GET, "/games/{game}/files/old/scratch.luau", None, 404).await?;

    println!("/script reload in game also uses the saved scripts");
    bot.command("script reload").await?;
    expect_chat(&mut bot, "editor script v2").await?;

    println!("collaborators: invite, shared access, limits, removal");
    owner
        .expect(Method::POST, "/games/{game}/collaborators", Some(json!({ "name": "nobody-here-xyz" })), 422)
        .await?;
    owner
        .expect(Method::POST, "/games/{game}/collaborators", Some(json!({ "name": name })), 422)
        .await?;
    let added = owner
        .expect(Method::POST, "/games/{game}/collaborators", Some(json!({ "name": helper.to_uppercase() })), 200)
        .await?;
    anyhow::ensure!(added["name"] == helper.as_str(), "added {added}");
    // The helper's lobby menu picks the game up without rejoining.
    tokio::time::sleep(Duration::from_millis(500)).await;
    helper_bot.select_hotbar(1).await?;
    helper_bot.use_item().await?;
    let items = expect_menu(&mut helper_bot, "My Games").await?;
    anyhow::ensure!(items.iter().any(|i| i.contains(&game)), "{game} not in the helper's My Games: {items:?}");

    let (token, hash) = mm_db::editor::new_secret();
    mm_db::editor::create_token(&db, &hash, bot_uuid(&helper), owner.game).await?;
    let collaborator = Editor::sign_in(&http, &format!("{}/edit?t={token}", owner.base)).await?;
    collaborator.expect(Method::GET, "/games/{game}/files/main.luau", None, 200).await?;
    let people = collaborator.expect(Method::GET, "/games/{game}/collaborators", None, 200).await?;
    anyhow::ensure!(people["can_manage"] == false, "collaborator can manage: {people}");
    collaborator
        .expect(Method::POST, "/games/{game}/collaborators", Some(json!({ "name": name })), 403)
        .await?;
    let helper_id = added["uuid"].as_str().context("uuid")?;
    owner
        .expect(Method::DELETE, &format!("/games/{{game}}/collaborators/{helper_id}"), None, 204)
        .await?;
    collaborator.expect(Method::GET, "/games/{game}/files/main.luau", None, 404).await?;

    println!("sign out ends the session");
    owner.expect(Method::POST, "/logout", None, 204).await?;
    owner.expect(Method::GET, "/session", None, 401).await?;

    println!("editor scenario passed");
    Ok(())
}

/// From the lobby: right-click `game` in My Games, then Publish (and the
/// live-update mode at `mode_slot` when the menu asks for one).
async fn publish_game(bot: &mut Bot, game: &str, mode_slot: Option<i16>) -> anyhow::Result<String> {
    println!("{}: publish \"{game}\"", bot.name);
    bot.select_hotbar(1).await?;
    bot.use_item().await?;
    let items = expect_menu(bot, "My Games").await?;
    let idx = items
        .iter()
        .position(|i| i.contains(game))
        .with_context(|| format!("{game} not in My Games: {items:?}"))?;
    bot.click(idx as i16, 1).await?;
    let manage = expect_menu(bot, "Manage:").await?;
    anyhow::ensure!(slot(&manage, 12).contains("Publish"), "no publish button: {manage:?}");
    bot.click(12, 0).await?;
    if let Some(mode) = mode_slot {
        expect_menu(bot, "Publish v").await?;
        bot.click(mode, 0).await?;
    }
    let reply = expect(bot, "publish result", |e| {
        matches!(e, BotEvent::Chat(m) if m.contains("Published") || m.contains("Couldn't") || m.contains("couldn't"))
    })
    .await?;
    let BotEvent::Chat(msg) = reply else { unreachable!() };
    anyhow::ensure!(msg.contains("Published"), "publish failed: {msg}");
    println!("  chat: {msg}");
    Ok(msg)
}

/// Joins the lobby and waits to spawn.
async fn lobby_bot(lobby: SocketAddr, name: &str, secret: &str) -> anyhow::Result<Bot> {
    let mut bot = Bot::connect_forwarded(lobby, name, secret).await?;
    expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    Ok(bot)
}

/// Back to the lobby with `/lobby` and reconnect there.
async fn to_lobby(gate: &mut MockGate, mut bot: Bot, name: &str, secret: &str) -> anyhow::Result<Bot> {
    bot.command("lobby").await?;
    let call = expect_connect(gate, name, "lobby").await?;
    drop(bot);
    let mut bot = Bot::connect_forwarded(call.address.context("lobby address")?, name, secret).await?;
    expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    Ok(bot)
}

/// Joins a published game from the Game Browser, following the handoff, and
/// retrying while it reports the world restarting or full.
async fn play(gate: &mut MockGate, mut bot: Bot, name: &str, game: &str, secret: &str) -> anyhow::Result<Bot> {
    for attempt in 1.. {
        bot.select_hotbar(0).await?;
        bot.use_item().await?;
        let items = expect_menu(&mut bot, "Game Browser").await?;
        let idx = items
            .iter()
            .position(|i| i.contains(game))
            .with_context(|| format!("{game} not listed: {items:?}"))?;
        bot.click(idx as i16, 0).await?;
        let reply = expect(&mut bot, "join reply", |e| {
            matches!(e, BotEvent::Chat(m) if m.contains("Sending you to") || m.contains("queue") || m.contains("Try again"))
        })
        .await?;
        let BotEvent::Chat(msg) = reply else { unreachable!() };
        if msg.contains("queue") {
            // Wait for the queue to move us.
            expect_chat_within(&mut bot, "Sending you to", Duration::from_secs(20)).await?;
        } else if !msg.contains("Sending you to") {
            anyhow::ensure!(attempt < 20, "never got in: {msg}");
            idle(&mut bot, Duration::from_millis(500)).await?;
            continue;
        }
        let call = expect_connect(gate, name, "gh-").await?;
        drop(bot);
        let mut shard = Bot::connect_forwarded(call.address.context("shard address")?, name, secret).await?;
        let reply = expect(&mut shard, "welcome", |e| {
            matches!(e, BotEvent::Chat(m) if m.contains("Joined game") || m.contains("weren't expected"))
        })
        .await?;
        if matches!(&reply, BotEvent::Chat(m) if m.contains("Joined game")) {
            return Ok(shard);
        }
        anyhow::ensure!(attempt < 20, "the shard kept bouncing {name}");
        let call = expect_connect(gate, name, "lobby").await?;
        drop(shard);
        bot = Bot::connect_forwarded(call.address.context("lobby address")?, name, secret).await?;
        expect(&mut bot, "lobby spawn", |e| matches!(e, BotEvent::Teleported(_))).await?;
    }
    unreachable!()
}

async fn expect_chat_within(bot: &mut Bot, needle: &str, time: Duration) -> anyhow::Result<String> {
    let event = tokio::time::timeout(time, bot.wait_for(|e| matches!(e, BotEvent::Chat(m) if m.contains(needle))))
        .await
        .with_context(|| format!("timed out waiting for {needle:?}"))??;
    let BotEvent::Chat(msg) = event else { unreachable!() };
    println!("  chat: ...{needle}...");
    Ok(msg)
}

const MATCH_SCRIPT: &str = r#"
game.Match.MinPlayers = 2
game.Match.CountdownSeconds = 3
local function everyone(text)
    for _, p in game.Players:GetPlayers() do p:SendMessage(text) end
end
game.Match.Starting:Connect(function(seconds) everyone("starting in " .. seconds) end)
game.Match.Started:Connect(function() everyone("match started v" .. tostring(game.Version)) end)
game.Match.Ended:Connect(function(winners) everyone("ended, first winner " .. winners[1].Name) end)
game.Players.PlayerAdded:Connect(function(player)
    player.Chatted:Connect(function(message)
        if message == "win" then game.Match:End(player) end
    end)
end)
game:BindToClose(function() print("match closing") end)
"#;

const WORLD_V1: &str = r#"
game.Players.PlayerAdded:Connect(function(player)
    player:SendMessage("v1 hello last=" .. tostring(game.LastShutdown))
    player.Chatted:Connect(function(message)
        if message == "where" then
            local p = player.Position
            player:SendMessage("at " .. math.floor(p.X) .. "," .. math.floor(p.Z))
        elseif message == "prize" then
            player:Give(Item.new("diamond", { Name = "Prize" }))
            player:SendMessage("gave prize")
        elseif message == "block" then
            player:SendMessage("block " .. game.World:GetBlock(Vector3.new(1, 65, 1)))
        end
    end)
end)
"#;

/// Publishing and the game kinds: a match runs its countdown and ends back
/// in the lobby; a persistent world keeps players' state across an idle
/// unload, reports `LastShutdown`, and takes a "scripts + regions" update
/// and a reset while a player is inside. Needs the lobby, a gamehost
/// (`idle_unload_secs = 30`) and Postgres.
pub async fn games(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let db = mm_db::connect(&mm_db::url_from_env()?, 2).await?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;
    wait_for_network(&gate, lobby).await?;
    let pid = std::process::id() % 100_000;
    let id_of = async |owner: &str, game: &str| -> anyhow::Result<i64> {
        let owned = mm_db::owned_games(&db, bot_uuid(owner)).await?;
        Ok(owned.iter().find(|g| g.name == game).context("created game not in the database")?.id)
    };

    println!("== match: countdown, start, End(winner), back to the lobby");
    let (owner, game) = (format!("m{pid}"), format!("Arena {pid}"));
    let mut owner_bot = create_kind(lobby, &owner, &game, &secret, MATCH).await?;
    let id = id_of(&owner, &game).await?;
    mm_db::put_draft_file(&db, id, "main.luau", MATCH_SCRIPT, None).await?;
    publish_game(&mut owner_bot, &game, None).await?;
    let (a, b) = (format!("ma{pid}"), format!("mb{pid}"));
    let lobby_a = lobby_bot(lobby, &a, &secret).await?;
    let mut alice = play(&mut gate, lobby_a, &a, &game, &secret).await?;
    let lobby_b = lobby_bot(lobby, &b, &secret).await?;
    let mut bob = play(&mut gate, lobby_b, &b, &game, &secret).await?;
    expect_chat(&mut alice, "starting in 3").await?;
    expect_chat_within(&mut alice, "match started v1", Duration::from_secs(6)).await?;
    expect_chat(&mut bob, "match started v1").await?;

    println!("a third player can't join a started match: they get a new instance");
    let c = format!("mc{pid}");
    let lobby_c = lobby_bot(lobby, &c, &secret).await?;
    let mut carol = play(&mut gate, lobby_c, &c, &game, &secret).await?;
    alice.chat("win").await?;
    let over = expect_chat(&mut bob, "The match is over").await?;
    anyhow::ensure!(over.contains(&a), "wrong winner: {over}");
    expect_chat(&mut alice, &format!("ended, first winner {a}")).await?;
    anyhow::ensure!(
        tokio::time::timeout(Duration::from_millis(500), carol.wait_for(|e| matches!(e, BotEvent::Chat(m) if m.contains("match is over"))))
            .await
            .is_err(),
        "the third player was put into the started match"
    );
    println!("after the result screen both go back to the lobby");
    let first = tokio::time::timeout(Duration::from_secs(10), gate.connects.recv()).await?.context("gate stopped")?;
    let second = tokio::time::timeout(Duration::from_secs(5), gate.connects.recv()).await?.context("gate stopped")?;
    let mut sent: Vec<_> = [&first, &second].iter().map(|c| (c.player.clone(), c.server.clone())).collect();
    sent.sort();
    let mut want = vec![
        (bot_uuid(&a).hyphenated().to_string(), "lobby".to_owned()),
        (bot_uuid(&b).hyphenated().to_string(), "lobby".to_owned()),
    ];
    want.sort();
    anyhow::ensure!(sent == want, "after the match: {sent:?}");
    println!("  both sent to the lobby");
    drop((alice, bob, carol));

    println!("== persistent world: player state, LastShutdown, live updates");
    let (owner, game) = (format!("w{pid}"), format!("Prison {pid}"));
    let owner_bot = create_kind(lobby, &owner, &game, &secret, PERSISTENT).await?;
    let id = id_of(&owner, &game).await?;
    mm_db::put_draft_file(&db, id, "main.luau", WORLD_V1, None).await?;
    let mut owner_bot = owner_bot;
    publish_game(&mut owner_bot, &game, None).await?;

    println!("owner builds a glass pad in the draft and marks it as a region");
    let mut builder = open_draft(&mut gate, owner_bot, &owner, &game, &secret).await?;
    select_square(&mut builder).await?;
    builder.command("/set glass").await?;
    expect_chat(&mut builder, "Changed 25 blocks to glass").await?;
    builder.command("/region add pad").await?;
    expect_chat(&mut builder, "Saved region").await?;
    mm_db::put_draft_file(&db, id, "main.luau", &WORLD_V1.replace("v1 hello", "v2 hello"), None).await?;
    let mut owner_bot = to_lobby(&mut gate, builder, &owner, &secret).await?;

    let p = format!("wp{pid}");
    let lobby_p = lobby_bot(lobby, &p, &secret).await?;
    let mut player = play(&mut gate, lobby_p, &p, &game, &secret).await?;
    expect_chat(&mut player, "v1 hello last=nil").await?;
    player.chat("block").await?;
    expect_chat(&mut player, "block air").await?;
    player.chat("prize").await?;
    expect_chat(&mut player, "gave prize").await?;

    println!("publish v2 as scripts + regions while the player is inside (the draft saves first)");
    publish_game(&mut owner_bot, &game, Some(13)).await?;
    expect_chat(&mut player, "updated to v2").await?;
    expect_chat(&mut player, "v2 hello").await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    player.chat("block").await?;
    expect_chat(&mut player, "block glass").await?;

    println!("move, leave, wait for the idle unload, come back");
    player.move_to(chunkedge::protocol::math::DVec3::new(6.5, 65.0, -3.5)).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    player.chat("where").await?;
    expect_chat(&mut player, "at 6,-4").await?;
    let lobby_p = to_lobby(&mut gate, player, &p, &secret).await?;
    let mut lobby_p = lobby_p;
    idle(&mut lobby_p, Duration::from_secs(38)).await?;
    let saved_state = mm_db::publish::player_state(&db, id, bot_uuid(&p)).await?;
    anyhow::ensure!(saved_state.is_some(), "the player's state wasn't saved");
    let mut player = play(&mut gate, lobby_p, &p, &game, &secret).await?;
    // The inventory is restored before scripts see the player, so the prize
    // arrives ahead of the greeting.
    let (mut prize, mut hello) = (false, None);
    tokio::time::timeout(
        STEP_TIMEOUT,
        player.wait_for(|e| {
            match e {
                BotEvent::SlotChanged { window_id: 0, item, .. } if item.contains("Prize") => prize = true,
                BotEvent::ScreenContent { window_id: 0, items } if items.iter().any(|i| i.contains("Prize")) => {
                    prize = true
                }
                BotEvent::Chat(m) if m.contains("v2 hello last=") => hello = Some(m.clone()),
                _ => {}
            }
            hello.is_some()
        }),
    )
    .await
    .context("no greeting after rejoining")??;
    let hello = hello.unwrap_or_default();
    anyhow::ensure!(!hello.contains("last=nil"), "LastShutdown not set after the unload: {hello}");
    anyhow::ensure!(prize, "the prize wasn't restored");
    println!("  prize restored");
    player.chat("where").await?;
    expect_chat(&mut player, "at 6,-4").await?;
    player.chat("block").await?;
    expect_chat(&mut player, "block glass").await?;

    println!("publish v3 as a full reset while the player is inside");
    // The owner sat unread through the wait and timed out; log in again.
    drop(owner_bot);
    let mut owner_bot = lobby_bot(lobby, &owner, &secret).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_game(&mut owner_bot, &game, Some(15)).await?;
    expect_chat(&mut player, "reset for v3").await?;
    let backups = mm_db::publish::live_world_backups(&db, id).await?;
    anyhow::ensure!(backups.len() == 1, "expected one backup, got {backups:?}");

    println!("games scenario passed");
    Ok(())
}

const DATA_SCRIPT: &str = r#"
local visits = DataStoreService:GetDataStore("visits")
local profiles = DataStoreService:GetDataStore("profiles")
local board = DataStoreService:GetOrderedDataStore("board")
game.Players.PlayerAdded:Connect(function(player)
    local n = visits:IncrementAsync(player.UserId)
    player:SendMessage("visit " .. n .. " draft=" .. tostring(game.IsDraft))
    player.Chatted:Connect(function(message)
        if message == "save" then
            profiles:SetAsync(player.UserId, { name = player.Name, coins = 7, tags = { "a", "b" } })
            player:SendMessage("saved")
        elseif message == "load" then
            local p = profiles:UpdateAsync(player.UserId, function(old)
                old.coins += 1
                return old
            end)
            player:SendMessage("loaded " .. p.name .. " coins=" .. p.coins .. " tags=" .. #p.tags)
        elseif message == "board" then
            board:SetAsync("x", 5)
            board:SetAsync("y", 50)
            board:IncrementAsync(player.Name, 20)
            local pages = board:GetSortedAsync(false, 2)
            local parts = {}
            for _, entry in pages:GetCurrentPage() do
                table.insert(parts, entry.key .. "=" .. entry.value)
            end
            player:SendMessage("top " .. table.concat(parts, ",") .. " more=" .. tostring(not pages.IsFinished))
        elseif message == "bad" then
            local ok, err = pcall(profiles.SetAsync, profiles, "k", { f = print })
            player:SendMessage("bad " .. tostring(ok) .. " " .. tostring(err))
        end
    end)
end)
"#;

/// Opens the Game Browser and returns the slot and text of `game`'s icon.
async fn find_in_browser(bot: &mut Bot, game: &str) -> anyhow::Result<(i16, String)> {
    bot.select_hotbar(0).await?;
    bot.use_item().await?;
    let items = expect_menu(bot, "Game Browser").await?;
    let idx = items
        .iter()
        .position(|i| i.contains(game))
        .with_context(|| format!("{game} not listed: {items:?}"))?;
    Ok((idx as i16, items[idx].clone()))
}

/// DataStores and the browser's saved data: a published game's scripts keep
/// data in Postgres across visits (apart from the draft's), a returning
/// player is asked to rate the game, and ratings, favorites, recent games
/// and search survive a reconnect. Needs the lobby, a gamehost and Postgres.
pub async fn data(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let db = mm_db::connect(&mm_db::url_from_env()?, 2).await?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;
    wait_for_network(&gate, lobby).await?;
    let pid = std::process::id() % 100_000;
    let (owner, game, p) = (format!("d{pid}"), format!("Vault {pid}"), format!("dp{pid}"));

    let mut owner_bot = create_kind(lobby, &owner, &game, &secret, OPEN_WORLD).await?;
    let owned = mm_db::owned_games(&db, bot_uuid(&owner)).await?;
    let id = owned.iter().find(|g| g.name == game).context("created game not in the database")?.id;
    mm_db::put_draft_file(&db, id, "main.luau", DATA_SCRIPT, None).await?;
    publish_game(&mut owner_bot, &game, None).await?;

    println!("== DataStores in the published game");
    let lobby_p = lobby_bot(lobby, &p, &secret).await?;
    let mut player = play(&mut gate, lobby_p, &p, &game, &secret).await?;
    expect_chat(&mut player, "visit 1 draft=false").await?;
    player.chat("save").await?;
    expect_chat(&mut player, "saved").await?;
    player.chat("load").await?;
    expect_chat(&mut player, &format!("loaded {p} coins=8 tags=2")).await?;
    player.chat("board").await?;
    expect_chat(&mut player, &format!("top y=50,{p}=20 more=true")).await?;
    player.chat("bad").await?;
    expect_chat(&mut player, "can't store a function").await?;
    let visits = mm_db::datastore::Store {
        game: id,
        draft: false,
        ordered: false,
        name: "visits",
    };
    let uuid = bot_uuid(&p).hyphenated().to_string();
    let stored = mm_db::datastore::get(&db, visits, &uuid).await?.context("visit count not saved")?;
    anyhow::ensure!(stored.value == mm_db::datastore::Value::Json("1".into()), "stored {stored:?}");

    println!("== back in the lobby: rate it");
    let mut lobby_p = to_lobby(&mut gate, player, &p, &secret).await?;
    expect_chat(&mut lobby_p, "How was").await?;
    lobby_p.command(&format!("rate {id} 4")).await?;
    expect_chat(&mut lobby_p, "You rated").await?;
    let (idx, icon) = find_in_browser(&mut lobby_p, &game).await?;
    anyhow::ensure!(icon.contains("★ 4.0") && icon.contains("(1 rating)"), "rating not shown: {icon}");

    println!("favorite it, then check the details menu");
    lobby_p.click(idx, 1).await?;
    let items = expect_menu(&mut lobby_p, "Game Browser").await?;
    anyhow::ensure!(slot(&items, idx as usize).contains("★\n"), "not marked favorite: {}", slot(&items, idx as usize));
    lobby_p.shift_click(idx).await?;
    let info = expect_menu(&mut lobby_p, &game).await?;
    anyhow::ensure!(slot(&info, 4).contains("1 player so far"), "details: {}", slot(&info, 4));
    anyhow::ensure!(slot(&info, 24).contains("Favorite"), "favorite button: {}", slot(&info, 24));
    anyhow::ensure!(slot(&info, 32).contains("Your rating"), "4th star: {}", slot(&info, 32));
    lobby_p.click(33, 0).await?;
    expect_chat(&mut lobby_p, "You rated").await?;
    let info = expect_menu(&mut lobby_p, &game).await?;
    anyhow::ensure!(slot(&info, 4).contains("★ 5.0"), "re-rating not shown: {}", slot(&info, 4));

    println!("reconnect: favorites, recent and search come back from the database");
    drop(lobby_p);
    let mut lobby_p = lobby_bot(lobby, &p, &secret).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, icon) = find_in_browser(&mut lobby_p, &game).await?;
    anyhow::ensure!(icon.contains("★ 5.0"), "rating lost: {icon}");
    lobby_p.click(50, 0).await?;
    let favorites = expect_menu(&mut lobby_p, "Favorites").await?;
    anyhow::ensure!(favorites.iter().any(|i| i.contains(&game)), "favorite lost: {favorites:?}");
    lobby_p.click(5, 0).await?;
    let recent = expect_menu(&mut lobby_p, "Recently Played").await?;
    anyhow::ensure!(recent.iter().any(|i| i.contains(&game)), "recent lost: {recent:?}");
    lobby_p.select_hotbar(8).await?;
    lobby_p.use_item().await?;
    expect_chat(&mut lobby_p, "Type a game").await?;
    lobby_p.chat(&format!("vault {pid}")).await?;
    let results = expect_menu(&mut lobby_p, "Search Results").await?;
    anyhow::ensure!(slot(&results, 10).contains(&game), "search: {results:?}");

    println!("== the second visit reads the saved count; the draft has its own data");
    let mut player = play(&mut gate, lobby_p, &p, &game, &secret).await?;
    expect_chat(&mut player, "visit 2 draft=false").await?;
    drop(player);
    let mut builder = open_draft(&mut gate, owner_bot, &owner, &game, &secret).await?;
    expect_chat(&mut builder, "visit 1 draft=true").await?;

    println!("data scenario passed");
    Ok(())
}

const MOD_SCRIPT: &str = r#"
game.Players.PlayerAdded:Connect(function(player)
    player:SendMessage("Welcome to Mod Test")
    player:Give(Item.new("stick", { Name = "Zap Wand", Lore = { "Zaps things" } }))
end)
"#;

/// Fetches a `/metrics` page and checks it has each of `names`.
async fn expect_metrics(addr: &str, names: &[&str]) -> anyhow::Result<()> {
    let body = reqwest::get(format!("http://{addr}/metrics")).await?.error_for_status()?.text().await?;
    for name in names {
        anyhow::ensure!(body.contains(name), "{addr}/metrics has no {name}:\n{body}");
    }
    println!("  {addr}/metrics has {}", names.join(", "));
    Ok(())
}

/// Moderation: a report from a game and one from the lobby put a game on
/// hold (the dev lobby holds at 2), a moderator reviews it in `/mod` (reports,
/// script text), clears it, then bans the creator, which stops the game;
/// `/rank set` and the metrics endpoints. Needs the lobby, a gamehost and
/// Postgres, with the dev configs.
pub async fn moderation(lobby: SocketAddr, gate_bind: SocketAddr) -> anyhow::Result<()> {
    let secret = std::env::var("MINIMODES_VELOCITY_SECRET").context("MINIMODES_VELOCITY_SECRET must be set")?;
    let db = mm_db::connect(&mm_db::url_from_env()?, 2).await?;
    let mut gate = mock_gate::start(gate_bind, lobby).await?;
    wait_for_network(&gate, lobby).await?;
    let pid = std::process::id() % 100_000;
    let (owner, game) = (format!("o{pid}"), format!("Mod Test {pid}"));
    let (moderator, a, b) = (format!("mod{pid}"), format!("ra{pid}"), format!("rb{pid}"));

    let mut owner_bot = create_kind(lobby, &owner, &game, &secret, OPEN_WORLD).await?;
    let id = mm_db::owned_games(&db, bot_uuid(&owner))
        .await?
        .iter()
        .find(|g| g.name == game)
        .context("created game not in the database")?
        .id;
    mm_db::put_draft_file(&db, id, "main.luau", MOD_SCRIPT, None).await?;
    publish_game(&mut owner_bot, &game, None).await?;

    // The rank is read when the account loads, so grant it before joining.
    mm_db::player_joined(&db, bot_uuid(&moderator), &moderator).await?;
    mm_db::moderation::set_rank(&db, bot_uuid(&moderator), Some("admin"), None).await?;
    let mut staff = lobby_bot(lobby, &moderator, &secret).await?;

    println!("== a player reports the game from inside it");
    let lobby_a = lobby_bot(lobby, &a, &secret).await?;
    let mut alice = play(&mut gate, lobby_a, &a, &game, &secret).await?;
    expect_chat(&mut alice, "Welcome to Mod Test").await?;
    alice.command("report").await?;
    expect_chat(&mut alice, "Usage").await?;
    alice.command("report spams the chat").await?;
    expect_chat(&mut alice, "report was sent").await?;
    expect_chat(&mut staff, "reported (1 open)").await?;

    println!("== another reports it from the browser; two reports hold it");
    let mut bob = lobby_bot(lobby, &b, &secret).await?;
    bob.command("mod").await?;
    expect_chat(&mut bob, "Only moderators").await?;
    let (idx, _) = find_in_browser(&mut bob, &game).await?;
    bob.shift_click(idx).await?;
    let info = expect_menu(&mut bob, &game).await?;
    anyhow::ensure!(slot(&info, 44).contains("Report"), "no report button: {}", slot(&info, 44));
    bob.click(44, 0).await?;
    expect_chat(&mut bob, "Why are you reporting").await?;
    bob.chat("bad words everywhere").await?;
    expect_chat(&mut bob, "report was sent").await?;
    expect_chat(&mut staff, "held for review after 2 reports").await?;
    bob.select_hotbar(0).await?;
    bob.use_item().await?;
    let browser = expect_menu(&mut bob, "Game Browser").await?;
    anyhow::ensure!(!browser.iter().any(|i| i.contains(&game)), "held game still listed");
    let mut owner_games = mm_db::owned_games(&db, bot_uuid(&owner)).await?;
    anyhow::ensure!(owner_games.pop().and_then(|g| g.hold).as_deref() == Some("review"), "not held in the database");

    println!("== the moderator reviews it");
    staff.command("mod").await?;
    let queue = expect_menu(&mut staff, "Moderation Queue").await?;
    let idx = queue.iter().position(|i| i.contains(&game)).with_context(|| format!("not queued: {queue:?}"))?;
    anyhow::ensure!(queue[idx].contains("2 open report"), "queue entry: {}", queue[idx]);
    staff.click(idx as i16, 0).await?;
    let review = expect_menu(&mut staff, "Review:").await?;
    let status = slot(&review, 4);
    anyhow::ensure!(status.contains("On hold") && status.contains("2 reports"), "review: {status}");
    staff.click(19, 0).await?;
    expect_chat(&mut staff, "bad words everywhere").await?;
    expect_chat(&mut staff, "spams the chat").await?;
    // The shard writes the text log every two seconds.
    idle(&mut staff, Duration::from_secs(3)).await?;
    staff.click(20, 0).await?;
    expect_chat(&mut staff, "Welcome to Mod Test").await?;
    let item = expect_chat(&mut staff, "Zap Wand").await?;
    anyhow::ensure!(item.contains("Zaps things") && item.contains(&a), "item line: {item}");

    println!("clear it: back in the browser");
    staff.click(21, 0).await?;
    expect_chat(&mut staff, "Cleared").await?;
    let review = expect_menu(&mut staff, "Review:").await?;
    anyhow::ensure!(slot(&review, 4).contains("Listed"), "after clearing: {}", slot(&review, 4));
    find_in_browser(&mut bob, &game).await?;

    println!("== ban the creator: the game stops and they can't build");
    staff.click(25, 0).await?;
    expect_chat(&mut staff, "Shift-click").await?;
    staff.shift_click(25).await?;
    expect_chat(&mut staff, "from creating until lifted").await?;
    expect_chat(&mut owner_bot, "You were banned from creating games").await?;
    let call = expect_connect(&mut gate, &a, "lobby").await?;
    println!("  {} sent to {}", call.player, call.server);
    drop(alice);
    idle(&mut owner_bot, Duration::from_millis(500)).await?;
    owner_bot.select_hotbar(1).await?;
    owner_bot.use_item().await?;
    let mine = expect_menu(&mut owner_bot, "My Games").await?;
    let idx = mine.iter().position(|i| i.contains(&game)).context("game gone from My Games")?;
    anyhow::ensure!(mine[idx].contains("Under moderator review"), "hold not shown: {}", mine[idx]);
    owner_bot.click(idx as i16, 0).await?;
    expect_chat(&mut owner_bot, "banned from creating games").await?;
    anyhow::ensure!(mm_db::editor::role(&db, id, bot_uuid(&owner)).await?.is_none(), "banned owner can still edit");

    staff.command(&format!("creatorunban {owner}")).await?;
    expect_chat(&mut staff, "Lifted").await?;
    expect_chat(&mut owner_bot, "ban was lifted").await?;

    println!("== ranks");
    staff.command(&format!("rank set {b} nonsense")).await?;
    expect_chat(&mut staff, "No rank `nonsense`").await?;
    staff.command(&format!("rank set {b} plus")).await?;
    expect_chat(&mut staff, "is now").await?;
    expect_chat(&mut bob, "Your rank is now").await?;
    idle(&mut bob, Duration::from_millis(500)).await?;
    bob.command("rank").await?;
    expect_chat(&mut bob, "256×256 maps").await?;
    bob.command(&format!("rank set {b} admin")).await?;
    expect_chat(&mut bob, "Usage").await?;

    println!("== metrics");
    expect_metrics("127.0.0.1:9101", &["mm_lobby_players", "mm_shard_tick_p99_ms{shard=\"1\"", "mm_held_games"]).await?;
    expect_metrics(
        "127.0.0.1:9111",
        &["mm_tick_ms{quantile=\"0.99\"}", "mm_instances{kind=\"open_world\"}", "mm_script_text_logged_total"],
    )
    .await?;

    println!("moderation scenario passed");
    Ok(())
}
