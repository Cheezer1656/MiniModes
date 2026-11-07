# Local dev stack

```sh
export GATE_VELOCITY_SECRET=dev-secret MINIMODES_VELOCITY_SECRET=dev-secret
export MINIMODES_DATABASE_URL=postgres://minimodes@127.0.0.1:5432/minimodes  # migrations run on start
gate --config config/dev/gate.yml          # offline mode, :25577, API on :8090
cargo run -p lobby -- config/dev/lobby.toml        # coordinator on :25700
cargo run -p gamehost -- config/dev/gamehost.toml  # registers as gh-1 with Gate
cargo run -p mmbot -- join 127.0.0.1:25577 3 5     # 3 bots, hold 5 s
cargo run -p mmbot -- lobby-ui 127.0.0.1:25577
```

Drafts are saved under `data/worlds/` (content-addressed `.mmw` files) with
the pointer in Postgres. `cargo test -p mm_db` runs its queries against
`MINIMODES_TEST_DATABASE_URL` when set (use a disposable database).

In game, pick a game in the browser to be sent to a shard; `/lobby` (or
`/hub`, `/l`, `/leave`) returns you.

## Handoff test without Gate

`mmbot handoff` serves a mock Gate API on :8090 and logs bots straight into
the lobby and shard with self-signed Velocity forwarding. Start it first (the
shard retries registration until it's up), then the lobby and gamehost:

```sh
cargo run -p mmbot -- handoff 127.0.0.1:25600 127.0.0.1:8090
```

`mmbot drafts` (same setup) creates a game through My Games, builds with
`//set`, leaves, and checks the blocks are still there after the draft
unloads and reopens.
