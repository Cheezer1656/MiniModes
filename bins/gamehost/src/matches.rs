//! The match flow of match games: wait for `MinPlayers`, count down, start
//! (no one else may join), and after `game.Match:End(winners)` show the
//! result and send everyone back to the lobby. In a draft the flow only
//! moves on `/match start` and `/match end`, so creators can test it alone.

use chunkedge::command::manager::CommandExecutionMessage;
use chunkedge::prelude::*;
use mm_proto::GameKind;
use mm_script::{DEFAULT_COUNTDOWN, DEFAULT_MIN_PLAYERS, Event, MatchPhase};

use crate::instance::{InInstance, Instance, StopRequest};
use crate::players::Ready;
use crate::script::ScriptHost;
use crate::world::GameWorld;

const TICKS_PER_SECOND: u64 = 20;
/// How long the result stays up before players go back to the lobby.
const RESULT_TICKS: u64 = 5 * TICKS_PER_SECOND;

#[derive(Component)]
pub struct MatchFlow {
    pub phase: MatchPhase,
    min_players: u32,
    countdown: u32,
    /// Tick the countdown or the result screen ends.
    until: u64,
    /// Drafts: only `/match start|end` move the flow.
    manual: bool,
}

/// From a VM step: `game.Match` settings or `:End`.
#[derive(Message, Clone)]
pub enum MatchCommand {
    Settings { layer: Entity, min_players: u32, countdown: u32 },
    End { layer: Entity, winners: Vec<Entity> },
}

/// After building, before the next script steps are spawned.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatchSet;

pub struct MatchPlugin;

impl Plugin for MatchPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<MatchCommand>().add_systems(
            Update,
            (add_flows, sync_new_scripts, draft_commands, match_commands, run_matches)
                .chain()
                .in_set(MatchSet),
        );
    }
}

fn add_flows(mut commands: Commands, layers: Query<(Entity, &Instance), Added<GameWorld>>) {
    for (layer, instance) in &layers {
        if instance.spec.kind == GameKind::Match {
            commands.entity(layer).insert(MatchFlow {
                phase: MatchPhase::Waiting,
                min_players: DEFAULT_MIN_PLAYERS.min(instance.spec.max_players),
                countdown: DEFAULT_COUNTDOWN,
                until: 0,
                manual: instance.is_draft(),
            });
        }
    }
}

/// Restarted scripts start out believing the match is waiting.
fn sync_new_scripts(mut layers: Query<(&MatchFlow, &mut ScriptHost), Added<ScriptHost>>) {
    for (flow, mut host) in &mut layers {
        if flow.phase != MatchPhase::Waiting {
            host.push(Event::Match {
                phase: flow.phase,
                seconds: 0,
                winners: Vec::new(),
            });
        }
    }
}

fn set_phase(flow: &mut MatchFlow, host: Option<&mut ScriptHost>, phase: MatchPhase, seconds: u32, winners: Vec<u32>) {
    flow.phase = phase;
    if let Some(host) = host {
        host.push(Event::Match { phase, seconds, winners });
    }
}

fn start_countdown(flow: &mut MatchFlow, host: Option<&mut ScriptHost>, tick: u64) {
    flow.until = tick + u64::from(flow.countdown) * TICKS_PER_SECOND;
    let seconds = flow.countdown;
    set_phase(flow, host, MatchPhase::Starting, seconds, Vec::new());
}

/// `/match start` and `/match end` in a match game's draft.
fn draft_commands(
    mut messages: MessageReader<CommandExecutionMessage>,
    mut players: Query<(&InInstance, &mut Client)>,
    mut layers: Query<(&mut MatchFlow, Option<&mut ScriptHost>)>,
    mut ends: MessageWriter<MatchCommand>,
    server: Res<Server>,
) {
    for cmd in messages.read() {
        let mut args = cmd.command.split_whitespace();
        if args.next() != Some("match") {
            continue;
        }
        let Ok((in_instance, mut client)) = players.get_mut(cmd.executor) else {
            continue;
        };
        let Ok((mut flow, host)) = layers.get_mut(in_instance.0).map_err(|_| ()).and_then(|l| {
            if l.0.manual { Ok(l) } else { Err(()) }
        }) else {
            client.send_chat_message("§c/match only works in the draft of a match game.");
            continue;
        };
        match args.next() {
            Some("start") if flow.phase == MatchPhase::Waiting => {
                start_countdown(&mut flow, host.map(|h| h.into_inner()), server.current_tick() as u64);
            }
            Some("start") => client.send_chat_message("§cThe match is already running; §f/match end§c first."),
            Some("end") if flow.phase != MatchPhase::Ended => {
                ends.write(MatchCommand::End {
                    layer: in_instance.0,
                    winners: Vec::new(),
                });
            }
            Some("end") => client.send_chat_message("§cThe match already ended."),
            _ => client.send_chat_message("§7Usage: §f/match start§7 or §f/match end"),
        }
    }
}

