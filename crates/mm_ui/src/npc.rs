use std::collections::HashMap;

use chunkedge::entity::display::BillboardRenderConstraints;
use chunkedge::entity::player::PlayerEntity;
use chunkedge::entity::text_display::{self, TextDisplayEntity};
use chunkedge::entity::HeadYaw;
use chunkedge::player_list::{Listed, PlayerListEntryBundle};
use chunkedge::prelude::*;
use chunkedge::protocol::WritePacket;
use chunkedge::protocol::packets::play::set_player_team_s2c::{
    CollisionRule, Mode, NameTagVisibility, SetPlayerTeamS2c, TeamFlags,
};
use chunkedge::protocol::IntoTextComponent;

use crate::{UiAction, UiSource};

/// Scoreboard team holding every NPC so their placeholder usernames stay hidden.
const NPC_TEAM: &str = "mm_npcs";
/// Height of the label above an NPC's feet.
const LABEL_HEIGHT: f64 = 2.05;
/// Clicks on NPCs closer together than this (per player) are ignored; the
/// client often sends several interact packets for one click.
const CLICK_DEBOUNCE_TICKS: i64 = 4;

pub struct NpcSpec {
    /// Label lines rendered above the NPC, top first.
    pub label: Vec<Text>,
    pub pos: DVec3,
    pub yaw: f32,
    pub pitch: f32,
    /// Signed Mojang skin texture (`value`, `signature`).
    pub skin: Option<(String, String)>,
}

/// The action an NPC fires when clicked.
#[derive(Component, Clone)]
pub struct NpcAction<A: Send + Sync + 'static>(pub A);

/// Placeholder usernames of all NPCs, sent to clients as a hidden-nametag team.
#[derive(Resource, Default)]
pub struct NpcTeam {
    names: Vec<String>,
    /// Added since the last broadcast to already-connected clients.
    pending: Vec<String>,
}

pub(crate) fn build(app: &mut App) {
    if app.world().contains_resource::<NpcTeam>() {
        return;
    }
    app.init_resource::<NpcTeam>()
        .add_systems(Update, sync_npc_team);
}

/// Spawns a player-model NPC with a floating label and returns its entity, so
/// the caller can insert an [`NpcAction`].
pub fn spawn_npc(commands: &mut Commands, team: &mut NpcTeam, layer: Entity, spec: NpcSpec) -> Entity {
    let username = format!("npc{:04}", team.names.len());
    let uuid = UniqueId::default();

    let mut properties = Properties::default();
    if let Some((value, signature)) = spec.skin {
        properties.set_skin(value, signature);
    }
    commands.spawn(PlayerListEntryBundle {
        uuid,
        username: Username(username.clone()),
        properties,
        listed: Listed(false),
        ..Default::default()
    });

    let npc = commands
        .spawn((
            PlayerEntity,
            EntityLayerId(layer),
            uuid,
            Position::new(spec.pos),
            Look::new(spec.yaw, spec.pitch),
            HeadYaw(spec.yaw),
        ))
        .id();

    if !spec.label.is_empty() {
        let mut label = Text::default();
        for (i, line) in spec.label.into_iter().enumerate() {
            if i > 0 {
                label += Text::text("\n");
            }
            label += line;
        }
        commands.spawn((
            TextDisplayEntity,
            EntityLayerId(layer),
            Position::new(spec.pos + DVec3::new(0.0, LABEL_HEIGHT, 0.0)),
            text_display::Text(label),
            // Always face the viewer.
            BillboardRenderConstraints(3),
        ));
    }

    team.names.push(username.clone());
    team.pending.push(username);
    npc
}

fn sync_npc_team(mut team: ResMut<NpcTeam>, mut clients: Query<&mut Client>) {
    let pending = std::mem::take(&mut team.pending);
    for mut client in &mut clients {
        if !client.is_added() {
            if !pending.is_empty() {
                client.write_packet(&SetPlayerTeamS2c {
                    team_name: NPC_TEAM,
                    mode: Mode::AddEntities {
                        entities: pending.iter().map(String::as_str).collect(),
                    },
                });
            }
            continue;
        }
        client.write_packet(&SetPlayerTeamS2c {
            team_name: NPC_TEAM,
            mode: Mode::CreateTeam {
                team_display_name: Text::default().into_cow_text_component(),
                team_prefix: Text::default().into_cow_text_component(),
                team_suffix: Text::default().into_cow_text_component(),
                name_tag_visibility: NameTagVisibility::Never,
                collision_rule: CollisionRule::Never,
                team_color: None,
                friendly_flags: TeamFlags::new(),
                entities: team.names.iter().map(String::as_str).collect(),
            },
        });
    }
}

pub(crate) fn handle_npc_clicks<A: Clone + Send + Sync + 'static>(
    mut interactions: MessageReader<InteractEntityMessage>,
    npcs: Query<&NpcAction<A>>,
    server: Res<Server>,
    mut last_click: Local<HashMap<Entity, i64>>,
    mut actions: MessageWriter<UiAction<A>>,
) {
    let now = server.current_tick();
    for interaction in interactions.read() {
        if interaction.hand != Hand::Main {
            continue;
        }
        let Ok(action) = npcs.get(interaction.entity) else {
            continue;
        };
        let last = last_click.entry(interaction.client).or_insert(i64::MIN / 2);
        if now - *last < CLICK_DEBOUNCE_TICKS {
            continue;
        }
        *last = now;
        actions.write(UiAction {
            client: interaction.client,
            action: action.0.clone(),
            source: UiSource::Npc(interaction.entity),
        });
    }
    // Forget players who left so the map stays bounded.
    if now % 1200 == 0 {
        last_click.retain(|_, tick| now - *tick < CLICK_DEBOUNCE_TICKS);
    }
}
