use std::path::PathBuf;

use chunkedge::anvil::{AnvilLevel, ChunkLoadMessage, ChunkLoadStatus};
use chunkedge::prelude::*;

use crate::config::Spawn;

#[derive(Resource)]
pub struct LobbyWorld {
    pub path: Option<PathBuf>,
    pub keep_loaded_radius: i32,
}

/// Marker for the lobby's single layer entity.
#[derive(Component)]
pub struct LobbyLayer;

pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, setup_world).add_systems(
            Update,
            (
                despawn_disconnected_clients,
                init_clients,
                handle_chunk_loads,
                return_from_void,
            ),
        );
    }
}

fn setup_world(
    mut commands: Commands,
    server: Res<Server>,
    dimensions: Res<DimensionTypeRegistry>,
    biomes: Res<BiomeRegistry>,
    world: Res<LobbyWorld>,
    spawn: Res<Spawn>,
) {
    let mut layer = LayerBundle::new(ident!("overworld"), &dimensions, &biomes, &server);
    let r = world.keep_loaded_radius;
    let center = ChunkPos::from(BlockPos::new(spawn.x as i32, spawn.y as i32, spawn.z as i32));

    match &world.path {
        Some(path) => {
            let mut level = AnvilLevel::new(path, &biomes);
            for z in -r..=r {
                for x in -r..=r {
                    let pos = ChunkPos::new(center.x + x, center.z + z);
                    level.ignored_chunks.insert(pos);
                    level.force_chunk_load(pos);
                }
            }
            tracing::info!("loading lobby world from {}", path.display());
            commands.spawn((layer, level, LobbyLayer));
        }
        None => {
            tracing::warn!("no lobby world configured; generating a dev platform");
            for z in -r..=r {
                for x in -r..=r {
                    layer
                        .chunk
                        .insert_chunk([center.x + x, center.z + z], UnloadedChunk::new());
                }
            }
            let floor = spawn.y as i32 - 1;
            for z in -16..16 {
                for x in -16..16 {
                    layer.chunk.set_block(
                        [spawn.x as i32 + x, floor, spawn.z as i32 + z],
                        BlockState::SMOOTH_QUARTZ,
                    );
                }
            }
            commands.spawn((layer, LobbyLayer));
        }
    }
}

fn init_clients(
    mut clients: Query<
        (
            &mut EntityLayerId,
            &mut VisibleChunkLayer,
            &mut VisibleEntityLayers,
            &mut Position,
            &mut Look,
            &mut GameMode,
        ),
        Added<Client>,
    >,
    layer: Single<Entity, With<LobbyLayer>>,
    spawn: Res<Spawn>,
) {
    for (mut layer_id, mut visible_chunks, mut visible_entities, mut pos, mut look, mut mode) in
        &mut clients
    {
        layer_id.0 = *layer;
        visible_chunks.0 = *layer;
        visible_entities.0.insert(*layer);
        pos.set(spawn.pos());
        look.yaw = spawn.yaw;
        look.pitch = spawn.pitch;
        *mode = GameMode::Adventure;
    }
}

fn handle_chunk_loads(
    mut messages: MessageReader<ChunkLoadMessage>,
    mut layer: Single<&mut ChunkLayer, With<LobbyLayer>>,
) {
    for message in messages.read() {
        match &message.status {
            ChunkLoadStatus::Success { .. } => {}
            ChunkLoadStatus::Empty => {
                layer.insert_chunk(message.pos, UnloadedChunk::new());
            }
            ChunkLoadStatus::Failed(e) => {
                tracing::error!(
                    "failed to load lobby chunk ({}, {}): {e:#}",
                    message.pos.x,
                    message.pos.z
                );
                layer.insert_chunk(message.pos, UnloadedChunk::new());
            }
        }
    }
}

fn return_from_void(mut clients: Query<(&mut Position, &mut Look), With<Client>>, spawn: Res<Spawn>) {
    for (mut pos, mut look) in &mut clients {
        if pos.0.y < spawn.void_y {
            pos.set(spawn.pos());
            look.yaw = spawn.yaw;
            look.pitch = spawn.pitch;
        }
    }
}
