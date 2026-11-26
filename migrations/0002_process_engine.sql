CREATE TABLE process_jobs (
    uid TEXT PRIMARY KEY NOT NULL,
    domain TEXT NOT NULL,
    id TEXT NOT NULL,
    configuration TEXT NOT NULL CHECK (json_valid(configuration)),
    desired INTEGER NOT NULL CHECK (desired IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(domain, id)
) STRICT;

CREATE TABLE process_metadata (
    uid TEXT NOT NULL REFERENCES process_jobs(uid) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL CHECK (json_valid(value)),
    PRIMARY KEY(uid, key)
) STRICT;

CREATE TABLE core_metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL CHECK (json_valid(value))
) STRICT;

CREATE TABLE process_reports (
    sequence INTEGER PRIMARY KEY,
    uid TEXT NOT NULL REFERENCES process_jobs(uid) ON DELETE CASCADE,
    document TEXT NOT NULL CHECK (json_valid(document))
) STRICT;
CREATE INDEX process_reports_by_job ON process_reports(uid, sequence DESC);
