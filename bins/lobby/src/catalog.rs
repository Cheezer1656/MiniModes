//! The lobby's view of published games and live player counts. It's filled
//! from the database and shard heartbeats; menus only read it.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet, VecDeque};

use chunkedge::prelude::*;
use mm_proto::{GameId, GameLiveStats, GameSummary};

const RECENT_LIMIT: usize = mm_db::library::RECENT_LIMIT as usize;
/// Top Rated ranks by the mean of a game's ratings plus this many imagined
/// ratings of `PRIOR_MEAN`, so one 5-star rating doesn't top the list.
const PRIOR_RATINGS: f64 = 3.0;
const PRIOR_MEAN: f64 = 3.0;

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

    /// Adds a newly published game or replaces its listing. A game on
    /// moderation hold is taken out instead.
    pub fn upsert(&mut self, game: GameSummary) {
        if game.hold.is_some() || game.version == 0 {
            self.remove(game.id);
            return;
        }
        match self.games.iter_mut().find(|g| g.id == game.id) {
            Some(existing) => *existing = game,
            None => self.games.push(game),
        }
    }

    pub fn len(&self) -> usize {
        self.games.len()
    }

    pub fn remove(&mut self, id: GameId) {
        self.games.retain(|g| g.id != id);
    }

    /// A player rated the game; `mean` and `count` are its new totals.
    pub fn set_rating(&mut self, id: GameId, mean: Option<f32>, count: u32) {
        if let Some(game) = self.games.iter_mut().find(|g| g.id == id) {
            game.rating = mean;
            game.ratings = count;
        }
    }

    /// A player joined the game for the first time.
    pub fn add_player(&mut self, id: GameId) {
        if let Some(game) = self.games.iter_mut().find(|g| g.id == id) {
            game.plays += 1;
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
                games.sort_by_key(|g| Reverse(self.live(g.id).players));
            }
            BrowserTab::Popular => {
                games.sort_by_key(|g| Reverse((self.live(g.id).players, g.plays, g.published_at)));
            }
            BrowserTab::Newest => games.sort_by_key(|g| Reverse(g.published_at)),
            BrowserTab::TopRated => {
                games.retain(|g| g.rating.is_some());
                games.sort_by(|a, b| rank_score(b).total_cmp(&rank_score(a)).then(b.ratings.cmp(&a.ratings)));
            }
        }
        games
    }

    /// Games matching every word of `query` in their name, creator or
    /// description, best matches first.
    pub fn search(&self, query: &str) -> Vec<&GameSummary> {
        let query = query.to_lowercase();
        let terms: Vec<&str> = query.split_whitespace().collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<_> = self
            .games
            .iter()
            .filter_map(|g| search_score(g, &query, &terms).map(|score| (score, g)))
            .collect();
        scored.sort_by_key(|(score, g)| Reverse((*score, self.live(g.id).players, g.plays)));
        scored.into_iter().map(|(_, g)| g).collect()
    }

    pub fn resolve<'a>(&'a self, ids: impl IntoIterator<Item = &'a GameId>) -> Vec<&'a GameSummary> {
        ids.into_iter().filter_map(|id| self.get(*id)).collect()
    }
}

/// How Top Rated orders games.
fn rank_score(game: &GameSummary) -> f64 {
    let count = f64::from(game.ratings);
    let mean = game.rating.map_or(PRIOR_MEAN, f64::from);
    (mean * count + PRIOR_MEAN * PRIOR_RATINGS) / (count + PRIOR_RATINGS)
}

/// `None` unless every term matches somewhere. Name matches count most,
/// then the creator, then the description.
fn search_score(game: &GameSummary, query: &str, terms: &[&str]) -> Option<u32> {
    let name = game.name.to_lowercase();
    let creator = game.creator.to_lowercase();
    let description = game.description.to_lowercase();
    let mut score = 0;
    if name == query {
        score += 20;
    } else if name.starts_with(query) {
        score += 10;
    }
    for term in terms {
        let in_name = if name.split_whitespace().any(|w| w.starts_with(term)) {
            4
        } else if name.contains(term) {
            2
        } else {
            0
        };
        let by_creator = if creator == *term {
            4
        } else if creator.starts_with(term) {
            2
        } else {
            0
        };
        let in_description = u32::from(description.contains(term));
        let best = in_name.max(by_creator).max(in_description);
        if best == 0 {
            return None;
        }
        score += best;
    }
    Some(score)
}

