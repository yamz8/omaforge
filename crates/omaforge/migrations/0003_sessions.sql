-- Browser sessions. The cookie holds a random token; only its SHA-256 is stored.
CREATE TABLE sessions (
    id           INTEGER PRIMARY KEY,
    user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   TEXT    NOT NULL UNIQUE,
    -- Sent back in every form, so other sites can't submit forms as this session.
    csrf_token   TEXT    NOT NULL,
    created_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at   TEXT    NOT NULL
);

CREATE INDEX sessions_user ON sessions (user_id);
