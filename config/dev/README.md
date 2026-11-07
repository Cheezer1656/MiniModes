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

## Scripts

A draft runs its Luau scripts (`draft_files` in Postgres; `main.luau` is the
entry, see `crates/mm_script/minimodes.d.luau` for the API) once its map
loads. Edit them in the web editor (below), or put files in with SQL or
`mm_db::put_draft_file` and `/script reload` in the draft (`/script stop`
stops them). Script output shows in the draft's chat and the editor console.

`mmbot scripts` (same setup as `drafts`) uploads a small game, checks
events, chat, region rules and reload, and that a runaway loop is stopped
without stalling the shard.

## Web editor

The lobby serves the script editor on `[web] bind` (`http://127.0.0.1:8081`
here; Caddy adds TLS in production, where `secure_cookies` stays on). In a
draft, `/script edit` sends a clickable one-time link (10 minutes) that
signs the browser in for 24 hours; gamehost's `editor_url` is the origin
those links use. To get a link without joining:

```sh
cargo run -p mmbot -- editor-link <game_id>   # as the game's owner
```

Saving with "Reload on save" (or ▶ Run, Ctrl+Enter) restarts the running
draft's scripts through the lobby (`ReloadDraft`); their output streams to
the console. The editor keeps 50 revisions per file, flags saves that would
overwrite someone else's, and the owner can invite up to 7 collaborators
(People), who can then open the draft from My Games too. Monaco loads from
jsDelivr.

`mmbot editor` (same setup as `drafts`, plus `[web]` and `editor_url`)
checks the link, the file API and its limits, reload into the running
draft, the console stream, history, and collaborator access.
