//! Moderation in the lobby: reports (sent here and from games), the `/mod`
//! queue menu and its actions, creator bans, and `/rank`. Database work
//! runs on tokio and comes back as a list of [`Effect`]s.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chunkedge::UniqueId;
use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_db::moderation::{self as db, NewReport, QueueEntry, ReportOutcome};
use mm_proto::ipc::LobbyMsg;
use mm_proto::{GameId, GameSummary, Hold};
use mm_ui::{Menu, MenuCommandsExt, icon};
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::account::{DEFAULT_RANK, Db, Ranks, RefreshAccount};
use crate::catalog::{GameCatalog, PlayerGames};
use crate::coordinator::Coordinator;
use crate::menus::{CLOSE, EMPTY_SLOT, GRID, LobbyAction, NEXT, PER_PAGE, PREV, ROWS};
use crate::ui::CurrentView;

const QUEUE_LIMIT: i64 = 280;
const REPORT_COOLDOWN: Duration = Duration::from_secs(60);
pub const REPORT_REASON_LEN: std::ops::RangeInclusive<usize> = 3..=200;
const BAN_REASON_LEN: std::ops::RangeInclusive<usize> = 3..=200;

#[derive(Resource, Deserialize, Clone, Copy)]
#[serde(default, deny_unknown_fields)]
pub struct ModerationConfig {
    /// Open reports from different players that put a game on hold.
    pub hold_at_reports: i64,
    /// Days of script text kept in the audit log.
    pub text_log_days: i32,
}

impl Default for ModerationConfig {
    fn default() -> Self {
        Self {
            hold_at_reports: 5,
            text_log_days: 30,
        }
    }
}

/// File a report (from `/report` here or in a game).
#[derive(Message, Clone, Debug)]
pub struct FileReport {
    /// Who to answer, when the reporter is in the lobby.
    pub client: Option<Entity>,
    pub reporter: Uuid,
    pub game: GameId,
    pub version: Option<u32>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModAction {
    Queue { page: usize },
    Game(GameId),
    Reports(GameId),
    TextLog(GameId),
    /// Resolve the reports and lift the hold.
    Clear(GameId),
    Hold(GameId),
    /// Take down (shift-click).
    Remove(GameId),
    /// Stop every running instance.
    Kill(GameId),
    /// Ban the owner from creating (shift-click).
    BanOwner(GameId),
}

/// A moderation menu click.
#[derive(Message, Clone, Debug)]
pub struct ModClick {
    pub client: Entity,
    pub action: ModAction,
    pub shift: bool,
}

/// Open reports and games on hold, for metrics.
#[derive(Resource, Default, Clone, Copy)]
pub struct ModCounts {
    pub open_reports: i64,
    pub held_games: i64,
}

/// The queue as last loaded for this moderator (pages render from it).
#[derive(Component, Default)]
struct ModQueue(Vec<QueueEntry>);

/// What a finished database task changes.
enum Effect {
    Say(Entity, String),
    /// Chat to a player by UUID, if they're in the lobby.
    Notify(Uuid, String),
    /// Chat to every moderator in the lobby.
    ToMods(String),
    /// Reload a player's rank, ban and games, if they're in the lobby.
    Refresh(Uuid),
    /// List (or relist) a game as it now is; held games are left out.
    Relist(Box<GameSummary>),
    Unlist(GameId),
    Kill(GameId),
    OpenQueue(Entity, Vec<QueueEntry>),
    OpenGame(Entity, Box<QueueEntry>),
    Counts(ModCounts),
}

#[derive(Resource)]
struct Effects {
    tx: UnboundedSender<Vec<Effect>>,
    rx: UnboundedReceiver<Vec<Effect>>,
}

#[derive(Resource, Default)]
struct ReportCooldowns(HashMap<Uuid, Instant>);

pub struct ModerationPlugin;

impl Plugin for ModerationPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(Effects { tx, rx })
            .init_resource::<ReportCooldowns>()
            .init_resource::<ModCounts>()
            .add_message::<FileReport>()
            .add_message::<ModClick>()
            .add_systems(Update, (commands, file_reports, clicks, maintenance, apply_effects).chain());
    }
}

