//! Lobby chat broadcast and chat-based prompts (search queries, game names).

use chunkedge::message::ChatReceivedMessage;
use chunkedge::prelude::*;
use chunkedge::UniqueId;
use mm_proto::{GameId, GameKind};

use crate::account::CreateGameRequest;
use crate::catalog::{GameCatalog, PlayerGames};
use crate::moderation::FileReport;
use crate::ui::open_search_results;
use crate::world::LobbyLayer;

const PROMPT_TICKS: i64 = 30 * 20;
const NAME_LEN: std::ops::RangeInclusive<usize> = 3..=32;

#[derive(Clone, Debug)]
pub enum PromptPurpose {
    Search,
    GameName { kind: GameKind, icon: &'static str },
    /// Why the player is reporting this game.
    Report(GameId),
}

/// While present, the player's next chat message answers this prompt instead of chatting.
#[derive(Component)]
pub struct ChatPrompt {
    purpose: PromptPurpose,
    expires_at: i64,
}

impl ChatPrompt {
    pub fn new(server: &Server, purpose: PromptPurpose) -> Self {
        Self {
            purpose,
            expires_at: server.current_tick() + PROMPT_TICKS,
        }
    }
}

pub struct ChatPlugin;

impl Plugin for ChatPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, (handle_chat, expire_prompts));
    }
}

fn handle_chat(
    mut commands: Commands,
    mut messages: MessageReader<ChatReceivedMessage>,
    mut players: Query<(&mut Client, &Username, &UniqueId, &PlayerGames, Option<&ChatPrompt>)>,
    mut layer: Single<&mut ChunkLayer, With<LobbyLayer>>,
    mut creates: MessageWriter<CreateGameRequest>,
    mut reports: MessageWriter<FileReport>,
    catalog: Res<GameCatalog>,
) {
    for msg in messages.read() {
        let Ok((mut client, username, uuid, games, prompt)) = players.get_mut(msg.client) else {
            continue;
        };
        // Players can't inject formatting codes.
        let text = msg.message.replace('§', "");
        let text = text.trim();
        if text.is_empty() {
            continue;
        }

        if let Some(prompt) = prompt {
            commands.entity(msg.client).remove::<ChatPrompt>();
            if text.eq_ignore_ascii_case("cancel") {
                client.send_chat_message("§7Cancelled.");
                continue;
            }
            match &prompt.purpose {
                PromptPurpose::Search => {
                    open_search_results(&mut commands, msg.client, text, &catalog, games);
                }
                PromptPurpose::Report(game) => {
                    reports.write(FileReport {
                        client: Some(msg.client),
                        reporter: uuid.0,
                        game: *game,
                        version: None,
                        reason: text.to_owned(),
                    });
                }
                PromptPurpose::GameName { kind, icon } => {
                    if !NAME_LEN.contains(&text.chars().count()) {
                        client.send_chat_message(format!(
                            "§cNames must be {}-{} characters. Start again from My Games.",
                            NAME_LEN.start(),
                            NAME_LEN.end()
                        ));
                        continue;
                    }
                    creates.write(CreateGameRequest {
                        client: msg.client,
                        name: text.to_owned(),
                        icon: (*icon).to_owned(),
                        kind: *kind,
                    });
                }
            }
            continue;
        }

        layer.send_chat_message(format!("§7{}§8: §f{text}", username.0));
    }
}

fn expire_prompts(
    mut commands: Commands,
    mut prompts: Query<(Entity, &mut Client, &ChatPrompt)>,
    server: Res<Server>,
) {
    for (player, mut client, prompt) in &mut prompts {
        if server.current_tick() >= prompt.expires_at {
            commands.entity(player).remove::<ChatPrompt>();
            client.send_chat_message("§7Prompt timed out.");
        }
    }
}
