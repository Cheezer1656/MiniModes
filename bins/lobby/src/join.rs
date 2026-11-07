//! Requests to play a game or edit a draft, handed to the coordinator.

use chunkedge::UniqueId;
use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::prelude::*;
use mm_proto::GameId;
use mm_proto::ipc::GameVersion;

use crate::catalog::{GameCatalog, PlayerGames};
use crate::coordinator::{Coordinator, Joining};

#[derive(Message, Clone, Copy, Debug)]
pub struct JoinRequest {
    pub client: Entity,
    pub game: GameId,
    /// Build in the draft instead of playing the published game. Only the
    /// owner and collaborators may.
    pub draft: bool,
}

pub struct JoinPlugin;

impl Plugin for JoinPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<JoinRequest>()
            .add_systems(Update, (handle_joins, leave_queue));
    }
}

fn handle_joins(
    mut requests: MessageReader<JoinRequest>,
    mut clients: Query<(&mut Client, &UniqueId, &PlayerGames)>,
    mut coordinator: ResMut<Coordinator>,
    catalog: Res<GameCatalog>,
) {
    for request in requests.read() {
        let Ok((mut client, uuid, games)) = clients.get_mut(request.client) else {
            continue;
        };
        let game = if request.draft {
            games.editable(request.game)
        } else {
            catalog.get(request.game).filter(|g| g.version > 0)
        };
        let Some(game) = game else {
            client.send_chat_message("§cThat game is no longer available.");
            continue;
        };
        let version = if request.draft {
            GameVersion::Draft
        } else {
            GameVersion::Published(game.version)
        };
        match coordinator.begin_join(request.client, uuid.0, game, version) {
            Ok(Joining::Sent) if request.draft => {
                client.send_chat_message(format!("§7Opening the draft of §a{}§7...", game.name))
            }
            Ok(Joining::Sent) => client.send_chat_message(format!("§7Sending you to §a{}§7...", game.name)),
            Ok(Joining::Queued(pos)) => client.send_chat_message(format!(
                "§e{} is full. You're §f#{pos}§e in the queue §7(/leavequeue to leave)",
                game.name
            )),
            Err(msg) => client.send_chat_message(msg),
        }
    }
}

fn leave_queue(
    mut commands: MessageReader<CommandExecutionMessage>,
    mut clients: Query<&mut Client>,
    mut coordinator: ResMut<Coordinator>,
) {
    for cmd in commands.read() {
        if cmd.command.split_whitespace().next() != Some("leavequeue") {
            continue;
        }
        let left = coordinator.leave_queues(cmd.executor);
        if let Ok(mut client) = clients.get_mut(cmd.executor) {
            client.send_chat_message(if left { "§7You left the queue." } else { "§7You aren't in a queue." });
        }
    }
}