pub fn ban_message(ban: &db::Ban) -> String {
    let until = match ban.until {
        Some(t) => format!(" for {} more", duration_text(t - now_unix())),
        None => String::new(),
    };
    format!("§cYou're banned from creating games{until}: §7{}", ban.reason)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn duration_text(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn is_mod(ranks: &Ranks, games: &PlayerGames) -> bool {
    ranks.limits(games.rank.as_deref()).moderator
}

/// Runs `task` on tokio and applies its effects next tick; a database error
/// tells `client` (if any) it failed.
fn spawn<F>(tokio: &Tokio, effects: &Effects, client: Option<Entity>, what: &'static str, task: F)
where
    F: Future<Output = Result<Vec<Effect>, mm_db::Error>> + Send + 'static,
{
    let tx = effects.tx.clone();
    tokio.0.spawn(async move {
        let effects = task.await.unwrap_or_else(|e| {
            tracing::error!("{what}: {e}");
            client
                .map(|c| vec![Effect::Say(c, format!("§cCouldn't {what} right now. Try again later."))])
                .unwrap_or_default()
        });
        let _ = tx.send(effects);
    });
}

#[allow(clippy::too_many_arguments)]
fn commands(
    mut messages: MessageReader<CommandExecutionMessage>,
    mut players: Query<(&mut Client, &UniqueId, &Username, &PlayerGames)>,
    mut reports: MessageWriter<FileReport>,
    ranks: Res<Ranks>,
    db: Res<Db>,
    effects: Res<Effects>,
    tokio: Res<Tokio>,
) {
    for cmd in messages.read() {
        let mut args = cmd.command.split_whitespace();
        let Some(name) = args.next() else { continue };
        if !["report", "mod", "creatorban", "creatorunban", "rank"].contains(&name) {
            continue;
        }
        let Ok((mut client, uuid, username, games)) = players.get_mut(cmd.executor) else {
            continue;
        };
        let (me, executor, pool) = (uuid.0, cmd.executor, db.0.clone());
        let staff = ranks.limits(games.rank.as_deref());
        let args: Vec<&str> = args.collect();
        if name != "report" && name != "rank" && !staff.moderator {
            client.send_chat_message("§cOnly moderators can do that.");
            continue;
        }
        match (name, args.as_slice()) {
            ("report", [id, reason @ ..]) if id.parse::<i64>().is_ok() && !reason.is_empty() => {
                reports.write(FileReport {
                    client: Some(executor),
                    reporter: me,
                    game: GameId(id.parse().unwrap()),
                    version: None,
                    reason: reason.join(" "),
                });
            }
            ("report", _) => client.send_chat_message(
                "§7Shift-click a game in the browser and use §cReport§7, or §f/report <reason>§7 while playing it.",
            ),
            ("mod", []) => open_queue(&tokio, &effects, executor, pool),
            ("mod", ["game", id]) => match id.trim_start_matches('#').parse() {
                Ok(id) => open_game(&tokio, &effects, executor, pool, GameId(id)),
                Err(_) => client.send_chat_message("§7Usage: §f/mod game <id>"),
            },
            ("mod", _) => client.send_chat_message("§7Usage: §f/mod§7 (the queue) or §f/mod game <id>"),
            ("creatorban", [target, rest @ ..]) => {
                let (days, reason) = match rest.first().and_then(|d| d.strip_suffix('d')).and_then(|d| d.parse().ok()) {
                    Some(days) => (Some(days), rest[1..].join(" ")),
                    None => (None, rest.join(" ")),
                };
                if !BAN_REASON_LEN.contains(&reason.chars().count()) || days.is_some_and(|d: i32| d <= 0) {
                    client.send_chat_message("§7Usage: §f/creatorban <player> [<days>d] <reason>");
                    continue;
                }
                let target = (*target).to_owned();
                spawn(&tokio, &effects, Some(executor), "ban that creator", async move {
                    let Some((player, name)) = db::find_player(&pool, &target).await? else {
                        return Ok(vec![Effect::Say(executor, format!("§cNo player named {target} has joined."))]);
                    };
                    let banned = db::ban_creator(&pool, player, &reason, days, me).await?;
                    Ok(ban_effects(executor, player, &name, &reason, days, banned))
                });
            }
            ("creatorunban", [target]) => {
                let target = (*target).to_owned();
                spawn(&tokio, &effects, Some(executor), "lift that ban", async move {
                    let Some((player, name)) = db::find_player(&pool, &target).await? else {
                        return Ok(vec![Effect::Say(executor, format!("§cNo player named {target} has joined."))]);
                    };
                    Ok(if db::unban_creator(&pool, player, me).await? {
                        vec![
                            Effect::Say(executor, format!("§aLifted {name}'s creator ban. Their games stay held until cleared.")),
                            Effect::Notify(player, "§aYour creator ban was lifted.".to_owned()),
                            Effect::Refresh(player),
                        ]
                    } else {
                        vec![Effect::Say(executor, format!("§7{name} isn't banned."))]
                    })
                });
            }
            ("creatorban" | "creatorunban", _) => client.send_chat_message(
                "§7Usage: §f/creatorban <player> [<days>d] <reason>§7 or §f/creatorunban <player>",
            ),
            ("rank", []) => {
                let rank = games.rank.as_deref().unwrap_or(DEFAULT_RANK);
                let size = staff.map_size;
                client.send_chat_message(format!(
                    "§7{}, your rank is §f{rank}§7: {size}×{size} maps, {} games, {} MB script memory, {} MB DataStores",
                    username.0, staff.max_drafts, staff.memory_mb, staff.datastore_mb
                ));
            }
            ("rank", [target]) if staff.moderator => {
                let target = (*target).to_owned();
                spawn(&tokio, &effects, Some(executor), "look up that rank", async move {
                    let Some((player, name)) = db::find_player(&pool, &target).await? else {
                        return Ok(vec![Effect::Say(executor, format!("§cNo player named {target} has joined."))]);
                    };
                    let rank = db::rank(&pool, player).await?.unwrap_or_else(|| DEFAULT_RANK.to_owned());
                    let ban = db::creator_ban(&pool, player).await?;
                    let mut text = format!("§7{name}: rank §f{rank}");
                    if let Some(ban) = ban {
                        text += &format!(" §c(creator ban: {})", ban.reason);
                    }
                    Ok(vec![Effect::Say(executor, text)])
                });
            }
            ("rank", ["set", target, rank]) if staff.admin => {
                let rank = match *rank {
                    r if r == DEFAULT_RANK || r == "default" => None,
                    r if ranks.0.contains_key(r) => Some(r.to_owned()),
                    r => {
                        let mut names: Vec<_> = ranks.0.keys().map(String::as_str).collect();
                        names.sort_unstable();
                        client.send_chat_message(format!("§cNo rank `{r}`. Ranks: §f{}", names.join(", ")));
                        continue;
                    }
                };
                let target = (*target).to_owned();
                spawn(&tokio, &effects, Some(executor), "set that rank", async move {
                    let Some((player, name)) = db::find_player(&pool, &target).await? else {
                        return Ok(vec![Effect::Say(executor, format!("§cNo player named {target} has joined."))]);
                    };
                    db::set_rank(&pool, player, rank.as_deref(), Some(me)).await?;
                    let shown = rank.as_deref().unwrap_or(DEFAULT_RANK);
                    Ok(vec![
                        Effect::Say(executor, format!("§a{name} is now §f{shown}§a.")),
                        Effect::Notify(player, format!("§aYour rank is now §f{shown}§a.")),
                        Effect::Refresh(player),
                    ])
                });
            }
            ("rank", _) => client.send_chat_message(if staff.admin {
                "§7Usage: §f/rank§7, §f/rank <player>§7 or §f/rank set <player> <rank>"
            } else if staff.moderator {
                "§7Usage: §f/rank§7 or §f/rank <player>"
            } else {
                "§7Usage: §f/rank"
            }),
            _ => {}
        }
    }
}

fn ban_effects(
    moderator: Entity,
    player: Uuid,
    name: &str,
    reason: &str,
    days: Option<i32>,
    games: Vec<i64>,
) -> Vec<Effect> {
    let length = days.map_or("until lifted".to_owned(), |d| format!("for {d} days"));
    let mut effects = vec![
        Effect::Say(
            moderator,
            format!("§aBanned {name} from creating {length}; their {} game(s) are held and stopped.", games.len()),
        ),
        Effect::ToMods(format!("§c[Mod] §f{name}§c was banned from creating: §7{reason}")),
        Effect::Notify(player, format!("§cYou were banned from creating games {length}: §7{reason}")),
        Effect::Refresh(player),
    ];
    for game in games {
        effects.push(Effect::Unlist(GameId(game)));
        effects.push(Effect::Kill(GameId(game)));
    }
    effects
}

fn open_queue(tokio: &Tokio, effects: &Effects, client: Entity, pool: mm_db::Pool) {
    spawn(tokio, effects, Some(client), "load the moderation queue", async move {
        Ok(vec![Effect::OpenQueue(client, db::queue(&pool, QUEUE_LIMIT).await?)])
    });
}

fn open_game(tokio: &Tokio, effects: &Effects, client: Entity, pool: mm_db::Pool, game: GameId) {
    spawn(tokio, effects, Some(client), "load that game", async move {
        Ok(vec![match db::queue_entry(&pool, game.0).await? {
            Some(entry) => Effect::OpenGame(client, Box::new(entry)),
            None => Effect::Say(client, format!("§cThere's no game #{}.", game.0)),
        }])
    });
}

#[allow(clippy::too_many_arguments)]
fn file_reports(
    mut reports: MessageReader<FileReport>,
    mut clients: Query<&mut Client>,
    mut cooldowns: ResMut<ReportCooldowns>,
    config: Res<ModerationConfig>,
    catalog: Res<GameCatalog>,
    db: Res<Db>,
    effects: Res<Effects>,
    tokio: Res<Tokio>,
) {
    let now = Instant::now();
    cooldowns.0.retain(|_, until| *until > now);
    for report in reports.read() {
        let mut say = |text: &str| {
            if let Some(mut c) = report.client.and_then(|c| clients.get_mut(c).ok()) {
                c.send_chat_message(text.to_owned());
            }
        };
        let reason = report.reason.replace('§', "");
        let reason = reason.trim();
        if !REPORT_REASON_LEN.contains(&reason.chars().count()) {
            say(&format!(
                "§cReasons must be {}-{} characters.",
                REPORT_REASON_LEN.start(),
                REPORT_REASON_LEN.end()
            ));
            continue;
        }
        // Shards rate-limit their own reports; this covers the lobby too.
        if report.client.is_some() && cooldowns.0.contains_key(&report.reporter) {
            say("§cYou just sent a report. Wait a minute before sending another.");
            continue;
        }
        cooldowns.0.insert(report.reporter, now + REPORT_COOLDOWN);
        let (pool, report, reason, hold_at) = (db.0.clone(), report.clone(), reason.to_owned(), config.hold_at_reports);
        let name = catalog.get(report.game).map(|g| g.name.clone());
        spawn(&tokio, &effects, report.client, "send your report", async move {
            let new = NewReport {
                game: report.game.0,
                reporter: report.reporter,
                reason: &reason,
                version: report.version.map(|v| v as i32),
            };
            let outcome = db::report(&pool, new, hold_at).await?;
            let reply = |text: &str| report.client.map(|c| Effect::Say(c, text.to_owned()));
            let name = name.unwrap_or_else(|| format!("game #{}", report.game.0));
            Ok(match outcome {
                ReportOutcome::Filed { open, held } => {
                    tracing::info!("report on game {} ({open} open): {reason}", report.game.0);
                    let mut effects: Vec<_> = reply("§aThanks, your report was sent to the moderators.").into_iter().collect();
                    effects.push(Effect::ToMods(format!(
                        "§e[Mod] §f{name}§e reported ({open} open): §7{reason} §8(/mod)"
                    )));
                    if held {
                        effects.push(Effect::Unlist(report.game));
                        effects.push(Effect::ToMods(format!(
                            "§c[Mod] §f{name}§c is held for review after {open} reports."
                        )));
                    }
                    effects
                }
                ReportOutcome::Duplicate => reply("§7You already reported this game; the moderators will look at it.")
                    .into_iter()
                    .collect(),
                ReportOutcome::OwnGame => reply("§7You can't report your own game.").into_iter().collect(),
                ReportOutcome::NoGame => reply("§cThere's no such game.").into_iter().collect(),
            })
        });
    }
}

fn clicks(
    mut clicks: MessageReader<ModClick>,
    mut players: Query<(&mut Client, &UniqueId, &PlayerGames, Option<&ModQueue>)>,
    mut commands: Commands,
    ranks: Res<Ranks>,
    db: Res<Db>,
    effects: Res<Effects>,
    tokio: Res<Tokio>,
) {
    for ModClick { client, action, shift } in clicks.read() {
        let Ok((mut c, uuid, games, queue)) = players.get_mut(*client) else {
            continue;
        };
        if !is_mod(&ranks, games) {
            commands.close_menu(*client);
            continue;
        }
        let (client, me, pool) = (*client, uuid.0, db.0.clone());
        match action {
            // The first page reloads; other pages show the loaded queue.
            ModAction::Queue { page } => match queue {
                Some(q) if *page > 0 => {
                    commands.open_menu(client, queue_menu(&q.0, *page));
                }
                _ => open_queue(&tokio, &effects, client, pool),
            },
            ModAction::Game(game) => open_game(&tokio, &effects, client, pool, *game),
            ModAction::Reports(game) => {
                let game = *game;
                spawn(&tokio, &effects, Some(client), "load the reports", async move {
                    let rows = db::open_reports(&pool, game.0, 10).await?;
                    let mut out = vec![Effect::Say(client, format!("§e§lOpen reports on #{}§7 ({} newest)", game.0, rows.len()))];
                    out.extend(rows.into_iter().map(|r| {
                        let version = r.version.map_or("lobby".to_owned(), |v| format!("v{v}"));
                        Effect::Say(
                            client,
                            format!("§8{} ago §7{} §8({version})§7: §f{}", duration_text(r.age_secs), r.reporter, r.reason),
                        )
                    }));
                    Ok(out)
                });
            }
            ModAction::TextLog(game) => {
                let game = *game;
                spawn(&tokio, &effects, Some(client), "load the script text log", async move {
                    let rows = db::text_log(&pool, game.0, 15).await?;
                    let mut out = vec![Effect::Say(client, format!("§e§lScript text in #{}§7 ({} newest)", game.0, rows.len()))];
                    out.extend(rows.into_iter().rev().map(|l| {
                        let version = l.version.map_or("draft".to_owned(), |v| format!("v{v}"));
                        let to = l.player.unwrap_or_else(|| "?".to_owned());
                        // Show the codes rather than apply them.
                        let text = l.text.replace('§', "&");
                        Effect::Say(
                            client,
                            format!("§8{} ago {version} {} → {to}: §f{text}", duration_text(l.age_secs), l.kind),
                        )
                    }));
                    Ok(out)
                });
            }
            ModAction::Clear(game) => {
                let game = *game;
                spawn(&tokio, &effects, Some(client), "clear the game", async move {
                    let Some(row) = db::clear(&pool, game.0, me).await? else {
                        return Ok(vec![Effect::Say(client, "§cThat game no longer exists.".to_owned())]);
                    };
                    let mut out = vec![
                        Effect::Say(client, format!("§aCleared §f{}§a: reports resolved, hold lifted.", row.name)),
                        Effect::Relist(Box::new(row.summary())),
                    ];
                    out.extend(db::queue_entry(&pool, game.0).await?.map(|e| Effect::OpenGame(client, Box::new(e))));
                    Ok(out)
                });
            }
            ModAction::Hold(game) | ModAction::Remove(game) => {
                let hold = if matches!(action, ModAction::Remove(_)) { Hold::Removed } else { Hold::Review };
                if hold == Hold::Removed && !shift {
                    c.send_action_bar_message("§cShift-click to take the game down");
                    continue;
                }
                let game = *game;
                spawn(&tokio, &effects, Some(client), "hold the game", async move {
                    let reason = match hold {
                        Hold::Review => "held by a moderator",
                        Hold::Removed => "taken down by a moderator",
                    };
                    if !db::set_hold(&pool, game.0, hold, reason, me).await? {
                        return Ok(vec![Effect::Say(client, "§cThat game no longer exists.".to_owned())]);
                    }
                    let mut out = vec![Effect::Unlist(game)];
                    if hold == Hold::Removed {
                        out.push(Effect::Kill(game));
                        out.push(Effect::Say(client, "§aTaken down and stopped.".to_owned()));
                    } else {
                        out.push(Effect::Say(client, "§aHeld for review; it's out of the browser.".to_owned()));
                    }
                    out.extend(db::queue_entry(&pool, game.0).await?.map(|e| Effect::OpenGame(client, Box::new(e))));
                    Ok(out)
                });
            }
            ModAction::Kill(game) => {
                let game = *game;
                spawn(&tokio, &effects, Some(client), "stop the game", async move {
                    db::log_kill(&pool, game.0, me).await?;
                    Ok(vec![Effect::Kill(game), Effect::Say(client, "§aStopped its running instances.".to_owned())])
                });
            }
            ModAction::BanOwner(game) => {
                if !shift {
                    c.send_action_bar_message("§cShift-click to ban the creator");
                    continue;
                }
                let game = *game;
                spawn(&tokio, &effects, Some(client), "ban that creator", async move {
                    let Some(row) = mm_db::game(&pool, game.0).await? else {
                        return Ok(vec![Effect::Say(client, "§cThat game no longer exists.".to_owned())]);
                    };
                    let reason = format!("game #{} ({})", game.0, row.name);
                    let banned = db::ban_creator(&pool, row.owner, &reason, None, me).await?;
                    Ok(ban_effects(client, row.owner, &row.owner_name, &reason, None, banned))
                });
            }
        }
    }
}

/// Hourly: the audit log's partitions. Every minute: counts for metrics.
fn maintenance(
    mut last: Local<Option<(Instant, Instant)>>,
    config: Res<ModerationConfig>,
    db: Res<Db>,
    effects: Res<Effects>,
    tokio: Res<Tokio>,
) {
    let now = Instant::now();
    let (log_at, counts_at) = last.get_or_insert((now, now - Duration::from_secs(60)));
    if now >= *log_at {
        *log_at = now + Duration::from_secs(3600);
        let (pool, days) = (db.0.clone(), config.text_log_days);
        spawn(&tokio, &effects, None, "maintain the script text log", async move {
            db::maintain_text_log(&pool, days).await?;
            Ok(Vec::new())
        });
    }
    if now >= *counts_at {
        *counts_at = now + Duration::from_secs(60);
        let pool = db.0.clone();
        spawn(&tokio, &effects, None, "count reports", async move {
            let (open_reports, held_games) = db::counts(&pool).await?;
            Ok(vec![Effect::Counts(ModCounts {
                open_reports,
                held_games,
            })])
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_effects(
    mut commands: Commands,
    mut effects: ResMut<Effects>,
    mut players: Query<(Entity, &mut Client, &UniqueId, &PlayerGames)>,
    mut catalog: ResMut<GameCatalog>,
    mut refreshes: MessageWriter<RefreshAccount>,
    mut counts: ResMut<ModCounts>,
    coordinator: Res<Coordinator>,
    ranks: Res<Ranks>,
) {
    while let Ok(batch) = effects.rx.try_recv() {
        for effect in batch {
            match effect {
                Effect::Say(client, text) => {
                    if let Ok((_, mut c, ..)) = players.get_mut(client) {
                        c.send_chat_message(text);
                    }
                }
                Effect::Notify(uuid, text) => {
                    for (_, mut c, id, _) in &mut players {
                        if id.0 == uuid {
                            c.send_chat_message(text.clone());
                        }
                    }
                }
                Effect::ToMods(text) => {
                    for (_, mut c, _, games) in &mut players {
                        if is_mod(&ranks, games) {
                            c.send_chat_message(text.clone());
                        }
                    }
                }
                Effect::Refresh(uuid) => {
                    for (entity, _, id, _) in &players {
                        if id.0 == uuid {
                            refreshes.write(RefreshAccount(entity));
                        }
                    }
                }
                Effect::Relist(game) => catalog.upsert(*game),
                Effect::Unlist(game) => catalog.remove(game),
                Effect::Kill(game) => {
                    tracing::info!("moderation: stopping game {}", game.0);
                    coordinator.broadcast(LobbyMsg::KillGame { game });
                }
                Effect::OpenQueue(client, queue) => {
                    commands.open_menu(client, queue_menu(&queue, 0));
                    commands.entity(client).remove::<CurrentView>().insert(ModQueue(queue));
                }
                Effect::OpenGame(client, entry) => {
                    commands.open_menu(client, game_menu(&entry));
                    commands.entity(client).remove::<CurrentView>();
                }
                Effect::Counts(c) => *counts = c,
            }
        }
    }
}

fn queue_menu(queue: &[QueueEntry], page: usize) -> Menu<LobbyAction> {
    let mut menu = Menu::new("Moderation Queue", ROWS);
    menu.item(
        4,
        icon(
            ItemKind::Bell,
            "§cModeration Queue",
            [
                Text::text(format!("§7{} game(s) need a look", queue.len())),
                Text::text("§8Held games first, then most reported"),
            ],
        ),
    );
    if queue.is_empty() {
        menu.item(EMPTY_SLOT, icon(ItemKind::LimeDye, "§aNothing to review", []));
    }
    let pages = queue.len().div_ceil(PER_PAGE).max(1);
    let page = page.min(pages - 1);
    for (slot, entry) in GRID.iter().zip(queue.iter().skip(page * PER_PAGE)) {
        let game = &entry.game;
        let kind = ItemKind::from_str(&game.icon).unwrap_or(ItemKind::Paper);
        let mut lore = vec![Text::text(format!("§7by §f{} §8· #{}", game.owner_name, game.id))];
        lore.extend(status_lines(entry));
        lore.extend([Text::default(), Text::text("§eClick to review")]);
        menu.button(*slot, icon(kind, format!("§a{}", game.name), lore), LobbyAction::Mod(ModAction::Game(GameId(game.id))));
    }
    if page > 0 {
        menu.button(
            PREV,
            icon(ItemKind::Arrow, "§7← Previous page", []),
            LobbyAction::Mod(ModAction::Queue { page: page - 1 }),
        );
    }
    if page + 1 < pages {
        menu.button(
            NEXT,
            icon(ItemKind::Arrow, "§7Next page →", []),
            LobbyAction::Mod(ModAction::Queue { page: page + 1 }),
        );
    }
    menu.button(CLOSE, icon(ItemKind::Barrier, "§cClose", []), LobbyAction::Close);
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |slot| !GRID.contains(&slot));
    menu
}

fn status_lines(entry: &QueueEntry) -> Vec<Text> {
    let mut lines = Vec::new();
    match entry.game.hold() {
        Some(Hold::Review) => lines.push(Text::text(format!(
            "§6On hold: §7{}",
            entry.hold_reason.as_deref().unwrap_or("review")
        ))),
        Some(Hold::Removed) => lines.push(Text::text(format!(
            "§cTaken down: §7{}",
            entry.hold_reason.as_deref().unwrap_or("removed")
        ))),
        None if entry.game.published_version.is_some() => lines.push(Text::text("§aListed")),
        None => lines.push(Text::text("§8Not published")),
    }
    lines.push(Text::text(format!("§c{} open report(s)", entry.open_reports)));
    if let Some(reason) = &entry.latest_reason {
        lines.push(Text::text(format!("§7Latest: §f\"{}\"", truncate(reason, 40))));
    }
    lines
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(max - 1).collect::<String>())
}

fn game_menu(entry: &QueueEntry) -> Menu<LobbyAction> {
    let game = &entry.game;
    let id = GameId(game.id);
    let mut menu = Menu::new(format!("Review: {}", game.name), 5);
    let kind = ItemKind::from_str(&game.icon).unwrap_or(ItemKind::Paper);
    let mut lore = vec![
        Text::text(format!("§7by §f{} §8· #{}", game.owner_name, game.id)),
        Text::text(match game.published_version {
            Some(v) => format!("§7Published §fv{v}"),
            None => "§8Not published".to_owned(),
        }),
    ];
    lore.extend(status_lines(entry));
    menu.item(4, icon(kind, format!("§a{}", game.name), lore));

    let button = |kind, name: &'static str, hint: &str| icon(kind, name, [Text::text(format!("§7{hint}"))]);
    let mod_button = |menu: &mut Menu<LobbyAction>, slot, item, action| {
        menu.button(slot, item, LobbyAction::Mod(action));
    };
    mod_button(&mut menu, 19, button(ItemKind::Paper, "§eReports", "Show the open reports in chat"), ModAction::Reports(id));
    mod_button(
        &mut menu,
        20,
        button(ItemKind::WritableBook, "§eScript text", "Show what its scripts recently said"),
        ModAction::TextLog(id),
    );
    mod_button(
        &mut menu,
        21,
        button(ItemKind::LimeDye, "§aClear", "Resolve reports and lift any hold"),
        ModAction::Clear(id),
    );
    mod_button(
        &mut menu,
        22,
        button(ItemKind::OrangeDye, "§6Hold for review", "Take it out of the browser"),
        ModAction::Hold(id),
    );
    mod_button(
        &mut menu,
        23,
        button(ItemKind::Tnt, "§cStop instances", "Send its players to the lobby"),
        ModAction::Kill(id),
    );
    mod_button(
        &mut menu,
        24,
        button(ItemKind::Barrier, "§4Take down", "Shift-click: unlist, stop, block publishing"),
        ModAction::Remove(id),
    );
    mod_button(
        &mut menu,
        25,
        button(ItemKind::IronBars, "§4Ban creator", "Shift-click: ban the owner from creating"),
        ModAction::BanOwner(id),
    );
    mod_button(&mut menu, 40, icon(ItemKind::Bell, "§7Back to the queue", []), ModAction::Queue { page: 0 });
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |_| true);
    menu
}
