//! The lobby's view of published games and live player counts. It's filled
//! from the database and shard heartbeats; menus only read it.

use std::collections::{HashMap, HashSet, VecDeque};

use chunkedge::prelude::*;
use mm_proto::{GameId, GameLiveStats, GameSummary};

const RECENT_LIMIT: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserTab {
    Featured,
    Popular,
    Newest,
    TopRated,
}

impl BrowserTab {
    pub const ALL: [Self; 4] = [Self::Featured, Self::Popular, Self::Newest, Self::TopRated];
}

#[derive(Resource, Default)]
pub struct GameCatalog {
    games: Vec<GameSummary>,
    live: HashMap<GameId, GameLiveStats>,
}

impl GameCatalog {
    pub fn new(games: Vec<GameSummary>) -> Self {
        Self {
            games,
            live: HashMap::new(),
        }
    }

    /// Adds a newly published game or replaces its listing.
    pub fn upsert(&mut self, game: GameSummary) {
        match self.games.iter_mut().find(|g| g.id == game.id) {
            Some(existing) => *existing = game,
            None => self.games.push(game),
        }
    }

    pub fn set_live(&mut self, live: HashMap<GameId, GameLiveStats>) {
        self.live = live;
    }

    pub fn get(&self, id: GameId) -> Option<&GameSummary> {
        self.games.iter().find(|g| g.id == id)
    }

    pub fn live(&self, id: GameId) -> GameLiveStats {
        self.live.get(&id).copied().unwrap_or_default()
    }

    pub fn tab(&self, tab: BrowserTab) -> Vec<&GameSummary> {
        let mut games: Vec<_> = self.games.iter().collect();
        match tab {
            BrowserTab::Featured => {
                games.retain(|g| g.featured);
                games.sort_by_key(|g| std::cmp::Reverse(self.live(g.id).players));
            }
            BrowserTab::Popular => {
                games.sort_by_key(|g| std::cmp::Reverse((self.live(g.id).players, g.published_at)));
            }
            BrowserTab::Newest => games.sort_by_key(|g| std::cmp::Reverse(g.published_at)),
            BrowserTab::TopRated => {
                games.retain(|g| g.rating.is_some());
                games.sort_by(|a, b| b.rating.partial_cmp(&a.rating).unwrap_or(std::cmp::Ordering::Equal));
            }
        }
        games
    }

    /// Case-insensitive match on game name or creator.
    pub fn search(&self, query: &str) -> Vec<&GameSummary> {
        let query = query.to_lowercase();
        let mut games: Vec<_> = self
            .games
            .iter()
            .filter(|g| g.name.to_lowercase().contains(&query) || g.creator.to_lowercase() == query)
            .collect();
        games.sort_by_key(|g| std::cmp::Reverse(self.live(g.id).players));
        games
    }

    pub fn resolve<'a>(&'a self, ids: impl IntoIterator<Item = &'a GameId>) -> Vec<&'a GameSummary> {
        ids.into_iter().filter_map(|id| self.get(*id)).collect()
    }
}

/// Per-player favorites, recently played games, and account data loaded
/// from the database.
#[derive(Component, Default)]
pub struct PlayerGames {
    pub favorites: Vec<GameId>,
    pub recent: VecDeque<GameId>,
    /// Games this player created, newest first.
    pub owned: Vec<GameSummary>,
    /// Other players' games this player may edit.
    pub shared: Vec<GameSummary>,
    pub rank: Option<String>,
    /// Open worlds among `owned` that save player state.
    pub player_saves: HashSet<GameId>,
    /// Set once `owned` and `rank` have loaded.
    pub loaded: bool,
}

impl PlayerGames {
    pub fn is_favorite(&self, id: GameId) -> bool {
        self.favorites.contains(&id)
    }

    /// Returns whether the game is now a favorite.
    pub fn toggle_favorite(&mut self, id: GameId) -> bool {
        if let Some(i) = self.favorites.iter().position(|f| *f == id) {
            self.favorites.remove(i);
            false
        } else {
            self.favorites.push(id);
            true
        }
    }

    pub fn owned(&self, id: GameId) -> Option<&GameSummary> {
        self.owned.iter().find(|g| g.id == id)
    }

    /// A game whose draft this player may edit.
    pub fn editable(&self, id: GameId) -> Option<&GameSummary> {
        self.owned(id).or_else(|| self.shared.iter().find(|g| g.id == id))
    }

    pub fn played(&mut self, id: GameId) {
        self.recent.retain(|r| *r != id);
        self.recent.push_front(id);
        self.recent.truncate(RECENT_LIMIT);
    }
}
