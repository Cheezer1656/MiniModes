//! Moderation on the shard: script limits from the owner's rank, the log of
//! text scripts show players, and `/report`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chunkedge::UniqueId;
use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::item::ItemComponent;
use chunkedge::prelude::*;
use mm_core::Tokio;
use mm_db::moderation::TextLine;
use mm_proto::ipc::{GameVersion, RankQuota, ShardMsg};
use mm_script::{Command, PlayerId};

use crate::instance::{InInstance, Instance};
use crate::link::LobbyLink;
use crate::world::Storage;

/// Lines logged per instance per minute; scripts that print more are
/// sampled (the rest only counted).
const TEXT_LINES_PER_MINUTE: u32 = 600;
const TEXT_MAX_LEN: usize = 512;
const REPORT_COOLDOWN: Duration = Duration::from_secs(60);
const REPORT_REASON_LEN: std::ops::RangeInclusive<usize> = 3..=200;

/// Script limits by the owner's rank, from the lobby. Until it sends them,
/// every game gets the defaults from this shard's config.
#[derive(Resource, Default)]
pub struct RankQuotas {
    pub default: Option<RankQuota>,
    pub ranks: HashMap<String, RankQuota>,
}

impl RankQuotas {
    pub fn get(&self, rank: Option<&str>) -> Option<RankQuota> {
        rank.and_then(|r| self.ranks.get(r)).copied().or(self.default)
    }
}

/// Text waiting to be written to `script_text_log`.
#[derive(Resource, Default)]
pub struct TextLog {
    lines: Vec<TextLine>,
    pub logged: u64,
    pub dropped: u64,
}

/// Per instance: the last text each player got of each kind (repeats
/// aren't logged again), and this minute's line budget.
#[derive(Component, Default)]
pub struct TextAudit {
    last: HashMap<(Option<PlayerId>, &'static str), String>,
    window: u64,
    lines: u32,
}

/// What a script showed someone, by kind (`chat`, `title`, `subtitle`,
/// `actionbar`, `item`).
pub fn command_texts(command: &Command) -> Vec<(PlayerId, &'static str, String)> {
    match command {
        Command::Message { player, text } => vec![(*player, "chat", text.clone())],
        Command::Title {
            player, title, subtitle, ..
        } => {
            let mut texts = vec![(*player, "title", title.clone())];
            if !subtitle.is_empty() {
                texts.push((*player, "subtitle", subtitle.clone()));
            }
            texts
        }
        Command::ActionBar { player, text } => vec![(*player, "actionbar", text.clone())],
        Command::Give { player, item } | Command::SetSlot { player, item, .. } => {
            let defaults = item.default_components();
            let mut parts = Vec::new();
            for component in item.components() {
                if defaults.contains(component) {
                    continue;
                }
                // `to_legacy_lossy` starts each line with a reset (§r).
                let plain = |t: &Text| t.to_legacy_lossy().replace("§r", "");
                match component {
                    ItemComponent::ItemName(name) => parts.insert(0, plain(&name.text)),
                    ItemComponent::Lore(lines) => parts.extend(lines.iter().map(|l| plain(&l.text))),
                    _ => {}
                }
            }
            if parts.is_empty() {
                Vec::new()
            } else {
                vec![(*player, "item", parts.join(" | "))]
            }
        }
        _ => Vec::new(),
    }
}

impl TextAudit {
    pub fn record(
        &mut self,
        log: &mut TextLog,
        tick: u64,
        instance: &Instance,
        (id, player): (PlayerId, Option<Uuid>),
        kind: &'static str,
        mut text: String,
    ) {
        if text.trim().is_empty() {
            return;
        }
        if text.len() > TEXT_MAX_LEN {
            let mut end = TEXT_MAX_LEN;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        let key = (Some(id), kind);
        if self.last.get(&key) == Some(&text) {
            return;
        }
        if tick >= self.window + 60 * 20 {
            self.window = tick;
            self.lines = 0;
        }
        self.lines += 1;
        if self.lines > TEXT_LINES_PER_MINUTE {
            log.dropped += 1;
            return;
        }
        self.last.insert(key, text.clone());
        log.logged += 1;
        log.lines.push(TextLine {
            game: instance.spec.game.0,
            version: match instance.spec.version {
                GameVersion::Draft => None,
                GameVersion::Published(v) => Some(v as i32),
            },
            instance: instance.spec.id.0 as i64,
            player,
            kind,
            text,
        });
    }

    /// Forgets a player who left, so the map doesn't grow.
    pub fn forget(&mut self, id: PlayerId) {
        self.last.retain(|(p, _), _| *p != Some(id));
    }
}

pub struct ModerationPlugin;

impl Plugin for ModerationPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RankQuotas>()
            .init_resource::<TextLog>()
            .init_resource::<ReportCooldowns>()
            .add_systems(Update, (flush_text_log, report_command));
    }
}

