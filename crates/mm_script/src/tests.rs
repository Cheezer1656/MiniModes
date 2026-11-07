use std::time::{Duration, Instant};

use chunkedge::prelude::*;
use mm_world::{BlockMirror, Bounds};

use crate::io::{Command, Event, GameInfo, Limits, MatchPhase, PlayerSnap, Snapshot, StepOutput};
use crate::{Bundle, Vm};

fn limits() -> Limits {
    Limits {
        memory: 16 << 20,
        max_overrun_steps: 5,
        ..Limits::default()
    }
}

fn vm_with(files: &[(&str, &str)], limits: Limits) -> Vm {
    vm_info(files, limits, GameInfo::default())
}

fn vm_info(files: &[(&str, &str)], limits: Limits, info: GameInfo) -> Vm {
    let bundle = Bundle::new(files.iter().map(|(p, s)| (p.to_string(), s.to_string()))).unwrap();
    Vm::new(bundle, limits, BlockMirror::flat(Bounds::square(32), 64), info).unwrap()
}

fn vm(main: &str) -> Vm {
    vm_with(&[("main.luau", main)], limits())
}

fn step(vm: &mut Vm, tick: u64, players: &[PlayerSnap], events: Vec<Event>) -> StepOutput {
    let snapshot = Snapshot {
        tick,
        players: players.to_vec(),
        blocks: None,
    };
    vm.step(snapshot, events)
}

fn messages(out: &StepOutput) -> Vec<String> {
    out.diagnostics.iter().map(|d| d.message.clone()).collect()
}

fn player(id: u32, pos: [f64; 3]) -> PlayerSnap {
    PlayerSnap {
        id,
        pos,
        yaw: 0.0,
        pitch: 0.0,
        health: 20.0,
        food: 20,
        game_mode: GameMode::Adventure,
        held: ItemKind::Air,
    }
}

fn join(id: u32, name: &str) -> Event {
    Event::PlayerAdded {
        id,
        name: name.into(),
        uuid: Uuid::from_u128(id as u128),
    }
}

#[test]
fn task_wait_counts_ticks() {
    let mut vm = vm("print('start') local dt = task.wait(1) print('woke', dt) task.wait() print('next')");
    assert_eq!(messages(&step(&mut vm, 0, &[], vec![])), ["start"]);
    for tick in 1..20 {
        let out = step(&mut vm, tick, &[], vec![]);
        assert!(out.diagnostics.is_empty(), "tick {tick}: {:?}", messages(&out));
        assert_eq!(out.next_timer, Some(20));
    }
    assert_eq!(messages(&step(&mut vm, 20, &[], vec![])), ["woke 1"]);
    assert_eq!(messages(&step(&mut vm, 21, &[], vec![])), ["next"]);
}

