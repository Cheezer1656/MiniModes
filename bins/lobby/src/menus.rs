//! Chest-menu layouts for the lobby. Every menu is 6 rows: a header row, a
//! 7×4 grid of games, and a navigation row.

use std::sync::Arc;

use chunkedge::item::ItemComponent;
use chunkedge::prelude::*;
use mm_proto::{GameId, GameKind, GameSummary, LiveUpdate};
use mm_ui::{Menu, icon};

use crate::catalog::{BrowserTab, GameCatalog, PlayerGames};

const ROWS: u8 = 6;
const GRID: [u16; 28] = [
    10, 11, 12, 13, 14, 15, 16, //
    19, 20, 21, 22, 23, 24, 25, //
    28, 29, 30, 31, 32, 33, 34, //
    37, 38, 39, 40, 41, 42, 43,
];
const PER_PAGE: usize = GRID.len();
const EMPTY_SLOT: u16 = 22;

const PREV: u16 = 45;
const SEARCH: u16 = 48;
const CLOSE: u16 = 49;
const SECONDARY: u16 = 50;
const NEXT: u16 = 53;

#[derive(Clone, Debug, PartialEq)]
pub enum LobbyAction {
    Browser { tab: BrowserTab, page: usize },
    MyGames { page: usize },
    Favorites { recent: bool, page: usize },
    SearchResults { query: Arc<str>, page: usize },
    /// Prompt the player to type a search query in chat.
    StartSearch,
    /// Left click plays, right click toggles favorite, shift-click shows
    /// the game's details.
    Game(GameId),
    /// A game's description, favorite toggle and rating.
    GameInfo(GameId),
    ToggleFavorite(GameId),
    Rate { game: GameId, stars: u8 },
    /// Join one of your own games' draft to build (right click: manage it).
    EditDraft(GameId),
    /// Build, publish, settings and play for one of your games.
    ManageGame(GameId),
    /// Pick how a publish reaches a persistent world that's already live.
    PublishModes(GameId),
    Publish { game: GameId, mode: LiveUpdate },
    TogglePlayerSaves(GameId),
    /// Step 1 of creating a game: pick its kind.
    CreateGame,
    /// Step 2: pick an icon.
    CreateKind(GameKind),
    /// Step 3: name it in chat.
    CreateIcon { kind: GameKind, icon: &'static str },
    Close,
}

/// Icons offered when creating a game.
pub const GAME_ICONS: [&str; 21] = [
    "grass_block", "diamond_sword", "bow", "iron_pickaxe", "golden_apple", "tnt", "ender_pearl",
    "oak_sapling", "fishing_rod", "cake", "redstone", "compass", "map", "beacon",
    "netherite_chestplate", "trident", "elytra", "slime_ball", "jukebox", "dragon_head", "shield",
];

/// Renders the menu for a view action. Returns `None` for non-view actions.
pub fn render(
    view: &LobbyAction,
    catalog: &GameCatalog,
    player: &PlayerGames,
) -> Option<Menu<LobbyAction>> {
    Some(match view {
        LobbyAction::Browser { tab, page } => browser(catalog, player, *tab, *page),
        LobbyAction::MyGames { page } => my_games(player, *page),
        LobbyAction::ManageGame(id) => manage(player, player.editable(*id)?),
        LobbyAction::PublishModes(id) => publish_modes(player.owned(*id)?),
        LobbyAction::CreateGame => create_kind(),
        LobbyAction::CreateKind(kind) => create_icon(*kind),
        LobbyAction::Favorites { recent, page } => favorites(catalog, player, *recent, *page),
        LobbyAction::SearchResults { query, page } => search_results(catalog, player, query, *page),
        LobbyAction::GameInfo(id) => game_info(catalog, player, catalog.get(*id)?),
        _ => return None,
    })
}

fn browser(catalog: &GameCatalog, player: &PlayerGames, tab: BrowserTab, page: usize) -> Menu<LobbyAction> {
    let mut menu = Menu::new("Game Browser", ROWS);
    for (slot, t) in [1, 3, 5, 7].into_iter().zip(BrowserTab::ALL) {
        let (kind, name, hint) = match t {
            BrowserTab::Featured => (ItemKind::NetherStar, "§6Featured", "Hand-picked by staff"),
            BrowserTab::Popular => (ItemKind::BlazePowder, "§cPopular", "Most players right now"),
            BrowserTab::Newest => (ItemKind::Clock, "§bNewest", "Recently published"),
            BrowserTab::TopRated => (ItemKind::Diamond, "§aTop Rated", "Highest player ratings"),
        };
        let mut item = icon(kind, name, [Text::text(format!("§7{hint}"))]);
        if t == tab {
            item.insert_component(ItemComponent::EnchantmentGlintOverride(true));
        }
        menu.button(slot, item, LobbyAction::Browser { tab: t, page: 0 });
    }

    let games = catalog.tab(tab);
    game_grid(&mut menu, catalog, player, &games, page, "No games here yet", |page| {
        LobbyAction::Browser { tab, page }
    });
    menu.button(
        SECONDARY,
        icon(ItemKind::NetherStar, "§eFavorites & Recent", []),
        LobbyAction::Favorites { recent: false, page: 0 },
    );
    finish(menu)
}

fn my_games(player: &PlayerGames, page: usize) -> Menu<LobbyAction> {
    let mut menu = Menu::new("My Games", ROWS);
    menu.item(
        4,
        icon(ItemKind::WritableBook, "§bMy Games", [Text::text("§7Games you created or help build")]),
    );
    if !player.loaded {
        menu.item(EMPTY_SLOT, icon(ItemKind::Clock, "§7Loading your games...", []));
    } else if player.owned.is_empty() && player.shared.is_empty() {
        menu.item(EMPTY_SLOT, icon(ItemKind::Barrier, "§cYou haven't created any games", []));
    }
    let games: Vec<_> = player
        .owned
        .iter()
        .map(|g| (g, false))
        .chain(player.shared.iter().map(|g| (g, true)))
        .collect();
    let pages = games.len().div_ceil(PER_PAGE).max(1);
    let page = page.min(pages - 1);
    for (slot, (game, shared)) in GRID.iter().zip(games.iter().skip(page * PER_PAGE)) {
        menu.button(*slot, draft_icon(game, *shared), LobbyAction::EditDraft(game.id));
    }
    if page > 0 {
        menu.button(PREV, icon(ItemKind::Arrow, "§7← Previous page", []), LobbyAction::MyGames { page: page - 1 });
    }
    if page + 1 < pages {
        menu.button(NEXT, icon(ItemKind::Arrow, "§7Next page →", []), LobbyAction::MyGames { page: page + 1 });
    }
    menu.button(
        SECONDARY,
        icon(
            ItemKind::EmeraldBlock,
            "§aCreate a Game",
            [Text::text("§7Start a new draft world")],
        ),
        LobbyAction::CreateGame,
    );
    finish(menu)
}

fn draft_icon(game: &GameSummary, shared: bool) -> ItemStack {
    let kind = ItemKind::from_str(&game.icon).unwrap_or(ItemKind::Paper);
    let mut lore = vec![Text::text(format!("§7{}", kind_label(game.kind)))];
    if shared {
        lore.push(Text::text(format!("§7Owned by §f{}", game.creator)));
    }
    lore.push(Text::text(match game.version {
        0 => "§8Not published yet".to_owned(),
        v => format!("§7Published §fv{v}"),
    }));
    lore.extend([
        Text::default(),
        Text::text("§eClick to build"),
        Text::text("§eRight-click to publish and manage"),
        Text::text("§8/script edit inside opens the editor"),
    ]);
    icon(kind, format!("§a{}", game.name), lore)
}

fn persistent(game: &GameSummary) -> bool {
    game.kind
        == GameKind::OpenWorld {
            persistent_world: true,
        }
}

fn manage(player: &PlayerGames, game: &GameSummary) -> Menu<LobbyAction> {
    let mut menu = Menu::new(format!("Manage: {}", game.name), 3);
    let owner = player.owned(game.id).is_some();
    menu.button(
        10,
        icon(ItemKind::Bricks, "§aBuild", [Text::text("§7Open the draft world")]),
        LobbyAction::EditDraft(game.id),
    );
    if owner {
        let mut lore = vec![Text::text(match game.version {
            0 => "§7Make the draft playable by everyone".to_owned(),
            v => format!("§7Replace v{v} with the current draft"),
        })];
        let modes = persistent(game) && game.version > 0;
        if modes {
            lore.push(Text::text("§7You'll choose what happens to the live world"));
        } else if game.version > 0 {
            lore.push(Text::text("§7Running instances finish on their version"));
        }
        let action = if modes {
            LobbyAction::PublishModes(game.id)
        } else {
            LobbyAction::Publish {
                game: game.id,
                mode: LiveUpdate::Reset,
            }
        };
        menu.button(12, icon(ItemKind::Beacon, format!("§bPublish v{}", game.version + 1), lore), action);
        if matches!(game.kind, GameKind::OpenWorld { .. }) {
            let on = player.player_saves.contains(&game.id);
            let (item, state) = if on {
                (ItemKind::LimeDye, "§aOn")
            } else {
                (ItemKind::GrayDye, "§cOff")
            };
            menu.button(
                14,
                icon(
                    item,
                    format!("§eSave player progress: {state}"),
                    [
                        Text::text("§7Keep each player's inventory and"),
                        Text::text("§7position between visits"),
                        Text::text("§8Click to toggle"),
                    ],
                ),
                LobbyAction::TogglePlayerSaves(game.id),
            );
        }
    } else {
        menu.item(
            12,
            icon(ItemKind::Barrier, "§7Publish", [Text::text(format!("§7Only {} can publish", game.creator))]),
        );
    }
    if game.version > 0 {
        menu.button(16, icon(ItemKind::Emerald, "§aPlay", [Text::text(format!("§7Join v{}", game.version))]), LobbyAction::Game(game.id));
    }
    menu.button(22, icon(ItemKind::Arrow, "§7Back", []), LobbyAction::MyGames { page: 0 });
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |_| true);
    menu
}

