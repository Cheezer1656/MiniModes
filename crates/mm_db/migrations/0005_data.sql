-- Script DataStores. Drafts keep their own data (`draft`), so testing never
-- touches what players of the published game saved. `value` is JSON text,
-- not jsonb: jsonb can't hold "\u0000" and reorders keys.
CREATE TABLE datastore_entries (
    game       bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    draft      boolean NOT NULL,
    store      text NOT NULL,
    key        text NOT NULL,
    value      text NOT NULL,
    -- Bumped on every write, for UpdateAsync's compare-and-set.
    version    bigint NOT NULL DEFAULT 1,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (game, draft, store, key)
);

-- Ordered stores hold integers and can be listed by value (leaderboards).
CREATE TABLE ordered_datastore_entries (
    game       bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    draft      boolean NOT NULL,
    store      text NOT NULL,
    key        text NOT NULL,
    value      bigint NOT NULL,
    version    bigint NOT NULL DEFAULT 1,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (game, draft, store, key)
);
CREATE INDEX ordered_datastore_entries_value ON ordered_datastore_entries (game, draft, store, value, key);

-- Bytes each game's stores use (store + key + value lengths), checked
-- against its quota on every write.
CREATE TABLE datastore_usage (
    game  bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    draft boolean NOT NULL,
    bytes bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (game, draft)
);

CREATE TABLE favorites (
    player   uuid NOT NULL REFERENCES players,
    game     bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    added_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (player, game)
);

-- One row per player and published game they joined.
CREATE TABLE plays (
    player        uuid NOT NULL REFERENCES players,
    game          bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    count         integer NOT NULL DEFAULT 1,
    first_played  timestamptz NOT NULL DEFAULT now(),
    last_played   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (player, game)
);
CREATE INDEX plays_game ON plays (game);
CREATE INDEX plays_recent ON plays (player, last_played DESC);

-- Only players who played a game may rate it.
CREATE TABLE ratings (
    player   uuid NOT NULL REFERENCES players,
    game     bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    stars    smallint NOT NULL CHECK (stars BETWEEN 1 AND 5),
    rated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (player, game)
);
CREATE INDEX ratings_game ON ratings (game);
