//! Per-game console: script output and platform notices from shards, kept
//! briefly for late joiners and broadcast to open editors.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use mm_proto::GameId;
use mm_proto::ipc::DiagnosticLevel;
use serde::Serialize;
use tokio::sync::broadcast;

/// Lines replayed to an editor that just connected.
const RECENT: usize = 200;
const CHANNEL: usize = 256;
const MAX_LINE: usize = 4000;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Line {
    /// Unix milliseconds.
    pub at: u64,
    pub level: &'static str,
    pub text: String,
}

impl Line {
    pub fn new(level: DiagnosticLevel, text: &str) -> Self {
        let mut text = text.to_owned();
        if text.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        Self {
            at: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64,
            level: match level {
                DiagnosticLevel::Info => "info",
                DiagnosticLevel::Warn => "warn",
                DiagnosticLevel::Error => "error",
                DiagnosticLevel::System => "system",
            },
            text,
        }
    }
}

struct GameConsole {
    recent: VecDeque<Line>,
    tx: broadcast::Sender<Line>,
}

#[derive(Clone, Default)]
pub struct ConsoleHub(Arc<Mutex<HashMap<GameId, GameConsole>>>);

impl ConsoleHub {
    pub fn push(&self, game: GameId, line: Line) {
        let mut games = self.0.lock().unwrap();
        let console = games.entry(game).or_insert_with(|| GameConsole {
            recent: VecDeque::new(),
            tx: broadcast::channel(CHANNEL).0,
        });
        if console.recent.len() == RECENT {
            console.recent.pop_front();
        }
        console.recent.push_back(line.clone());
        let _ = console.tx.send(line);
    }

    /// The recent lines and a receiver for what follows, with nothing lost
    /// or repeated in between.
    pub fn subscribe(&self, game: GameId) -> (Vec<Line>, broadcast::Receiver<Line>) {
        let mut games = self.0.lock().unwrap();
        let console = games.entry(game).or_insert_with(|| GameConsole {
            recent: VecDeque::new(),
            tx: broadcast::channel(CHANNEL).0,
        });
        (console.recent.iter().cloned().collect(), console.tx.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_recent_then_streams() {
        let hub = ConsoleHub::default();
        let game = GameId(4);
        for i in 0..RECENT + 5 {
            hub.push(game, Line::new(DiagnosticLevel::Info, &i.to_string()));
        }
        hub.push(GameId(5), Line::new(DiagnosticLevel::Info, "other game"));
        let (recent, mut rx) = hub.subscribe(game);
        assert_eq!(recent.len(), RECENT);
        assert_eq!(recent[0].text, "5");
        hub.push(game, Line::new(DiagnosticLevel::Error, "boom"));
        let next = rx.try_recv().unwrap();
        assert_eq!((next.level, next.text.as_str()), ("error", "boom"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn truncates_long_lines_on_char_boundaries() {
        let line = Line::new(DiagnosticLevel::Warn, &"é".repeat(MAX_LINE));
        assert!(line.text.len() <= MAX_LINE + '…'.len_utf8());
        assert!(line.text.ends_with('…'));
    }
}