fn publish_modes(game: &GameSummary) -> Menu<LobbyAction> {
    let mut menu = Menu::new(format!("Publish v{}", game.version + 1), 3);
    for (slot, item, mode, name, hint) in [
        (11, ItemKind::WritableBook, LiveUpdate::Scripts, "§aScripts only", "Swap the scripts; the world stays as players left it"),
        (
            13,
            ItemKind::GoldenShovel,
            LiveUpdate::Regions,
            "§eScripts + regions",
            "Also copy the draft's //region boxes over the world",
        ),
        (15, ItemKind::Tnt, LiveUpdate::Reset, "§cFull reset", "Back the world up, then replace it with the draft's map"),
    ] {
        let lore = wrap(hint, 32).into_iter().map(|l| Text::text(format!("§7{l}")));
        menu.button(slot, icon(item, name, lore), LobbyAction::Publish { game: game.id, mode });
    }
    menu.button(22, icon(ItemKind::Arrow, "§7Back", []), LobbyAction::ManageGame(game.id));
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |_| true);
    menu
}

fn kind_label(kind: GameKind) -> &'static str {
    match kind {
        GameKind::Match => "Match",
        GameKind::OpenWorld {
            persistent_world: false,
        } => "Open world",
        GameKind::OpenWorld {
            persistent_world: true,
        } => "Persistent world",
    }
}

