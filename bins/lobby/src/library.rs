//! Saves what players do in the browser: favorites, the games they join,
//! and their ratings. A player back from a game they haven't rated gets a
//! clickable ★ prompt, which runs `/rate <game> <stars>`.

use std::collections::HashMap;

use chunkedge::UniqueId;
use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_proto::GameId;
use mm_ui::MenuCommandsExt;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::account::Db;
use crate::catalog::{GameCatalog, PlayerGames};
use crate::menus::{self, LobbyAction};
use crate::ui::CurrentView;

/// A player was handed off to a published game.
#[derive(Message, Clone, Copy, Debug)]
pub struct Played {
    /// Their lobby connection, which is about to close.
    pub client: Entity,
    pub uuid: Uuid,
    pub game: GameId,
}

#[derive(Message, Clone, Copy, Debug)]
pub struct FavoriteChanged {
    pub uuid: Uuid,
    pub game: GameId,
    pub on: bool,
}

#[derive(Message, Clone, Copy, Debug)]
pub struct RateRequest {
    pub client: Entity,
    pub game: GameId,
    pub stars: u8,
}

enum LibraryEvent {
    FirstPlay(GameId),
    Rated {
        client: Entity,
        game: GameId,
        stars: u8,
        /// The game's new mean and count; `None` if the player never played it.
        rating: Option<(Option<f32>, u32)>,
    },
    RateFailed(Entity),
}

#[derive(Resource)]
struct LibraryEvents {
    tx: UnboundedSender<LibraryEvent>,
    rx: UnboundedReceiver<LibraryEvent>,
}

/// Players sent to a game, to ask for a rating when they come back (on a
/// new connection, not the one they left from).
#[derive(Resource, Default)]
struct AskToRate(HashMap<Uuid, (GameId, Entity)>);

pub struct LibraryPlugin;

impl Plugin for LibraryPlugin {
    fn build(&self, app: &mut App) {
        let (tx, rx) = mpsc::unbounded_channel();
        app.insert_resource(LibraryEvents { tx, rx })
            .init_resource::<AskToRate>()
            .add_message::<Played>()
            .add_message::<FavoriteChanged>()
            .add_message::<RateRequest>()
            .add_systems(
                Update,
                (record_plays, save_favorites, rate_command, save_ratings, apply_events, ask_to_rate),
            );
    }
}

fn record_plays(
    mut played: MessageReader<Played>,
    mut ask: ResMut<AskToRate>,
    db: Res<Db>,
    events: Res<LibraryEvents>,
    tokio: Res<Tokio>,
) {
    for &Played { client, uuid, game } in played.read() {
        ask.0.insert(uuid, (game, client));
        let (pool, tx) = (db.0.clone(), events.tx.clone());
        tokio.0.spawn(async move {
            match mm_db::library::record_play(&pool, uuid, game.0).await {
                Ok(true) => {
                    let _ = tx.send(LibraryEvent::FirstPlay(game));
                }
                Ok(false) => {}
                Err(e) => tracing::error!("recording a play of game {} by {uuid}: {e}", game.0),
            }
        });
    }
}

fn save_favorites(mut changes: MessageReader<FavoriteChanged>, db: Res<Db>, tokio: Res<Tokio>) {
    for &FavoriteChanged { uuid, game, on } in changes.read() {
        let pool = db.0.clone();
        tokio.0.spawn(async move {
            if let Err(e) = mm_db::library::set_favorite(&pool, uuid, game.0, on).await {
                tracing::error!("saving favorite {} for {uuid}: {e}", game.0);
            }
        });
    }
}

/// `/rate <stars>` rates the last game played; `/rate <game id> <stars>`
/// (from the clickable prompt) a specific one.
fn rate_command(
    mut messages: MessageReader<CommandExecutionMessage>,
    mut players: Query<(&mut Client, &PlayerGames)>,
    mut rates: MessageWriter<RateRequest>,
) {
    for cmd in messages.read() {
        let mut args = cmd.command.split_whitespace();
        if args.next() != Some("rate") {
            continue;
        }
        let Ok((mut client, games)) = players.get_mut(cmd.executor) else {
            continue;
        };
        let args: Vec<_> = args.collect();
        let parsed = match args[..] {
            [stars] => stars.parse().ok().map(|s| (games.recent.front().copied(), s)),
            [game, stars] => game.parse().ok().zip(stars.parse().ok()).map(|(g, s)| (Some(GameId(g)), s)),
            _ => None,
        };
        match parsed {
            Some((Some(game), stars @ 1..=5)) => {
                rates.write(RateRequest {
                    client: cmd.executor,
                    game,
                    stars,
                });
            }
            Some((None, _)) => client.send_chat_message("§cPlay a game first, then rate it."),
            _ => client.send_chat_message("§7Usage: §f/rate <1-5>§7 rates the last game you played"),
        }
    }
}

