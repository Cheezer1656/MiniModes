//! The lobby's Prometheus metrics: its own tick time and players, each
//! shard as last reported, listed games and the moderation backlog.

use chunkedge::prelude::*;
use mm_core::metrics::{Exposition, MetricsText, TickTimes};

use crate::catalog::GameCatalog;
use crate::coordinator::Coordinator;
use crate::moderation::ModCounts;

pub struct MetricsPlugin;

impl Plugin for MetricsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Last, render);
    }
}

fn render(
    text: Res<MetricsText>,
    times: Res<TickTimes>,
    server: Res<Server>,
    clients: Query<(), With<Client>>,
    coordinator: Option<Res<Coordinator>>,
    catalog: Res<GameCatalog>,
    counts: Res<ModCounts>,
) {
    if server.current_tick() % 20 != 0 {
        return;
    }
    let mut e = Exposition::default();
    e.family("mm_tick_ms", "gauge", "Tick duration over the last 10 seconds, in milliseconds.");
    for q in ["0.5", "0.99", "1"] {
        e.sample("mm_tick_ms", &[("quantile", q)], f64::from(times.quantile(q.parse().unwrap())));
    }
    e.single("mm_slow_ticks_total", "counter", "Ticks that took over 50 ms.", times.slow_ticks as f64);
    e.single("mm_lobby_players", "gauge", "Players in the lobby.", clients.iter().count() as f64);
    e.single("mm_listed_games", "gauge", "Games in the browser.", catalog.len() as f64);
    e.single("mm_open_reports", "gauge", "Unresolved reports.", counts.open_reports as f64);
    e.single("mm_held_games", "gauge", "Games on hold for review.", counts.held_games as f64);

    if let Some(coordinator) = coordinator {
        let shards = &coordinator.directory.shards;
        e.single("mm_shards_connected", "gauge", "Game-host shards connected to the lobby.", shards.len() as f64);
        let rows: Vec<_> = shards
            .iter()
            .map(|(id, s)| {
                let players: u32 = s.instances.values().map(|i| i.status.players).sum();
                (id.0.to_string(), s.gate_name.as_str(), s.tick_p99_ms, s.instances.len(), players)
            })
            .collect();
        e.family("mm_shard_tick_p99_ms", "gauge", "Each shard's p99 tick time, from its heartbeat.");
        for (id, name, p99, ..) in &rows {
            e.sample("mm_shard_tick_p99_ms", &[("shard", id), ("server", name)], f64::from(*p99));
        }
        e.family("mm_shard_instances", "gauge", "Instances each shard runs.");
        for (id, name, _, instances, _) in &rows {
            e.sample("mm_shard_instances", &[("shard", id), ("server", name)], *instances as f64);
        }
        e.family("mm_shard_players", "gauge", "Players in each shard's instances.");
        for (id, name, .., players) in &rows {
            e.sample("mm_shard_players", &[("shard", id), ("server", name)], f64::from(*players));
        }
    }
    text.set(e.finish());
}