fn create_kind() -> Menu<LobbyAction> {
    let mut menu = Menu::new("Create a Game", 3);
    for (slot, item, kind, hint) in [
        (11, ItemKind::IronSword, GameKind::Match, "Rounds with a start and an end; the map resets each match"),
        (
            13,
            ItemKind::GrassBlock,
            GameKind::OpenWorld {
                persistent_world: false,
            },
            "Drop in any time; parallel servers each start fresh",
        ),
        (
            15,
            ItemKind::IronPickaxe,
            GameKind::OpenWorld {
                persistent_world: true,
            },
            "One shared world whose changes are saved",
        ),
    ] {
        let lore = wrap(hint, 32).into_iter().map(|l| Text::text(format!("§7{l}")));
        menu.button(slot, icon(item, format!("§a{}", kind_label(kind)), lore), LobbyAction::CreateKind(kind));
    }
    menu.button(22, icon(ItemKind::Barrier, "§cCancel", []), LobbyAction::MyGames { page: 0 });
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |_| true);
    menu
}

fn create_icon(kind: GameKind) -> Menu<LobbyAction> {
    let mut menu = Menu::new("Pick an Icon", ROWS);
    menu.item(4, icon(ItemKind::Paper, format!("§a{}", kind_label(kind)), [Text::text("§7Choose your game's icon")]));
    for (slot, name) in GRID.iter().zip(GAME_ICONS) {
        let item = ItemKind::from_str(name).unwrap_or(ItemKind::Paper);
        menu.button(*slot, icon(item, "§eUse this icon", []), LobbyAction::CreateIcon { kind, icon: name });
    }
    menu.button(SECONDARY, icon(ItemKind::Arrow, "§7Back", []), LobbyAction::CreateGame);
    finish(menu)
}

