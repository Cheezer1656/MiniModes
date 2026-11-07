mod account;
mod catalog;
mod chat;
mod config;
mod coordinator;
mod join;
mod library;
mod menus;
mod publish;
mod ui;
mod web;
mod world;

use chunkedge::prelude::*;
use mm_core::BackendSettings;
use mm_gate::GateClient;

use crate::catalog::GameCatalog;
use crate::config::LobbyConfig;
use crate::coordinator::CoordinatorConfig;
use crate::ui::LobbyUiConfig;
use crate::world::LobbyWorld;

fn main() -> anyhow::Result<()> {
    let config: LobbyConfig = mm_core::load_config("lobby.toml")?;
    let runtime = config.threads.tokio_runtime()?;
    let backend = BackendSettings::behind_gate(config.bind, config.max_players, &runtime)?;

    let db = runtime.block_on(mm_db::connect(&mm_db::url_from_env()?, 5))?;
    anyhow::ensure!(
        config.ranks.contains_key(account::DEFAULT_RANK),
        "[ranks] must define `{}`",
        account::DEFAULT_RANK
    );
    let web = config.web.clone();
    let listed = runtime.block_on(mm_db::listed_games(&db))?;
    let catalog = GameCatalog::new(listed.iter().map(|g| g.summary()).collect());

    let mut app = App::new();
    backend.insert_into(&mut app);
    app.add_plugins(config.threads.task_pool_plugin())
        .add_plugins(DefaultPlugins)
        .insert_resource(config.spawn)
        .insert_resource(LobbyWorld {
            path: config.world,
            keep_loaded_radius: config.keep_loaded_radius,
        })
        .insert_resource(LobbyUiConfig {
            npcs: config.npcs,
            hotbar: config.hotbar,
        })
        .insert_resource(catalog)
        .insert_resource(account::Db(db))
        .insert_resource(account::Ranks(config.ranks))
        .insert_resource(CoordinatorConfig {
            bind: config.coordinator_bind,
            gate: GateClient::new(config.gate_api),
        })
        .add_plugins((
            coordinator::CoordinatorPlugin,
            account::AccountPlugin,
            world::WorldPlugin,
            ui::LobbyUiPlugin,
            chat::ChatPlugin,
            join::JoinPlugin,
            library::LibraryPlugin,
            publish::PublishPlugin,
            web::WebPlugin { config: web },
        ))
        .run();

    drop(runtime);
    Ok(())
}
