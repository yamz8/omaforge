-- Rules for protected branches, enforced by the pre-receive hook (pushes) and when
-- merging pull requests.
CREATE TABLE branch_rules (
    id                   INTEGER PRIMARY KEY,
    repo_id              INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    -- A branch name, or a pattern where `*` matches anything but `/` (`release/*`).
    pattern              TEXT    NOT NULL,
    require_pull_request INTEGER NOT NULL DEFAULT 1,
    required_approvals   INTEGER NOT NULL DEFAULT 0,
    block_force_push     INTEGER NOT NULL DEFAULT 1,
    block_deletion       INTEGER NOT NULL DEFAULT 1,
    created_at           TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (repo_id, pattern)
);

CREATE TABLE webhooks (
    id         INTEGER PRIMARY KEY,
    repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    url        TEXT    NOT NULL,
    -- Signs each delivery (X-Hub-Signature-256); may be empty.
    secret     TEXT    NOT NULL DEFAULT '',
    -- Comma-separated event names, or `*` for all.
    events     TEXT    NOT NULL DEFAULT 'push',
    active     INTEGER NOT NULL DEFAULT 1,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Each event sent (or to be sent) to a webhook. Pending ones are the delivery queue.
CREATE TABLE deliveries (
    id              INTEGER PRIMARY KEY,
    webhook_id      INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event           TEXT    NOT NULL,
    guid            TEXT    NOT NULL,
    payload         TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'succeeded', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    response_status INTEGER,
    response_body   TEXT,
    error           TEXT,
    duration_ms     INTEGER,
    created_at      TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    delivered_at    TEXT
);

CREATE INDEX deliveries_queue ON deliveries (status, next_attempt_at);
CREATE INDEX deliveries_webhook ON deliveries (webhook_id, id);
