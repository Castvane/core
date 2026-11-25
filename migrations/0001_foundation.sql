CREATE TABLE configuration (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version BETWEEN 1 AND 3),
    document TEXT NOT NULL CHECK (json_valid(document) AND json_type(document) = 'object'),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0)
) STRICT;

CREATE TABLE administrator (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    password_hash TEXT NOT NULL CHECK (length(password_hash) > 0),
    generation TEXT NOT NULL CHECK (length(generation) = 36)
) STRICT;

CREATE TABLE signing_key (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    secret BLOB NOT NULL CHECK (length(secret) = 32)
) STRICT;
