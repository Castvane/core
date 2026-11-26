CREATE TABLE cluster_raft_log (
    idx INTEGER PRIMARY KEY CHECK (idx >= 0),
    entry BLOB NOT NULL
) STRICT;

CREATE TABLE cluster_raft_state (
    key TEXT PRIMARY KEY,
    data BLOB NOT NULL
) STRICT;
