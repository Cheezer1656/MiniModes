-- People the owner lets edit a draft (scripts and map).
CREATE TABLE game_collaborators (
    game     bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    player   uuid NOT NULL REFERENCES players,
    added_by uuid REFERENCES players,
    added_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (game, player)
);
CREATE INDEX game_collaborators_player ON game_collaborators (player);

-- One-time links from `/script edit`. Only blake3 hashes of secrets are stored.
CREATE TABLE editor_tokens (
    hash       bytea PRIMARY KEY,
    player     uuid NOT NULL REFERENCES players,
    game       bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    expires_at timestamptz NOT NULL
);

-- Browser sessions a token was exchanged for. Access to each game is checked
-- per request, so removing a collaborator takes effect immediately.
CREATE TABLE editor_sessions (
    hash       bytea PRIMARY KEY,
    player     uuid NOT NULL REFERENCES players,
    expires_at timestamptz NOT NULL
);

-- Bumped on every save, so the editor can tell when someone else saved first.
ALTER TABLE draft_files ADD COLUMN version bigint NOT NULL DEFAULT 1;

-- Every saved revision; `source` is NULL where the file was deleted.
CREATE TABLE draft_file_history (
    id         bigserial PRIMARY KEY,
    game       bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    path       text NOT NULL,
    source     text,
    updated_by uuid REFERENCES players,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX draft_file_history_path ON draft_file_history (game, path, id DESC);