fn favorites(catalog: &GameCatalog, player: &PlayerGames, recent: bool, page: usize) -> Menu<LobbyAction> {
    let mut menu = Menu::new(if recent { "Recently Played" } else { "Favorites" }, ROWS);
    for (slot, is_recent, kind, name) in [
        (3, false, ItemKind::NetherStar, "§eFavorites"),
        (5, true, ItemKind::Clock, "§bRecently Played"),
    ] {
        let mut item = icon(kind, name, []);
        if is_recent == recent {
            item.insert_component(ItemComponent::EnchantmentGlintOverride(true));
        }
        menu.button(slot, item, LobbyAction::Favorites { recent: is_recent, page: 0 });
    }

    let games = if recent {
        catalog.resolve(&player.recent)
    } else {
        catalog.resolve(&player.favorites)
    };
    let empty = if recent {
        "You haven't played anything yet"
    } else {
        "Right-click a game to favorite it"
    };
    game_grid(&mut menu, catalog, player, &games, page, empty, |page| {
        LobbyAction::Favorites { recent, page }
    });
    back_to_browser(&mut menu);
    finish(menu)
}

fn search_results(catalog: &GameCatalog, player: &PlayerGames, query: &Arc<str>, page: usize) -> Menu<LobbyAction> {
    let mut menu = Menu::new("Search Results", ROWS);
    menu.item(4, icon(ItemKind::NameTag, format!("§dResults for \"{query}\""), []));
    let games = catalog.search(query);
    game_grid(&mut menu, catalog, player, &games, page, "No games matched your search", |page| {
        LobbyAction::SearchResults {
            query: query.clone(),
            page,
        }
    });
    back_to_browser(&mut menu);
    finish(menu)
}

fn game_grid(
    menu: &mut Menu<LobbyAction>,
    catalog: &GameCatalog,
    player: &PlayerGames,
    games: &[&GameSummary],
    page: usize,
    empty_message: &str,
    page_action: impl Fn(usize) -> LobbyAction,
) {
    if games.is_empty() {
        menu.item(EMPTY_SLOT, icon(ItemKind::Barrier, format!("§c{empty_message}"), []));
    }
    let pages = games.len().div_ceil(PER_PAGE).max(1);
    let page = page.min(pages - 1);
    for (slot, game) in GRID.iter().zip(games.iter().skip(page * PER_PAGE)) {
        menu.button(*slot, game_icon(catalog, player, game), LobbyAction::Game(game.id));
    }
    if page > 0 {
        menu.button(
            PREV,
            icon(ItemKind::Arrow, "§7← Previous page", [Text::text(format!("§8Page {page}/{pages}"))]),
            page_action(page - 1),
        );
    }
    if page + 1 < pages {
        menu.button(
            NEXT,
            icon(ItemKind::Arrow, "§7Next page →", [Text::text(format!("§8Page {}/{pages}", page + 2))]),
            page_action(page + 1),
        );
    }
}

fn game_icon(catalog: &GameCatalog, player: &PlayerGames, game: &GameSummary) -> ItemStack {
    let live = catalog.live(game.id);
    let favorite = player.is_favorite(game.id);
    let mut lore = vec![Text::text(format!("§7by §f{}", game.creator))];
    lore.extend(wrap(&game.description, 36).into_iter().take(3).map(|l| Text::text(format!("§8{l}"))));
    lore.push(Text::default());
    lore.push(Text::text(match game.kind {
        GameKind::Match => format!(
            "§7Match §8· §a{} waiting §8· §e{} in progress",
            live.waiting, live.in_progress
        ),
        GameKind::OpenWorld { .. } => format!("§7Open world §8· §a{} online", live.players),
    }));
    lore.push(Text::text(rating_line(game)));
    lore.push(Text::default());
    lore.push(Text::text("§eClick to play"));
    lore.push(Text::text(if favorite {
        "§7Right-click to unfavorite"
    } else {
        "§7Right-click to favorite"
    }));
    lore.push(Text::text("§7Shift-click for details and rating"));

    let kind = ItemKind::from_str(&game.icon).unwrap_or(ItemKind::Paper);
    let name = if favorite {
        format!("§a{} §e★", game.name)
    } else {
        format!("§a{}", game.name)
    };
    let mut item = icon(kind, name, lore);
    if favorite {
        item.insert_component(ItemComponent::EnchantmentGlintOverride(true));
    }
    item
}

fn rating_line(game: &GameSummary) -> String {
    match game.rating {
        Some(r) => format!(
            "§6★ {r:.1} §8({} rating{})",
            game.ratings,
            if game.ratings == 1 { "" } else { "s" }
        ),
        None => "§8Not rated yet".to_owned(),
    }
}

