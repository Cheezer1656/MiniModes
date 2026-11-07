//! A game-host shard: runs game instances for the lobby, behind Gate.

mod build;
mod config;
mod datastore;
mod instance;
mod link;
mod matches;
mod metrics;
mod moderation;
mod players;
mod script;
mod world;

use std::time::Duration;

use chunkedge::prelude::*;
use mm_core::BackendSettings;
use mm_gate::GateClient;

use crate::build::BuildSet;
use crate::config::GamehostConfig;
use crate::instance::{IdleUnload, InstanceSet};
use crate::matches::MatchSet;
use crate::players::PlayerSet;
use crate::script::ScriptSet;
use crate::world::WorldSet;
use crate::link::{GateLink, ShutdownRequests};

fn main() -> anyhow::Result<()> {
    let config: GamehostConfig = mm_core::load_config("gamehost.toml")?;
    let runtime = config.threads.tokio_runtime()?;
    let backend = BackendSettings::behind_gate(config.bind, config.max_players, &runtime)?;
    let gate_name = config.gate_name();

    let gate = GateLink {
        client: GateClient::new(config.gate_api.clone()),
        lobby_server: config.lobby_server.clone(),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::unbounded_channel();

    let storage = world::Storage {
        db: runtime.block_on(mm_db::connect(&mm_db::url_from_env()?, 5))?,
        maps: mm_world::WorldStore::open(config.data_dir.join("worlds"))?,
    };

    let mut app = App::new();
    backend.insert_into(&mut app);
    app.add_plugins(config.threads.task_pool_plugin())
        .add_plugins(DefaultPlugins);

    // After DefaultPlugins so the tasks' logs reach LogPlugin's subscriber.
    let metrics = mm_core::metrics::MetricsText::default();
    if let Some(bind) = config.metrics_bind {
        mm_core::metrics::serve(runtime.handle(), bind, metrics.clone());
    }

    let lobby = link::connect_lobby(runtime.handle(), config.lobby_coordinator, config.shard_id(), gate_name.clone());
    runtime.spawn(link::keep_registered(
        gate.client.clone(),
        gate_name.clone(),
        config.bind.to_string(),
    ));
    runtime.spawn(link::shutdown_on_signal(gate.clone(), gate_name, shutdown_tx));

    app.insert_resource(lobby)
        .insert_resource(metrics)
        .insert_resource(gate)
        .insert_resource(ShutdownRequests(shutdown_rx))
        .insert_resource(IdleUnload(Duration::from_secs(config.idle_unload_secs)))
        .insert_resource(storage)
        .insert_resource(world::Autosave(Duration::from_secs(config.autosave_secs)))
        .insert_resource(world::UpdateBlocksPerTick(config.edit_blocks_per_tick.max(1)))
        .insert_resource(build::EditLimits {
            max_volume: config.max_edit_volume,
            blocks_per_tick: config.edit_blocks_per_tick,
        })
        .insert_resource(config.scripts.script_config(config.editor_url.clone()))
        .configure_sets(
            Update,
            (
                InstanceSet,
                WorldSet,
                PlayerSet,
                ScriptSet::Apply,
                BuildSet,
                MatchSet,
                ScriptSet::Spawn,
            )
                .chain(),
        )
        .add_plugins((
            instance::InstancePlugin,
            world::WorldPlugin,
            build::BuildPlugin,
            script::ScriptPlugin,
            players::PlayersPlugin,
            matches::MatchPlugin,
            moderation::ModerationPlugin,
            metrics::MetricsPlugin,
            mm_core::metrics::TickTimerPlugin,
        ))
        .run();

    drop(runtime);
    Ok(())
}