fn match_commands(
    mut messages: MessageReader<MatchCommand>,
    mut layers: Query<(&mut MatchFlow, Option<&mut ScriptHost>)>,
    mut players: Query<(Entity, &InInstance, &Username, &mut Client)>,
    server: Res<Server>,
) {
    let tick = server.current_tick() as u64;
    for msg in messages.read() {
        match msg {
            MatchCommand::Settings {
                layer,
                min_players,
                countdown,
            } => {
                if let Ok((mut flow, _)) = layers.get_mut(*layer) {
                    flow.min_players = (*min_players).max(1);
                    flow.countdown = *countdown;
                }
            }
            MatchCommand::End { layer, winners } => {
                let Ok((mut flow, mut host)) = layers.get_mut(*layer) else {
                    continue;
                };
                if flow.phase == MatchPhase::Ended {
                    continue;
                }
                let names: Vec<_> = winners
                    .iter()
                    .filter_map(|w| players.get(*w).ok().map(|(_, _, name, _)| name.0.clone()))
                    .collect();
                let subtitle = if names.is_empty() {
                    "§7No winner".to_owned()
                } else {
                    format!("§7Winner: §f{}", names.join(", "))
                };
                for (entity, in_instance, _, mut client) in &mut players {
                    if in_instance.0 != *layer {
                        continue;
                    }
                    client.set_title_times(5, 80, 15);
                    client.set_subtitle(subtitle.clone());
                    client.set_title(if winners.contains(&entity) { "§6Victory!" } else { "§cGame over" });
                    client.send_chat_message(format!("§eThe match is over. {subtitle}"));
                }
                let ids = winners
                    .iter()
                    .filter_map(|w| host.as_ref().and_then(|h| h.id_of(*w)))
                    .collect();
                flow.until = tick + RESULT_TICKS;
                set_phase(&mut flow, host.as_deref_mut(), MatchPhase::Ended, 0, ids);
            }
        }
    }
}

fn run_matches(
    mut layers: Query<(Entity, &mut Instance, &mut MatchFlow, Option<&mut ScriptHost>), Without<Despawned>>,
    mut players: Query<(&InInstance, &mut Client), With<Ready>>,
    mut stops: MessageWriter<StopRequest>,
    server: Res<Server>,
) {
    let tick = server.current_tick() as u64;
    for (layer, mut instance, mut flow, mut host) in &mut layers {
        if instance.closing.is_some() {
            continue;
        }
        let present = players.iter().filter(|(p, _)| p.0 == layer).count() as u32;
        let mut tell = |f: &mut dyn FnMut(&mut Client)| {
            for (in_instance, mut client) in &mut players {
                if in_instance.0 == layer {
                    f(&mut client);
                }
            }
        };
        let second = tick.is_multiple_of(TICKS_PER_SECOND);
        match flow.phase {
            MatchPhase::Waiting if flow.manual => {}
            MatchPhase::Waiting if present >= flow.min_players => start_countdown(&mut flow, host.as_deref_mut(), tick),
            MatchPhase::Waiting => {
                if second {
                    let text = format!("§eWaiting for players §f{present}/{}", flow.min_players);
                    tell(&mut |c| c.set_action_bar(text.clone()));
                }
            }
            MatchPhase::Starting if !flow.manual && present < flow.min_players => {
                tell(&mut |c| c.send_chat_message("§cNot enough players; the countdown stopped."));
                set_phase(&mut flow, host.as_deref_mut(), MatchPhase::Waiting, 0, Vec::new());
            }
            MatchPhase::Starting if tick >= flow.until => {
                if !flow.manual {
                    instance.started = true;
                }
                tell(&mut |c| {
                    c.set_title_times(0, 20, 10);
                    c.set_subtitle("");
                    c.set_title("§aGo!");
                });
                set_phase(&mut flow, host.as_deref_mut(), MatchPhase::Started, 0, Vec::new());
            }
            MatchPhase::Starting => {
                let left = flow.until - tick;
                if left.is_multiple_of(TICKS_PER_SECOND) {
                    let seconds = left / TICKS_PER_SECOND;
                    if seconds <= 5 || seconds.is_multiple_of(10) || seconds == u64::from(flow.countdown) {
                        tell(&mut |c| {
                            c.set_title_times(0, 25, 5);
                            c.set_subtitle("§7The match is starting");
                            c.set_title(format!("§e{seconds}"));
                        });
                    }
                }
            }
            // Everyone left a running match: nothing to wait for.
            MatchPhase::Started if present == 0 && !flow.manual => {
                stops.write(StopRequest {
                    instance: instance.spec.id,
                    graceful: true,
                });
            }
            MatchPhase::Started => {}
            MatchPhase::Ended if tick >= flow.until => {
                if flow.manual {
                    tell(&mut |c| c.send_chat_message("§7The match reset; §f/match start§7 runs it again."));
                    set_phase(&mut flow, host.as_deref_mut(), MatchPhase::Waiting, 0, Vec::new());
                } else {
                    stops.write(StopRequest {
                        instance: instance.spec.id,
                        graceful: true,
                    });
                }
            }
            MatchPhase::Ended => {}
        }
    }
}
