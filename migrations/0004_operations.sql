CREATE TABLE iam_identity (
    name TEXT PRIMARY KEY CHECK (length(name) BETWEEN 1 AND 128),
    alias TEXT NOT NULL DEFAULT '',
    password_hash TEXT NOT NULL CHECK (length(password_hash) > 0),
    administrator INTEGER NOT NULL CHECK (administrator IN (0, 1)),
    active INTEGER NOT NULL CHECK (active IN (0, 1)),
    generation TEXT NOT NULL CHECK (length(generation) = 36),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE iam_policy (
    identity TEXT NOT NULL REFERENCES iam_identity(name) ON UPDATE CASCADE ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position BETWEEN 0 AND 63),
    document TEXT NOT NULL CHECK (json_valid(document) AND json_type(document) = 'object'),
    PRIMARY KEY (identity, position)
) STRICT;

CREATE TABLE zone (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 128),
    document TEXT NOT NULL CHECK (json_valid(document) AND json_type(document) = 'object')
) STRICT;

CREATE TABLE operation_event (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    category TEXT NOT NULL,
    action TEXT NOT NULL,
    actor TEXT,
    resource TEXT
) STRICT;
CREATE INDEX operation_event_category ON operation_event(category, id);

INSERT INTO zone VALUES ('$default', '{}');
INSERT OR IGNORE INTO zone(id, document)
SELECT DISTINCT json_extract(configuration,'$.zone'), '{}'
FROM process_jobs
WHERE json_extract(configuration,'$.zone') IS NOT NULL
  AND length(json_extract(configuration,'$.zone')) BETWEEN 1 AND 128;

CREATE INDEX process_jobs_zone ON process_jobs(
    coalesce(nullif(json_extract(configuration, '$.zone'), ''), '$default')
);
