CREATE TABLE ssh_keys (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT    NOT NULL,
    -- OpenSSH public key line, without the comment.
    public_key   TEXT    NOT NULL,
    -- SHA256 fingerprint ("SHA256:..."). A key belongs to exactly one user, so a
    -- connection's key identifies who is pushing.
    fingerprint  TEXT    NOT NULL UNIQUE,
    created_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at TEXT
);
