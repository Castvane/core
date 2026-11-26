CREATE TABLE media_aliases (
    position INTEGER PRIMARY KEY,
    pattern TEXT NOT NULL UNIQUE,
    target TEXT NOT NULL
) STRICT;