/// Per-player favorites, recently played games, ratings, and account data
/// loaded from the database.
#[derive(Component, Default)]
pub struct PlayerGames {
    pub favorites: Vec<GameId>,
    pub recent: VecDeque<GameId>,
    /// Every published game this player joined; only these may be rated.
    pub played: HashSet<GameId>,
    /// Stars this player gave each game they rated.
    pub ratings: HashMap<GameId, u8>,
    /// Games this player created, newest first.
    pub owned: Vec<GameSummary>,
    /// Other players' games this player may edit.
    pub shared: Vec<GameSummary>,
    pub rank: Option<String>,
    /// While set, the player can't create, build, edit scripts or publish.
    pub creator_ban: Option<mm_db::moderation::Ban>,
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

    /// Fills in what was saved from earlier visits.
    pub fn set_library(&mut self, library: mm_db::library::Library) {
        self.favorites = library.favorites.into_iter().map(GameId).collect();
        self.recent = library.played.iter().take(RECENT_LIMIT).map(|&g| GameId(g)).collect();
        self.played = library.played.into_iter().map(GameId).collect();
        self.ratings = library
            .ratings
            .into_iter()
            .map(|(g, stars)| (GameId(g), stars.clamp(1, 5) as u8))
            .collect();
    }

    pub fn owned(&self, id: GameId) -> Option<&GameSummary> {
        self.owned.iter().find(|g| g.id == id)
    }

    /// A game whose draft this player may edit.
    pub fn editable(&self, id: GameId) -> Option<&GameSummary> {
        self.owned(id).or_else(|| self.shared.iter().find(|g| g.id == id))
    }

    pub fn played(&mut self, id: GameId) {
        self.played.insert(id);
        self.recent.retain(|r| *r != id);
        self.recent.push_front(id);
        self.recent.truncate(RECENT_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use mm_proto::GameKind;

    use super::*;

    fn game(id: i64, name: &str, creator: &str, description: &str, rating: Option<(f32, u32)>) -> GameSummary {
        GameSummary {
            id: GameId(id),
            name: name.into(),
            creator: creator.into(),
            description: description.into(),
            icon: "stone".into(),
            kind: GameKind::Match,
            max_players: 8,
            featured: false,
            published_at: id,
            version: 1,
            rating: rating.map(|r| r.0),
            ratings: rating.map_or(0, |r| r.1),
            plays: 0,
            hold: None,
        }
    }

    fn ids(games: &[&GameSummary]) -> Vec<i64> {
        games.iter().map(|g| g.id.0).collect()
    }

    #[test]
    fn search_needs_every_word_and_ranks_names_first() {
        let catalog = GameCatalog::new(vec![
            game(1, "Sky Wars", "ann", "fight on islands", None),
            game(2, "Island Prison", "bob", "mine your way out", None),
            game(3, "Prison Break", "skyler", "escape the prison", None),
            game(4, "Skywalk", "cat", "parkour", None),
        ]);
        assert_eq!(ids(&catalog.search("prison")), [3, 2]);
        assert_eq!(ids(&catalog.search("sky")), [1, 4, 3]);
        assert_eq!(ids(&catalog.search("island prison")), [2]);
        assert_eq!(ids(&catalog.search("bob")), [2]);
        assert_eq!(ids(&catalog.search("mine out")), [2]);
        assert!(catalog.search("  ").is_empty());
        assert!(catalog.search("zzz").is_empty());
    }

    #[test]
    fn held_games_leave_the_catalog() {
        let mut catalog = GameCatalog::new(vec![game(1, "Bridge", "a", "", None), game(2, "Parkour", "b", "", None)]);
        let mut held = game(1, "Bridge", "a", "", None);
        held.hold = Some(mm_proto::Hold::Review);
        catalog.upsert(held);
        assert!(catalog.get(GameId(1)).is_none());
        catalog.upsert(game(1, "Bridge v2", "a", "", None));
        assert_eq!(catalog.get(GameId(1)).unwrap().name, "Bridge v2");
        catalog.remove(GameId(2));
        assert_eq!(catalog.len(), 1);
    }

    #[test]
    fn top_rated_weighs_the_number_of_ratings() {
        let catalog = GameCatalog::new(vec![
            game(1, "One perfect", "a", "", Some((5.0, 1))),
            game(2, "Many good", "b", "", Some((4.6, 40))),
            game(3, "Unrated", "c", "", None),
            game(4, "Many bad", "d", "", Some((1.5, 30))),
        ]);
        assert_eq!(ids(&catalog.tab(BrowserTab::TopRated)), [2, 1, 4]);
    }
}