#[test]
fn spawn_defer_delay_and_cancel() {
    let mut vm = vm(r#"
        task.spawn(function() print("spawned") task.wait(0.5) print("spawned again") end)
        task.defer(function() print("deferred") end)
        print("main")
        local t = task.delay(0.1, function() print("never") end)
        task.cancel(t)
        task.delay(0.1, print, "delayed")
    "#);
    assert_eq!(messages(&step(&mut vm, 0, &[], vec![])), ["spawned", "main", "deferred"]);
    assert_eq!(messages(&step(&mut vm, 2, &[], vec![])), ["delayed"]);
    assert_eq!(messages(&step(&mut vm, 10, &[], vec![])), ["spawned again"]);
}

#[test]
fn long_loops_are_preempted_and_resumed() {
    let mut vm = vm_with(
        &[("main.luau", "local n = 0 while n < 3e7 do n += 1 end print('done', n)")],
        Limits {
            max_overrun_steps: 10_000,
            ..limits()
        },
    );
    let mut steps = 0;
    loop {
        let started = Instant::now();
        let out = step(&mut vm, steps, &[], vec![]);
        assert!(started.elapsed() < Duration::from_millis(40), "a step overran its slice");
        steps += 1;
        if !out.diagnostics.is_empty() {
            assert_eq!(messages(&out), ["done 30000000"]);
            break;
        }
        assert!(out.overran && out.runnable);
    }
    assert!(steps > 2, "took {steps} steps");
}

#[test]
fn infinite_loops_are_killed() {
    let mut vm = vm("task.spawn(function() while true do end end) print('unreachable?')");
    let mut fatal = None;
    for tick in 0..10 {
        let out = step(&mut vm, tick, &[], vec![]);
        if out.fatal.is_some() {
            fatal = Some(tick);
            break;
        }
    }
    assert_eq!(fatal, Some(5), "killed after the sixth straight overrun");
}

#[test]
fn loops_in_unyieldable_places_are_stopped() {
    let mut vm = vm(r#"
        local ok, err = pcall(table.sort, {3, 2, 1}, function(a, b) while true do end end)
        print(ok, err)
    "#);
    let started = Instant::now();
    let out = step(&mut vm, 0, &[], vec![]);
    assert!(started.elapsed() < Duration::from_millis(200));
    // The comparator can't yield, so it's stopped with an error that the
    // thread can't keep running past.
    assert_eq!(messages(&out), ["script ran too long without yielding"]);
}

#[test]
fn memory_is_capped() {
    let mut vm = vm("local t = {} while true do table.insert(t, string.rep('x', 1e6)) end");
    let out = (0..100)
        .map(|tick| step(&mut vm, tick, &[], vec![]))
        .find(|out| !out.diagnostics.is_empty())
        .expect("the loop hits the limit");
    assert_eq!(messages(&out), ["out of memory"]);
    assert!(vm.used_memory() <= 16 << 20);
}

#[test]
fn sandbox_blocks_escapes() {
    let mut vm = vm(r#"
        print(io, debug, loadstring, getfenv, os.execute, os.exit, os.getenv)
        print(pcall(function() game.Players = nil end))
        print(pcall(function() string.rep = nil end))
        print(pcall(require, "../../etc/passwd"))
        print(type(os.clock()))
        x = 1
        print(x)
    "#);
    let out = messages(&step(&mut vm, 0, &[], vec![]));
    assert_eq!(out[0], "nil nil nil nil nil nil nil");
    assert!(out[1].starts_with("false") && out[1].contains("readonly"), "{}", out[1]);
    assert!(out[2].starts_with("false") && out[2].contains("readonly"), "{}", out[2]);
    assert!(out[3].starts_with("false") && out[3].contains("not found"), "{}", out[3]);
    assert_eq!(&out[4..], ["number", "1"]);
}

#[test]
fn require_resolves_caches_and_detects_cycles() {
    let mut vm = vm_with(
        &[
            ("main.luau", "local a = require('lib/a') print(a.name, a.b == require('lib/b'), pcall(require, 'loop'))"),
            ("lib/a.luau", "return { name = 'a', b = require('./b') }"),
            ("lib/b.luau", "return { name = 'b' }"),
            ("loop.luau", "return require('loop')"),
        ],
        limits(),
    );
    let out = messages(&step(&mut vm, 0, &[], vec![]));
    assert!(out[0].starts_with("a true false"), "{}", out[0]);
    assert!(out[0].contains("requires itself"), "{}", out[0]);
}

#[test]
fn syntax_errors_fail_loading() {
    let bundle = Bundle::new([("main.luau".into(), "local = 1".into())]).unwrap();
    let err = Vm::new(bundle, limits(), BlockMirror::flat(Bounds::square(32), 64), GameInfo::default()).err().unwrap();
    assert!(err.starts_with("main.luau:"), "{err}");
}

#[test]
fn players_chat_and_commands() {
    let mut vm = vm(r#"
        game.Players.PlayerAdded:Connect(function(p)
            p:SendMessage("hi " .. p.Name)
            p.Chatted:Connect(function(msg)
                if msg == "tp" then p:Teleport(Vector3.new(1, 70, 2)) end
                if msg == "list" then print(#game.Players:GetPlayers()) end
            end)
        end)
        game.Players.PlayerRemoving:Connect(function(p) print("bye", p.Name, p:IsInGame()) end)
    "#);
    step(&mut vm, 0, &[], vec![]);
    let out = step(&mut vm, 1, &[player(7, [0.0, 65.0, 0.0])], vec![join(7, "Alex")]);
    assert!(matches!(&out.commands[..], [Command::Message { player: 7, text }] if text == "hi Alex"));

    let chat = |m: &str| Event::Chatted {
        id: 7,
        message: m.into(),
    };
    let out = step(&mut vm, 2, &[player(7, [0.0, 65.0, 0.0])], vec![chat("tp"), chat("list")]);
    assert!(matches!(
        &out.commands[..],
        [Command::Teleport { player: 7, pos, look: None }] if *pos == [1.0, 70.0, 2.0]
    ));
    assert_eq!(messages(&out), ["1"]);

    let out = step(&mut vm, 3, &[], vec![Event::PlayerRemoving { id: 7 }]);
    assert_eq!(messages(&out), ["bye Alex false"]);
}

#[test]
fn the_entry_module_sees_players_present_at_start() {
    let mut vm = vm("game.Players.PlayerAdded:Connect(function(p) print('hello', p.Name) end)");
    assert_eq!(messages(&step(&mut vm, 0, &[], vec![join(1, "Sam")])), ["hello Sam"]);
}

#[test]
fn signal_wait_resumes_with_the_arguments() {
    let mut vm = vm("local p = game.Players.PlayerAdded:Wait() print('first', p.Name)");
    step(&mut vm, 0, &[], vec![]);
    let out = step(&mut vm, 1, &[], vec![join(1, "Sam"), join(2, "Kai")]);
    assert_eq!(messages(&out), ["first Sam"]);
}

#[test]
fn block_events_can_be_cancelled_late() {
    let mut vm = vm(r#"
        game.World.BlockBroken:Connect(function(e)
            print(e.Player.Name, e.Block, tostring(e.Position))
            if e.Block == "diamond_ore" then e:Cancel() end
        end)
    "#);
    step(&mut vm, 0, &[], vec![join(1, "Sam")]);
    let broke = |old| Event::BlockBroken {
        player: 1,
        pos: [3, 64, 3],
        old,
    };
    let out = step(&mut vm, 1, &[], vec![broke(BlockState::GRASS_BLOCK)]);
    assert_eq!(messages(&out), ["Sam grass_block 3, 64, 3"]);
    assert!(out.commands.is_empty());
    let out = step(&mut vm, 2, &[], vec![broke(BlockState::DIAMOND_ORE)]);
    assert!(matches!(
        &out.commands[..],
        [Command::Revert { pos: [3, 64, 3], expect, restore }]
            if *expect == BlockState::AIR && *restore == BlockState::DIAMOND_ORE
    ));
}

#[test]
fn blocks_fill_and_read_back() {
    let limits = Limits {
        max_fill_volume: 10_000,
        ..limits()
    };
    let mut vm = vm_with(&[("main.luau", r#"
        game.World:SetBlock(Vector3.new(0, 70, 0), "glass")
        print(game.World:GetBlock(0, 70, 0), game.World:GetBlock(Vector3.new(0, 64, 0)))
        local mine = Region.new(Vector3.new(-2, 60, -2), Vector3.new(1, 63, 1))
        mine:Refill({ stone = 70, coal_ore = 30 })
        print(pcall(game.World.Fill, game.World, Vector3.new(-100, 0, -100), Vector3.new(100, 255, 100), "air"))
        print(game.World:SetBlock(Vector3.new(500, 70, 0), "stone"))
    "#)], limits);
    let out = step(&mut vm, 0, &[], vec![]);
    let msgs = messages(&out);
    assert_eq!(msgs[0], "glass grass_block");
    assert!(msgs[1].starts_with("false") && msgs[1].contains("at most"), "{}", msgs[1]);
    assert_eq!(msgs[2], "false");
    let [Command::SetBlocks(glass), Command::SetBlocks(mine)] = &out.commands[..] else {
        panic!("{:?}", out.commands);
    };
    assert_eq!(glass, &[([0, 70, 0], BlockState::GLASS)]);
    // Only changed blocks are sent: all 32 dirt blocks, and stone that rolled coal.
    assert!(mine.len() >= 32 && mine.len() < 64, "{}", mine.len());
    assert!(mine.iter().all(|(_, s)| *s == BlockState::STONE || *s == BlockState::COAL_ORE));
}

#[test]
fn rules_and_regions_emit_commands_and_track_players() {
    let mut vm = vm(r#"
        game.Rules.CanBreak = false
        local mine = Region.new(Vector3.new(0, 60, 0), Vector3.new(9, 70, 9))
        mine.Rules.CanBreak = true
        mine.Rules.BreakableBlocks = { "stone", "coal_ore" }
        print(mine.Rules.CanBreak, #mine.Rules.BreakableBlocks, game.Rules.CanPlace)
        mine.Entered:Connect(function(p) print("in", p.Name) end)
        mine.Left:Connect(function(p) print("out", p.Name) end)
    "#);
    let out = step(&mut vm, 0, &[], vec![join(1, "Sam")]);
    assert_eq!(messages(&out), ["true 2 nil"]);
    let rules: Vec<_> = out
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::SetRules { region, rules } => Some((region.map(|r| r.0), rules.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[0], (None, crate::Rules { can_break: Some(false), ..Default::default() }));
    assert_eq!(rules[2].1.breakable, Some(vec![BlockKind::Stone, BlockKind::CoalOre]));

    let out = step(&mut vm, 1, &[player(1, [4.5, 65.0, 4.5])], vec![]);
    assert_eq!(messages(&out), ["in Sam"]);
    let out = step(&mut vm, 2, &[player(1, [4.5, 65.0, 4.5])], vec![]);
    assert!(out.diagnostics.is_empty());
    let out = step(&mut vm, 3, &[player(1, [20.0, 65.0, 4.5])], vec![]);
    assert_eq!(messages(&out), ["out Sam"]);
}

#[test]
fn runtime_errors_are_reported_with_the_module() {
    let mut vm = vm_with(
        &[("main.luau", "require('boom')"), ("boom.luau", "local x = nil\nreturn x.y")],
        limits(),
    );
    let out = messages(&step(&mut vm, 0, &[], vec![]));
    assert!(out[0].contains("boom.luau:2"), "{}", out[0]);
}

#[test]
fn match_api_follows_the_host() {
    let main = r#"
        assert(game.IsDraft == false and game.Version == 3 and game.GameId == 7)
        game.Match.MinPlayers = 1
        game.Match.CountdownSeconds = 3
        game.Match.Starting:Connect(function(s) print("starting " .. s .. " " .. game.Match.State) end)
        game.Match.Started:Connect(function()
            print("started")
            game.Match:End(game.Players:GetPlayers())
        end)
        game.Match.Ended:Connect(function(winners) print("winner " .. winners[1].Name) end)
    "#;
    let info = GameInfo {
        game_id: 7,
        version: Some(3),
        max_players: 4,
        ..GameInfo::default()
    };
    let mut vm = vm_info(&[("main.luau", main)], limits(), info);
    let out = step(&mut vm, 1, &[player(1, [0.0; 3])], vec![join(1, "Ann")]);
    assert!(messages(&out).is_empty(), "{:?}", messages(&out));
    assert!(out.commands.iter().any(|c| matches!(c, Command::MatchSettings { min_players: 1, countdown: 3 })));

    let starting = Event::Match {
        phase: MatchPhase::Starting,
        seconds: 3,
        winners: vec![],
    };
    let out = step(&mut vm, 2, &[player(1, [0.0; 3])], vec![starting]);
    assert_eq!(messages(&out), ["starting 3 Starting"]);
    let started = Event::Match {
        phase: MatchPhase::Started,
        seconds: 0,
        winners: vec![],
    };
    let out = step(&mut vm, 3, &[player(1, [0.0; 3])], vec![started]);
    assert_eq!(messages(&out), ["started"]);
    assert!(out.commands.iter().any(|c| matches!(c, Command::EndMatch { winners } if winners == &[1])));
    let ended = Event::Match {
        phase: MatchPhase::Ended,
        seconds: 0,
        winners: vec![1],
    };
    let out = step(&mut vm, 4, &[player(1, [0.0; 3])], vec![ended]);
    assert_eq!(messages(&out), ["winner Ann"]);
}

#[test]
fn open_worlds_have_no_match_and_see_last_shutdown() {
    let info = GameInfo {
        kind: mm_proto::GameKind::OpenWorld {
            persistent_world: true,
        },
        version: Some(1),
        last_shutdown: Some(1_700_000_000),
        ..GameInfo::default()
    };
    let main = "print(tostring(game.Match), game.LastShutdown)";
    let mut vm = vm_info(&[("main.luau", main)], limits(), info);
    assert_eq!(messages(&step(&mut vm, 1, &[], vec![])), ["nil 1700000000"]);
}

#[test]
fn bind_to_close_runs_until_callbacks_return() {
    let main = r#"
        game:BindToClose(function()
            print("closing")
            task.wait(0.1)
            print("saved")
        end)
    "#;
    let mut closing = vm(main);
    let out = step(&mut closing, 1, &[], vec![]);
    assert!(!out.closed);
    let out = step(&mut closing, 2, &[], vec![Event::Closing]);
    assert_eq!(messages(&out), ["closing"]);
    assert!(!out.closed);
    let out = step(&mut closing, 5, &[], vec![]);
    assert_eq!(messages(&out), ["saved"]);
    assert!(out.closed);

    // Nothing bound: closed right away.
    let mut idle = vm("print('hi')");
    assert!(step(&mut idle, 1, &[], vec![Event::Closing]).closed);
}
