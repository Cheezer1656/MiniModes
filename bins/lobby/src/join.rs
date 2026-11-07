//! Requests to play a game or edit a draft, handed to the coordinator.

use chunkedge::UniqueId;
use chunkedge::prelude::*;
use mm_proto::GameId;
use mm_proto::ipc::GameVersion;

use crate::catalog::{GameCatalog, PlayerGames};
use crate::coordinator::Coordinator;

#[derive(Message, Clone, Copy, Debug)]
pub struct JoinRequest {
    pub client: Entity,
    pub game: GameId,
    /// Build in the draft instead of playing the published game. Only the
    /// owner may.
    pub draft: bool,
}

pub struct JoinPlugin;

impl Plugin for JoinPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<JoinRequest>().add_systems(Update, handle_joins);
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
        let (game, version) = if request.draft {
            (games.owned(request.game), GameVersion::Draft)
        } else {
            (catalog.get(request.game), GameVersion::Published(1))
        };
        let Some(game) = game else {
            client.send_chat_message("§cThat game is no longer available.");
            continue;
        };
        match coordinator.begin_join(request.client, uuid.0, game, version) {
            Ok(()) if request.draft => {
                client.send_chat_message(format!("§7Opening the draft of §a{}§7...", game.name))
            }
            Ok(()) => client.send_chat_message(format!("§7Sending you to §a{}§7...", game.name)),
            Err(msg) => client.send_chat_message(msg),
        }
    }
}