const INFO_STARS: [u16; 5] = [29, 30, 31, 32, 33];

fn game_info(catalog: &GameCatalog, player: &PlayerGames, game: &GameSummary) -> Menu<LobbyAction> {
    let mut menu = Menu::new(game.name.clone(), 5);
    let kind = ItemKind::from_str(&game.icon).unwrap_or(ItemKind::Paper);
    let live = catalog.live(game.id);
    let mut lore = vec![Text::text(format!("§7by §f{}", game.creator))];
    lore.extend(wrap(&game.description, 36).into_iter().take(8).map(|l| Text::text(format!("§8{l}"))));
    lore.push(Text::default());
    lore.push(Text::text(format!("§7{} §8· §fv{}", kind_label(game.kind), game.version)));
    lore.push(Text::text(match game.kind {
        GameKind::Match => format!("§a{} waiting §8· §e{} in progress", live.waiting, live.in_progress),
        GameKind::OpenWorld { .. } => format!("§a{} online", live.players),
    }));
    lore.push(Text::text(format!(
        "§7{} player{} so far",
        game.plays,
        if game.plays == 1 { "" } else { "s" }
    )));
    lore.push(Text::text(rating_line(game)));
    menu.item(4, icon(kind, format!("§a{}", game.name), lore));

    menu.button(20, icon(ItemKind::Emerald, "§aPlay", []), LobbyAction::Game(game.id));
    let favorite = player.is_favorite(game.id);
    let mut star = icon(
        ItemKind::NetherStar,
        if favorite { "§e★ Favorite" } else { "§7Add to favorites" },
        [Text::text(if favorite { "§7Click to remove" } else { "§7Click to add" })],
    );
    star.insert_component(ItemComponent::EnchantmentGlintOverride(favorite));
    menu.button(24, star, LobbyAction::ToggleFavorite(game.id));

    let mine = player.ratings.get(&game.id).copied();
    let played = player.played.contains(&game.id);
    for (n, slot) in (1..=5u8).zip(INFO_STARS) {
        let lit = mine.is_some_and(|m| n <= m);
        let item = if lit { ItemKind::Sunflower } else { ItemKind::GrayDye };
        let name = format!("§6{}§8{}", "★".repeat(n.into()), "★".repeat(5 - usize::from(n)));
        if played {
            let hint = match mine {
                Some(m) if m == n => "§7Your rating".to_owned(),
                _ => format!("§eClick to rate {n}/5"),
            };
            menu.button(slot, icon(item, name, [Text::text(hint)]), LobbyAction::Rate { game: game.id, stars: n });
        } else {
            menu.item(slot, icon(item, name, [Text::text("§7Play it to rate it")]));
        }
    }
    menu.button(
        40,
        icon(ItemKind::Compass, "§aBack to Game Browser", []),
        LobbyAction::Browser {
            tab: BrowserTab::Popular,
            page: 0,
        },
    );
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |_| true);
    menu
}

fn back_to_browser(menu: &mut Menu<LobbyAction>) {
    menu.button(
        SECONDARY,
        icon(ItemKind::Compass, "§aBack to Game Browser", []),
        LobbyAction::Browser {
            tab: BrowserTab::Popular,
            page: 0,
        },
    );
}

/// Adds the shared navigation buttons and border filler.
fn finish(mut menu: Menu<LobbyAction>) -> Menu<LobbyAction> {
    menu.button(SEARCH, icon(ItemKind::NameTag, "§dSearch", [Text::text("§7Type a name in chat")]), LobbyAction::StartSearch);
    menu.button(CLOSE, icon(ItemKind::Barrier, "§cClose", []), LobbyAction::Close);
    menu.fill_empty_where(&icon(ItemKind::GrayStainedGlassPane, " ", []), |slot| !GRID.contains(&slot));
    menu
}

/// Greedy word wrap.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(wrap("a bb ccc dddd", 6), ["a bb", "ccc", "dddd"]);
        assert!(wrap("   ", 10).is_empty());
    }

    #[test]
    fn create_icons_are_items() {
        assert!(GAME_ICONS.len() <= GRID.len());
        for name in GAME_ICONS {
            assert!(ItemKind::from_str(name).is_some(), "{name}");
        }
    }

    #[test]
    fn grid_is_inside_border() {
        for slot in GRID {
            let (row, col) = (slot / 9, slot % 9);
            assert!((1..=4).contains(&row) && (1..=7).contains(&col), "slot {slot}");
        }
    }
}
