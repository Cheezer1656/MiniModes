//! Which shards are connected, what instances they run, and where a new
//! player should go. Pure bookkeeping, so placement is unit-testable.

use std::collections::HashMap;

use mm_proto::ipc::{GameVersion, InstanceId, InstanceStatus, LobbyMsg, ShardId};
use mm_proto::{GameId, GameKind, GameLiveStats};
use tokio::sync::mpsc::UnboundedSender;

/// Identifies one TCP connection, so a stale connection's disconnect can't
/// remove the shard that replaced it.
pub type ConnId = u64;

pub struct Shard {
    pub conn: ConnId,
    pub gate_name: String,
    pub tx: UnboundedSender<LobbyMsg>,
    pub instances: HashMap<InstanceId, Instance>,
    pub tick_p99_ms: f32,
}

pub struct Instance {
    pub status: InstanceStatus,
    /// Lobby-side joins sent as `ExpectPlayer` but not acked yet.
    pub reserved: u32,
}

impl Instance {
    pub fn occupancy(&self) -> u32 {
        self.status.players + self.status.expected + self.reserved
    }

    fn has_room(&self) -> bool {
        self.occupancy() < self.status.spec.max_players
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Placement {
    Existing(ShardId, InstanceId),
    /// Create a new instance on this shard.
    New(ShardId),
    /// A persistent world's single instance is at capacity.
    Full,
    NoShards,
}

#[derive(Default)]
pub struct Directory {
    pub shards: HashMap<ShardId, Shard>,
}

impl Directory {
    /// Adds a shard, returning the one it replaces (same id, new connection).
    pub fn connect(&mut self, id: ShardId, shard: Shard) -> Option<Shard> {
        self.shards.insert(id, shard)
    }

    /// Removes the shard if `conn` is still its connection.
    pub fn disconnect(&mut self, id: ShardId, conn: ConnId) -> Option<Shard> {
        if self.shards.get(&id)?.conn == conn {
            self.shards.remove(&id)
        } else {
            None
        }
    }

    /// Replaces a shard's instance list from a heartbeat, keeping reservations.
    pub fn update(&mut self, id: ShardId, statuses: Vec<InstanceStatus>, tick_p99_ms: f32) {
        let Some(shard) = self.shards.get_mut(&id) else {
            return;
        };
        shard.tick_p99_ms = tick_p99_ms;
        let mut old = std::mem::take(&mut shard.instances);
        shard.instances = statuses
            .into_iter()
            .map(|status| {
                let reserved = old.remove(&status.spec.id).map_or(0, |i| i.reserved);
                (status.spec.id, Instance { status, reserved })
            })
            .collect();
        // An instance the lobby just created with a join in flight may be
        // missing from a heartbeat the shard sent before creating it.
        shard
            .instances
            .extend(old.into_iter().filter(|(_, i)| i.reserved > 0));
    }

    pub fn instance_mut(&mut self, shard: ShardId, id: InstanceId) -> Option<&mut Instance> {
        self.shards.get_mut(&shard)?.instances.get_mut(&id)
    }

    /// Where a player joining `game` at `version` should go. A draft, like a
    /// persistent world, has one instance; `preferred` is where a new one
    /// should go if that shard is connected.
    pub fn place(&self, game: GameId, version: GameVersion, kind: GameKind, preferred: Option<ShardId>) -> Placement {
        let mut ofgame = self.shards.iter().flat_map(|(sid, shard)| {
            shard
                .instances
                .values()
                .filter(move |i| i.status.spec.game == game && i.status.spec.version == version)
                .map(move |i| (*sid, i))
        });

        let single = version == GameVersion::Draft
            || kind
                == GameKind::OpenWorld {
                    persistent_world: true,
                };
        match kind {
            _ if single => {
                if let Some((sid, instance)) = ofgame.next() {
                    return if instance.has_room() {
                        Placement::Existing(sid, instance.status.spec.id)
                    } else {
                        Placement::Full
                    };
                }
            }
            // Fill the fullest instance that still has room, so players meet
            // each other and matches start sooner.
            GameKind::Match | GameKind::OpenWorld { .. } => {
                let best = ofgame
                    .filter(|(_, i)| !i.status.started && i.has_room())
                    .max_by_key(|(sid, i)| (i.occupancy(), std::cmp::Reverse(i.status.spec.id), std::cmp::Reverse(*sid)));
                if let Some((sid, instance)) = best {
                    return Placement::Existing(sid, instance.status.spec.id);
                }
            }
        }

        match preferred {
            Some(shard) if self.shards.contains_key(&shard) => Placement::New(shard),
            _ => self.least_loaded().map_or(Placement::NoShards, Placement::New),
        }
    }

    fn least_loaded(&self) -> Option<ShardId> {
        self.shards
            .iter()
            .min_by_key(|(id, shard)| {
                let players: u32 = shard.instances.values().map(Instance::occupancy).sum();
                (players, shard.instances.len(), **id)
            })
            .map(|(id, _)| *id)
    }

    /// Per-game player counts for the browser.
    pub fn live_stats(&self) -> HashMap<GameId, GameLiveStats> {
        let mut stats: HashMap<GameId, GameLiveStats> = HashMap::new();
        for instance in self.shards.values().flat_map(|s| s.instances.values()) {
            let s = stats.entry(instance.status.spec.game).or_default();
            s.players += instance.status.players;
            if instance.status.spec.kind == GameKind::Match {
                if instance.status.started {
                    s.in_progress += 1;
                } else {
                    s.waiting += 1;
                }
            }
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use mm_proto::ipc::{GameVersion, InstanceSpec};

    use super::*;

    const MATCH: GameKind = GameKind::Match;
    const PUB: GameVersion = GameVersion::Published(1);
    const PERSISTENT: GameKind = GameKind::OpenWorld {
        persistent_world: true,
    };

    fn shard(dir: &mut Directory, id: u16) {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        dir.connect(
            ShardId(id),
            Shard {
                conn: id.into(),
                gate_name: format!("gh-{id}"),
                tx,
                instances: HashMap::new(),
                tick_p99_ms: 0.0,
            },
        );
    }

    fn instance(dir: &mut Directory, shard: u16, id: u64, game: i64, kind: GameKind, players: u32, started: bool) {
        let status = InstanceStatus {
            spec: InstanceSpec {
                id: InstanceId(id),
                game: GameId(game),
                version: GameVersion::Published(1),
                kind,
                max_players: 8,
            },
            players,
            expected: 0,
            started,
        };
        dir.shards
            .get_mut(&ShardId(shard))
            .unwrap()
            .instances
            .insert(InstanceId(id), Instance { status, reserved: 0 });
    }

    #[test]
    fn no_shards() {
        assert_eq!(Directory::default().place(GameId(1), PUB, MATCH, None), Placement::NoShards);
    }

    #[test]
    fn match_fills_fullest_waiting_instance() {
        let mut dir = Directory::default();
        shard(&mut dir, 1);
        shard(&mut dir, 2);
        instance(&mut dir, 1, 10, 1, MATCH, 2, false);
        instance(&mut dir, 2, 11, 1, MATCH, 5, false);
        instance(&mut dir, 2, 12, 1, MATCH, 7, true); // started
        instance(&mut dir, 1, 13, 1, MATCH, 8, false); // full
        instance(&mut dir, 1, 14, 2, MATCH, 6, false); // other game
        assert_eq!(dir.place(GameId(1), PUB, MATCH, None), Placement::Existing(ShardId(2), InstanceId(11)));

        // Reservations count toward capacity.
        dir.instance_mut(ShardId(2), InstanceId(11)).unwrap().reserved = 3;
        assert_eq!(dir.place(GameId(1), PUB, MATCH, None), Placement::Existing(ShardId(1), InstanceId(10)));
    }

    #[test]
    fn new_instances_go_to_least_loaded_shard() {
        let mut dir = Directory::default();
        shard(&mut dir, 1);
        shard(&mut dir, 2);
        instance(&mut dir, 1, 10, 5, MATCH, 3, true);
        assert_eq!(dir.place(GameId(5), PUB, MATCH, None), Placement::New(ShardId(2)));
        instance(&mut dir, 2, 11, 6, MATCH, 4, true);
        assert_eq!(dir.place(GameId(5), PUB, MATCH, None), Placement::New(ShardId(1)));
    }

    #[test]
    fn persistent_world_has_one_instance() {
        let mut dir = Directory::default();
        shard(&mut dir, 1);
        shard(&mut dir, 2);
        assert_eq!(dir.place(GameId(3), PUB, PERSISTENT, None), Placement::New(ShardId(1)));
        instance(&mut dir, 2, 20, 3, PERSISTENT, 7, false);
        assert_eq!(dir.place(GameId(3), PUB, PERSISTENT, None), Placement::Existing(ShardId(2), InstanceId(20)));
        dir.instance_mut(ShardId(2), InstanceId(20)).unwrap().status.players = 8;
        assert_eq!(dir.place(GameId(3), PUB, PERSISTENT, None), Placement::Full);
    }

    #[test]
    fn drafts_are_single_and_separate_from_published() {
        let mut dir = Directory::default();
        shard(&mut dir, 1);
        shard(&mut dir, 2);
        instance(&mut dir, 1, 10, 4, MATCH, 1, false);
        // A match's draft doesn't join the published match instance.
        assert_eq!(dir.place(GameId(4), GameVersion::Draft, MATCH, None), Placement::New(ShardId(2)));
        // The shard that last hosted the draft wins.
        assert_eq!(dir.place(GameId(4), GameVersion::Draft, MATCH, Some(ShardId(1))), Placement::New(ShardId(1)));
        assert_eq!(dir.place(GameId(4), GameVersion::Draft, MATCH, Some(ShardId(9))), Placement::New(ShardId(2)));
    }

    #[test]
    fn heartbeat_keeps_reservations_and_stale_disconnect_is_ignored() {
        let mut dir = Directory::default();
        shard(&mut dir, 1);
        instance(&mut dir, 1, 10, 1, MATCH, 1, false);
        dir.instance_mut(ShardId(1), InstanceId(10)).unwrap().reserved = 2;
        let mut status = dir.shards[&ShardId(1)].instances[&InstanceId(10)].status.clone();
        status.players = 4;
        dir.update(ShardId(1), vec![status], 3.0);
        let i = &dir.shards[&ShardId(1)].instances[&InstanceId(10)];
        assert_eq!((i.status.players, i.reserved), (4, 2));

        // A not-yet-reported instance survives while a join is reserved in it.
        instance(&mut dir, 1, 11, 1, MATCH, 0, false);
        dir.instance_mut(ShardId(1), InstanceId(11)).unwrap().reserved = 1;
        instance(&mut dir, 1, 12, 1, MATCH, 0, false);
        let status = dir.shards[&ShardId(1)].instances[&InstanceId(10)].status.clone();
        dir.update(ShardId(1), vec![status], 3.0);
        let mut ids: Vec<_> = dir.shards[&ShardId(1)].instances.keys().map(|i| i.0).collect();
        ids.sort();
        assert_eq!(ids, [10, 11]);

        assert!(dir.disconnect(ShardId(1), 999).is_none());
        assert!(dir.disconnect(ShardId(1), 1).is_some());
    }

    #[test]
    fn live_stats_split_waiting_and_in_progress() {
        let mut dir = Directory::default();
        shard(&mut dir, 1);
        instance(&mut dir, 1, 10, 1, MATCH, 2, false);
        instance(&mut dir, 1, 11, 1, MATCH, 6, true);
        instance(&mut dir, 1, 12, 2, PERSISTENT, 9, false);
        let stats = dir.live_stats();
        let m = stats[&GameId(1)];
        assert_eq!((m.players, m.waiting, m.in_progress), (8, 1, 1));
        assert_eq!(stats[&GameId(2)].players, 9);
    }
}
