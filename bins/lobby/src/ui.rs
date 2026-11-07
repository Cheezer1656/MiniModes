//! Wires hotbar items, NPCs and menu clicks to lobby menus.

use std::sync::Arc;

use chunkedge::prelude::*;
use mm_ui::{
    Hotbar, HotbarSlot, MenuCommandsExt, NpcAction, NpcSpec, NpcTeam, UiAction, UiInputSet,
    UiPlugin, UiSource, icon, spawn_npc,
};

use crate::catalog::{BrowserTab, GameCatalog, PlayerGames};
use crate::chat::{ChatPrompt, PromptPurpose};
use crate::config::{HotbarConfig, MenuName, NpcConfig};
use crate::join::JoinRequest;
use crate::menus::{self, LobbyAction};
use crate::publish::{PlayerSavesSetting, PublishRequest};
use crate::world::LobbyLayer;

#[derive(Resource)]
pub struct LobbyUiConfig {
    pub npcs: Vec<NpcConfig>,
    pub hotbar: Vec<HotbarConfig>,
}

/// The view a player's open menu was rendered from, so it can be re-rendered
/// after a change (e.g. toggling a favorite).
#[derive(Component)]
struct CurrentView(LobbyAction);

pub struct LobbyUiPlugin;

impl Plugin for LobbyUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(UiPlugin::<LobbyAction>::default())
            .add_systems(PostStartup, spawn_npcs)
            .add_systems(Update, (give_hotbar, handle_actions.after(UiInputSet)));
    }
}

fn opening(name: MenuName) -> LobbyAction {
    match name {
        MenuName::Browser => LobbyAction::Browser {
            tab: BrowserTab::Popular,
            page: 0,
        },
        MenuName::MyGames => LobbyAction::MyGames { page: 0 },
        MenuName::Favorites => LobbyAction::Favorites {
            recent: false,
            page: 0,
        },
        MenuName::Search => LobbyAction::StartSearch,
    }
}

fn spawn_npcs(
    mut commands: Commands,
    mut team: ResMut<NpcTeam>,
    config: Res<LobbyUiConfig>,
    layer: Single<Entity, With<LobbyLayer>>,
) {
    for npc in &config.npcs {
        let spec = NpcSpec {
            label: npc.label.iter().map(|l| Text::text(l.clone())).collect(),
            pos: DVec3::from(npc.pos),
            yaw: npc.yaw,
            pitch: npc.pitch,
            skin: npc.skin.as_ref().map(|s| (s.value.clone(), s.signature.clone())),
        };
        let entity = spawn_npc(&mut commands, &mut team, *layer, spec);
        commands.entity(entity).insert(NpcAction(opening(npc.opens)));
    }
}

fn give_hotbar(mut commands: Commands, clients: Query<Entity, Added<Client>>, config: Res<LobbyUiConfig>) {
    for client in &clients {
        let slots = config
            .hotbar
            .iter()
            .map(|h| HotbarSlot {
                index: h.slot,
                item: icon(
                    ItemKind::from_str(&h.item).unwrap_or(ItemKind::Paper),
                    h.name.clone(),
                    h.lore.iter().map(|l| Text::text(l.clone())),
                ),
                action: opening(h.opens),
            })
            .collect();
        commands
            .entity(client)
            .insert((Hotbar { slots }, PlayerGames::default()));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_actions(
    mut commands: Commands,
    mut actions: MessageReader<UiAction<LobbyAction>>,
    mut players: Query<(&mut Client, &mut PlayerGames, Option<&CurrentView>)>,
    catalog: Res<GameCatalog>,
    server: Res<Server>,
    mut joins: MessageWriter<JoinRequest>,
    mut publishes: MessageWriter<PublishRequest>,
    mut settings: MessageWriter<PlayerSavesSetting>,
) {
    for UiAction { client: player, action, source } in actions.read() {
        let Ok((mut client, mut games, view)) = players.get_mut(*player) else {
            continue;
        };

        let next_view = match action {
            LobbyAction::Close => {
                commands.close_menu(*player);
                commands.entity(*player).remove::<CurrentView>();
                continue;
            }
            LobbyAction::StartSearch => {
                commands.close_menu(*player);
                commands
                    .entity(*player)
                    .insert(ChatPrompt::new(&server, PromptPurpose::Search));
                client.send_chat_message("§dType a game or creator name in chat §7(or \"cancel\")");
                continue;
            }
            LobbyAction::CreateIcon { kind, icon } => {
                commands.close_menu(*player);
                commands.entity(*player).remove::<CurrentView>().insert(ChatPrompt::new(
                    &server,
                    PromptPurpose::GameName {
                        kind: *kind,
                        icon,
                    },
                ));
                client.send_chat_message("§aType a name for your game in chat §7(or \"cancel\")");
                continue;
            }
            LobbyAction::EditDraft(id) if matches!(source, UiSource::Menu { button: 1, .. }) => {
                LobbyAction::ManageGame(*id)
            }
            LobbyAction::Publish { game, mode } => {
                commands.close_menu(*player);
                commands.entity(*player).remove::<CurrentView>();
                publishes.write(PublishRequest {
                    client: *player,
                    game: *game,
                    mode: *mode,
                });
                continue;
            }
            LobbyAction::TogglePlayerSaves(id) => {
                let on = !games.player_saves.remove(id);
                if on {
                    games.player_saves.insert(*id);
                }
                settings.write(PlayerSavesSetting {
                    game: *id,
                    on,
                });
                client.send_action_bar_message(if on {
                    "§aPlayer progress will be saved (applies to newly started worlds)"
                } else {
                    "§7Player progress won't be saved"
                });
                LobbyAction::ManageGame(*id)
            }
            LobbyAction::EditDraft(id) => {
                commands.close_menu(*player);
                commands.entity(*player).remove::<CurrentView>();
                joins.write(JoinRequest {
                    client: *player,
                    game: *id,
                    draft: true,
                });
                continue;
            }
            LobbyAction::Game(id) => {
                let right_click = matches!(source, UiSource::Menu { button: 1, .. });
                if right_click {
                    let now_favorite = games.toggle_favorite(*id);
                    let name = catalog.get(*id).map_or("that game", |g| g.name.as_str());
                    client.send_action_bar_message(if now_favorite {
                        format!("§e★ Added {name} to favorites")
                    } else {
                        format!("§7Removed {name} from favorites")
                    });
                    // Re-render whatever menu they're looking at.
                    match view {
                        Some(CurrentView(v)) => v.clone(),
                        None => continue,
                    }
                } else {
                    commands.close_menu(*player);
                    commands.entity(*player).remove::<CurrentView>();
                    joins.write(JoinRequest {
                        client: *player,
                        game: *id,
                        draft: false,
                    });
                    continue;
                }
            }
            view => view.clone(),
        };

        if let Some(menu) = menus::render(&next_view, &catalog, &games) {
            commands.open_menu(*player, menu);
            commands.entity(*player).insert(CurrentView(next_view));
        }
    }
}

/// Opens the search results menu for `query`.
pub fn open_search_results(
    commands: &mut Commands,
    player: Entity,
    query: &str,
    catalog: &GameCatalog,
    games: &PlayerGames,
) {
    let view = LobbyAction::SearchResults {
        query: Arc::from(query),
        page: 0,
    };
    if let Some(menu) = menus::render(&view, catalog, games) {
        commands.open_menu(player, menu);
        commands.entity(player).insert(CurrentView(view));
    }
}