/// Writes the buffered lines every two seconds.
fn flush_text_log(mut log: ResMut<TextLog>, storage: Res<Storage>, tokio: Res<Tokio>, server: Res<Server>) {
    if server.current_tick() % 40 != 0 || log.lines.is_empty() {
        return;
    }
    let lines = std::mem::take(&mut log.lines);
    let db = storage.db.clone();
    tokio.0.spawn(async move {
        if let Err(e) = mm_db::moderation::write_text_log(&db, &lines).await {
            tracing::warn!("writing {} script text log lines: {e}", lines.len());
        }
    });
}

#[derive(Resource, Default)]
struct ReportCooldowns(HashMap<Uuid, Instant>);

/// `/report <reason>`: files a report on the game being played.
fn report_command(
    mut messages: MessageReader<CommandExecutionMessage>,
    mut players: Query<(&mut Client, &InInstance, &UniqueId)>,
    layers: Query<&Instance>,
    mut cooldowns: ResMut<ReportCooldowns>,
    link: Res<LobbyLink>,
) {
    for cmd in messages.read() {
        let Some(reason) = cmd.command.strip_prefix("report").filter(|r| r.is_empty() || r.starts_with(' ')) else {
            continue;
        };
        let Ok((mut client, in_instance, uuid)) = players.get_mut(cmd.executor) else {
            continue;
        };
        let Ok(instance) = layers.get(in_instance.0) else {
            continue;
        };
        let GameVersion::Published(version) = instance.spec.version else {
            client.send_chat_message("§cOnly published games can be reported.");
            continue;
        };
        let reason = reason.trim().replace('§', "");
        if !REPORT_REASON_LEN.contains(&reason.chars().count()) {
            client.send_chat_message("§7Usage: §f/report <reason>§7, e.g. §f/report offensive messages");
            continue;
        }
        let now = Instant::now();
        cooldowns.0.retain(|_, until| *until > now);
        if cooldowns.0.contains_key(&uuid.0) {
            client.send_chat_message("§cYou just sent a report. Wait a minute before sending another.");
            continue;
        }
        cooldowns.0.insert(uuid.0, now + REPORT_COOLDOWN);
        link.send(ShardMsg::Report {
            game: instance.spec.game,
            version: Some(version),
            reporter: uuid.0,
            reason,
        });
        client.send_chat_message("§aThanks, your report was sent to the moderators.");
    }
}

#[cfg(test)]
mod tests {
    use chunkedge::item::ItemStack;
    use mm_proto::ipc::{InstanceId, InstanceSpec};
    use mm_proto::{GameId, GameKind};

    use super::*;

    fn instance() -> Instance {
        Instance::new_for_test(InstanceSpec {
            id: InstanceId(3),
            game: GameId(7),
            version: GameVersion::Published(2),
            kind: GameKind::Match,
            max_players: 8,
        })
    }

    #[test]
    fn items_log_their_custom_text_only() {
        let plain = Command::Give {
            player: 1,
            item: ItemStack::new(ItemKind::Stone, 1),
        };
        assert!(command_texts(&plain).is_empty());
        let named = ItemStack::new(ItemKind::Stick, 1).with_components(vec![
            ItemComponent::ItemName(Text::text("Wand").into()),
            ItemComponent::Lore(vec![Text::text("§7Zaps").into()]),
        ]);
        let texts = command_texts(&Command::SetSlot {
            player: 2,
            slot: 1,
            item: named,
        });
        assert_eq!(texts, vec![(2, "item", "Wand | §7Zaps".to_owned())]);
        let title = Command::Title {
            player: 1,
            title: "Go".into(),
            subtitle: String::new(),
            times: [0; 3],
        };
        assert_eq!(command_texts(&title), vec![(1, "title", "Go".to_owned())]);
    }

    #[test]
    fn repeats_are_skipped_and_floods_capped() {
        let (instance, mut log, mut audit) = (instance(), TextLog::default(), TextAudit::default());
        let who = (1, Some(Uuid::from_u128(1)));
        for _ in 0..3 {
            audit.record(&mut log, 0, &instance, who, "actionbar", "Time: 10".into());
        }
        audit.record(&mut log, 0, &instance, who, "chat", "Time: 10".into());
        assert_eq!(log.lines.len(), 2);
        assert_eq!((log.lines[0].game, log.lines[0].version, log.lines[0].instance), (7, Some(2), 3));

        for n in 0..TEXT_LINES_PER_MINUTE + 10 {
            audit.record(&mut log, 20, &instance, who, "chat", format!("line {n}"));
        }
        assert_eq!(log.logged, u64::from(TEXT_LINES_PER_MINUTE));
        assert_eq!(log.dropped, 12);
        // The next minute logs again.
        audit.record(&mut log, 20 + 60 * 20, &instance, who, "chat", "later".into());
        assert_eq!(log.lines.last().unwrap().text, "later");
    }
}
