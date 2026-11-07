//! The shard's Prometheus metrics: tick time, players, and per instance
//! players, script step time and memory.

use std::collections::HashMap;

use chunkedge::prelude::*;
use mm_core::metrics::{Exposition, MetricsText, TickTimes};
use mm_proto::GameKind;
use mm_proto::ipc::GameVersion;

use crate::datastore::DataStats;
use crate::instance::{InInstance, Instance};
use crate::moderation::TextLog;
use crate::script::ScriptHost;

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
    clients: Query<Option<&InInstance>, With<Client>>,
    layers: Query<(Entity, &Instance, Option<&ScriptHost>), Without<Despawned>>,
    text_log: Res<TextLog>,
    data: Res<DataStats>,
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

    let mut per_layer: HashMap<Entity, u32> = HashMap::new();
    let mut total = 0;
    for in_instance in &clients {
        total += 1;
        if let Some(i) = in_instance {
            *per_layer.entry(i.0).or_default() += 1;
        }
    }
    e.single("mm_players", "gauge", "Players connected to this shard.", f64::from(total));

    let mut kinds: HashMap<&str, u32> = HashMap::new();
    for (_, instance, _) in &layers {
        *kinds.entry(kind_label(instance)).or_default() += 1;
    }
    e.family("mm_instances", "gauge", "Running instances by kind.");
    for kind in ["match", "open_world", "persistent_world", "draft"] {
        e.sample("mm_instances", &[("kind", kind)], f64::from(kinds.get(kind).copied().unwrap_or(0)));
    }

    let rows: Vec<_> = layers
        .iter()
        .map(|(entity, instance, host)| {
            let labels = [
                ("game", instance.spec.game.0.to_string()),
                ("instance", instance.spec.id.0.to_string()),
                ("version", match instance.spec.version {
                    GameVersion::Draft => "draft".to_owned(),
                    GameVersion::Published(v) => v.to_string(),
                }),
            ];
            (labels, per_layer.get(&entity).copied().unwrap_or(0), host)
        })
        .collect();
    e.family("mm_instance_players", "gauge", "Players in each instance.");
    for (labels, players, _) in &rows {
        e.sample("mm_instance_players", &refs(labels), f64::from(*players));
    }
    e.family("mm_instance_step_ms", "gauge", "Script step duration over the last 100 steps, in milliseconds.");
    for (labels, _, host) in &rows {
        let Some(host) = host else { continue };
        for q in ["0.5", "0.99"] {
            let mut l = refs(labels);
            l.push(("quantile", q));
            e.sample("mm_instance_step_ms", &l, f64::from(host.step_quantile(q.parse().unwrap())));
        }
    }
    e.family("mm_instance_script_memory_bytes", "gauge", "Luau heap of each instance's scripts.");
    for (labels, _, host) in &rows {
        if let Some(host) = host {
            e.sample("mm_instance_script_memory_bytes", &refs(labels), host.memory() as f64);
        }
    }

    e.single(
        "mm_script_text_logged_total",
        "counter",
        "Script text lines written to the audit log.",
        text_log.logged as f64,
    );
    e.single(
        "mm_script_text_dropped_total",
        "counter",
        "Script text lines over the per-instance rate, not logged.",
        text_log.dropped as f64,
    );
    e.single("mm_datastore_requests_total", "counter", "DataStore requests from scripts.", data.requests as f64);
    text.set(e.finish());
}

fn refs<'a>(labels: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    labels.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn kind_label(instance: &Instance) -> &'static str {
    if instance.is_draft() {
        return "draft";
    }
    match instance.spec.kind {
        GameKind::Match => "match",
        GameKind::OpenWorld {
            persistent_world: false,
        } => "open_world",
        GameKind::OpenWorld {
            persistent_world: true,
        } => "persistent_world",
    }
}
