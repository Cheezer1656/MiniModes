-- The latest published version; NULL until the first publish.
ALTER TABLE games ADD COLUMN published_version integer;
ALTER TABLE games ADD COLUMN first_published_at timestamptz;

-- Immutable snapshots of a draft. `map_hash` is NULL when the draft was
-- published before its map was ever saved (a fresh flat map).
CREATE TABLE game_versions (
    game         bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    version      integer NOT NULL CHECK (version > 0),
    map_hash     text,
    published_by uuid REFERENCES players,
    published_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (game, version)
);

CREATE TABLE game_version_files (
    game    bigint NOT NULL,
    version integer NOT NULL,
    path    text NOT NULL,
    source  text NOT NULL,
    PRIMARY KEY (game, version, path),
    FOREIGN KEY (game, version) REFERENCES game_versions ON DELETE CASCADE
);

-- Boxes of the draft that a "scripts + regions" publish copies over a
-- persistent world (`//region add <name>` in the draft). Corners inclusive.
CREATE TABLE publish_regions (
    game    bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    name    text NOT NULL,
    min_pos integer[] NOT NULL CHECK (cardinality(min_pos) = 3),
    max_pos integer[] NOT NULL CHECK (cardinality(max_pos) = 3),
    PRIMARY KEY (game, name)
);

-- The one world of a persistent game, created on its first load.
CREATE TABLE live_worlds (
    game          bigint PRIMARY KEY REFERENCES games ON DELETE CASCADE,
    -- Version whose scripts the world runs.
    version       integer NOT NULL,
    map_hash      text,
    -- A publish not applied yet: {"version": n, "mode": "...", "regions": [[min, max], ...]}.
    pending       jsonb,
    saved_at      timestamptz NOT NULL DEFAULT now(),
    -- When the world last unloaded, for `game.LastShutdown`.
    last_shutdown timestamptz
);

-- Worlds replaced by a full-reset publish.
CREATE TABLE live_world_backups (
    id         bigserial PRIMARY KEY,
    game       bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    version    integer NOT NULL,
    map_hash   text,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- Open worlds: each player's inventory, position and stats per game.
CREATE TABLE player_game_state (
    game     bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    player   uuid NOT NULL REFERENCES players,
    state    bytea NOT NULL,
    saved_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (game, player)
);
