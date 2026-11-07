-- Moderation hold on a game. 'review': out of the browser until a moderator
-- clears it (set by enough reports, a moderator, or a creator ban).
-- 'removed': taken down; it can't be played or republished.
ALTER TABLE games ADD COLUMN hold text CHECK (hold IN ('review', 'removed'));
ALTER TABLE games ADD COLUMN hold_reason text;
-- Open reports are counted from `reports` instead.
ALTER TABLE games DROP COLUMN report_count;

CREATE TABLE reports (
    id          bigserial PRIMARY KEY,
    game        bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    reporter    uuid NOT NULL REFERENCES players,
    reason      text NOT NULL,
    -- The published version being played; NULL when reported from the lobby.
    version     integer,
    created_at  timestamptz NOT NULL DEFAULT now(),
    resolved_at timestamptz,
    resolved_by uuid REFERENCES players
);
-- One open report per player per game.
CREATE UNIQUE INDEX reports_open ON reports (game, reporter) WHERE resolved_at IS NULL;

-- Banned creators can't create, build, edit scripts or publish.
CREATE TABLE creator_bans (
    uuid      uuid PRIMARY KEY REFERENCES players,
    reason    text NOT NULL,
    banned_by uuid REFERENCES players,
    banned_at timestamptz NOT NULL DEFAULT now(),
    -- NULL: until lifted.
    until     timestamptz
);

-- What moderators and admins did, for accountability.
CREATE TABLE mod_actions (
    id        bigserial PRIMARY KEY,
    at        timestamptz NOT NULL DEFAULT now(),
    moderator uuid REFERENCES players,
    action    text NOT NULL,
    game      bigint REFERENCES games ON DELETE SET NULL,
    target    uuid REFERENCES players,
    detail    text NOT NULL DEFAULT ''
);
CREATE INDEX mod_actions_game ON mod_actions (game, at);

-- Every player-facing string scripts produce (chat, titles, item names),
-- kept 30 days in daily partitions so old days drop cheaply.
CREATE TABLE script_text_log (
    at       timestamptz NOT NULL DEFAULT now(),
    game     bigint NOT NULL,
    -- NULL in a draft.
    version  integer,
    instance bigint NOT NULL,
    player   uuid,
    kind     text NOT NULL,
    text     text NOT NULL
) PARTITION BY RANGE (at);
CREATE INDEX script_text_log_game ON script_text_log (game, at);

-- Creates the partitions for yesterday through two days ahead (UTC) and
-- drops those older than `keep_days`. The lobby runs it hourly.
CREATE FUNCTION script_text_log_maintain(keep_days integer) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    today date := (now() AT TIME ZONE 'UTC')::date;
    d date;
    part record;
BEGIN
    FOR d IN SELECT generate_series(today - 1, today + 2, interval '1 day')::date LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS %I PARTITION OF script_text_log FOR VALUES FROM (%L) TO (%L)',
            'script_text_log_' || to_char(d, 'YYYYMMDD'),
            d::timestamp AT TIME ZONE 'UTC',
            (d + 1)::timestamp AT TIME ZONE 'UTC'
        );
    END LOOP;
    FOR part IN
        SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
        WHERE i.inhparent = 'script_text_log'::regclass
          AND c.relname < 'script_text_log_' || to_char(today - keep_days, 'YYYYMMDD')
    LOOP
        EXECUTE format('DROP TABLE %I', part.relname);
    END LOOP;
END $$;
SELECT script_text_log_maintain(30);
