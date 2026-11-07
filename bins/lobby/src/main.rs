mod account;
mod catalog;
mod chat;
mod config;
mod coordinator;
mod join;
mod library;
mod menus;
mod metrics;
mod moderation;
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
    // `lobby <config> rank set <player> <rank>`: for granting the first admin.
    let args: Vec<String> = std::env::args().skip(2).collect();
    if let [cmd, set, player, rank] = args.as_slice()
        && cmd == "rank"
        && set == "set"
    {
        return runtime.block_on(set_rank_cli(&config, player, rank));
    }
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

    let metrics = mm_core::metrics::MetricsText::default();
    if let Some(bind) = config.metrics_bind {
        mm_core::metrics::serve(runtime.handle(), bind, metrics.clone());
    }

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
        .insert_resource(config.moderation)
        .insert_resource(metrics)
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
            moderation::ModerationPlugin,
            metrics::MetricsPlugin,
            mm_core::metrics::TickTimerPlugin,
            web::WebPlugin { config: web },
        ))
        .run();

    drop(runtime);
    Ok(())
}

async fn set_rank_cli(config: &LobbyConfig, player: &str, rank: &str) -> anyhow::Result<()> {
    let rank = match rank {
        r if r == account::DEFAULT_RANK || r == "default" => None,
        r if config.ranks.contains_key(r) => Some(r),
        r => anyhow::bail!("no rank `{r}` in the config"),
    };
    let db = mm_db::connect(&mm_db::url_from_env()?, 1).await?;
    let (uuid, name) = mm_db::moderation::find_player(&db, player)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{player} has never joined; join once first"))?;
    mm_db::moderation::set_rank(&db, uuid, rank, None).await?;
    println!("{name} ({uuid}) is now {}; they get it on their next join", rank.unwrap_or(account::DEFAULT_RANK));
    Ok(())
}
