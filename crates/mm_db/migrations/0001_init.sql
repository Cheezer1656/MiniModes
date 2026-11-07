CREATE TABLE players (
    uuid       uuid PRIMARY KEY,
    username   text NOT NULL,
    first_seen timestamptz NOT NULL DEFAULT now(),
    last_seen  timestamptz NOT NULL DEFAULT now()
);

-- Players without a row have the default rank.
CREATE TABLE ranks (
    uuid       uuid PRIMARY KEY REFERENCES players,
    rank       text NOT NULL,
    granted_by uuid REFERENCES players,
    granted_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE games (
    id             bigserial PRIMARY KEY,
    owner          uuid NOT NULL REFERENCES players,
    name           text NOT NULL,
    description    text NOT NULL DEFAULT '',
    icon           text NOT NULL,
    kind           text NOT NULL CHECK (kind IN ('match', 'open_world', 'persistent_world')),
    max_players    integer NOT NULL CHECK (max_players > 0),
    -- Side length of the square build area; fixed by the owner's rank at creation.
    map_size       integer NOT NULL CHECK (map_size IN (128, 256, 512)),
    settings       jsonb NOT NULL DEFAULT '{}',
    -- blake3 hex of the draft's current .mmw; NULL until the first save.
    draft_map_hash text,
    listed         boolean NOT NULL DEFAULT false,
    report_count   integer NOT NULL DEFAULT 0,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX games_owner ON games (owner);