fn save_ratings(
    mut requests: MessageReader<RateRequest>,
    mut players: Query<(&mut Client, &UniqueId, &PlayerGames)>,
    catalog: Res<GameCatalog>,
    db: Res<Db>,
    events: Res<LibraryEvents>,
    tokio: Res<Tokio>,
) {
    for &RateRequest { client, game, stars } in requests.read() {
        let Ok((mut c, uuid, games)) = players.get_mut(client) else {
            continue;
        };
        if catalog.get(game).is_none() {
            c.send_chat_message("§cThat game isn't listed anymore.");
            continue;
        }
        if games.loaded && !games.played.contains(&game) {
            c.send_chat_message("§cPlay a game before rating it.");
            continue;
        }
        let (pool, tx, uuid) = (db.0.clone(), events.tx.clone(), uuid.0);
        tokio.0.spawn(async move {
            let event = match mm_db::library::rate(&pool, uuid, game.0, i16::from(stars)).await {
                Ok(summary) => LibraryEvent::Rated {
                    client,
                    game,
                    stars,
                    rating: summary.map(|s| (s.mean.map(|m| m as f32), s.count as u32)),
                },
                Err(e) => {
                    tracing::error!("saving a rating of game {} by {uuid}: {e}", game.0);
                    LibraryEvent::RateFailed(client)
                }
            };
            let _ = tx.send(event);
        });
    }
}

fn stars(n: u8) -> String {
    format!("§6{}§8{}", "★".repeat(n.into()), "★".repeat(5 - usize::from(n)))
}

fn apply_events(
    mut commands: Commands,
    mut events: ResMut<LibraryEvents>,
    mut catalog: ResMut<GameCatalog>,
    mut players: Query<(&mut Client, &mut PlayerGames, Option<&CurrentView>)>,
) {
    while let Ok(event) = events.rx.try_recv() {
        match event {
            LibraryEvent::FirstPlay(game) => catalog.add_player(game),
            LibraryEvent::Rated {
                client,
                game,
                stars: n,
                rating,
            } => {
                let Ok((mut c, mut games, view)) = players.get_mut(client) else {
                    continue;
                };
                let Some((mean, count)) = rating else {
                    c.send_chat_message("§cPlay a game before rating it.");
                    continue;
                };
                catalog.set_rating(game, mean, count);
                games.ratings.insert(game, n);
                let name = catalog.get(game).map_or("the game", |g| g.name.as_str());
                c.send_chat_message(format!("§7You rated §a{name} {}§7. Thanks!", stars(n)));
                // Show the new stars if they're looking at the game.
                if let Some(CurrentView(view @ LobbyAction::GameInfo(id))) = view
                    && *id == game
                    && let Some(menu) = menus::render(view, &catalog, &games)
                {
                    commands.open_menu(client, menu);
                }
            }
            LibraryEvent::RateFailed(client) => {
                if let Ok((mut c, ..)) = players.get_mut(client) {
                    c.send_chat_message("§cCouldn't save your rating right now. Try again later.");
                }
            }
        }
    }
}

/// Once a returning player's account has loaded, asks them to rate the game
/// they just left, unless they rated it before.
fn ask_to_rate(
    mut ask: ResMut<AskToRate>,
    mut players: Query<(Entity, &mut Client, &UniqueId, &PlayerGames), Changed<PlayerGames>>,
    catalog: Res<GameCatalog>,
) {
    if ask.0.is_empty() {
        return;
    }
    for (entity, mut client, uuid, games) in &mut players {
        if !games.loaded {
            continue;
        }
        match ask.0.get(&uuid.0) {
            Some(&(_, left_from)) if left_from != entity => {}
            _ => continue,
        }
        let Some((game, _)) = ask.0.remove(&uuid.0) else {
            continue;
        };
        let Some(summary) = catalog.get(game) else {
            continue;
        };
        if games.ratings.contains_key(&game) {
            continue;
        }
        let mut text = Text::text(format!("§7How was §a{}§7? ", summary.name));
        for n in 1..=5u8 {
            text += Text::text("★ ")
                .color(Color::GOLD)
                .on_click_run_command(format!("/rate {} {n}", game.0))
                .on_hover_show_text(format!("§7Rate it {}", stars(n)));
        }
        client.send_chat_message(text + Text::text("§8(click)"));
    }
}
