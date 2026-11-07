-- A draft's scripts. The web editor (phase 6) adds history on top.
CREATE TABLE draft_files (
    game       bigint NOT NULL REFERENCES games ON DELETE CASCADE,
    path       text NOT NULL,
    source     text NOT NULL,
    updated_by uuid REFERENCES players,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (game, path)
);
