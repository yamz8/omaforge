-- Machines that run CI jobs (`omaforge-runner`), site-wide.
CREATE TABLE runners (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL UNIQUE,
    token_hash   TEXT    NOT NULL UNIQUE,
    -- Comma-separated labels from the runner's last claim.
    labels       TEXT    NOT NULL DEFAULT '',
    version      TEXT    NOT NULL DEFAULT '',
    last_seen_at TEXT,
    created_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- One run of one workflow file for one event.
CREATE TABLE workflow_runs (
    id            INTEGER PRIMARY KEY,
    repo_id       INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    -- Counts up per repository: "#12".
    number        INTEGER NOT NULL,
    workflow_name TEXT    NOT NULL,
    workflow_path TEXT    NOT NULL,
    -- The commit subject or pull request title.
    title         TEXT    NOT NULL,
    event         TEXT    NOT NULL,
    git_ref       TEXT    NOT NULL,
    head_branch   TEXT,
    head_sha      TEXT    NOT NULL,
    pull_number   INTEGER,
    actor_id      INTEGER REFERENCES users(id) ON DELETE SET NULL,
    -- The event's webhook payload, for `github.event`.
    payload       TEXT    NOT NULL DEFAULT '{}',
    status        TEXT    NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'in_progress', 'completed')),
    conclusion    TEXT    CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
    -- Why the workflow couldn't run (an invalid file).
    error         TEXT,
    -- Someone cancelled it: it ends as cancelled whatever its jobs did.
    cancelled     INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at    TEXT,
    completed_at  TEXT,
    UNIQUE (repo_id, number)
);

CREATE INDEX workflow_runs_sha ON workflow_runs (repo_id, head_sha);

-- One job of a run (one per matrix combination).
CREATE TABLE ci_jobs (
    id               INTEGER PRIMARY KEY,
    run_id           INTEGER NOT NULL REFERENCES workflow_runs(id) ON DELETE CASCADE,
    -- The job's id in the workflow file.
    job_key          TEXT    NOT NULL,
    name             TEXT    NOT NULL,
    -- Comma-separated: what the job needs (job keys) and where it runs (labels).
    needs            TEXT    NOT NULL DEFAULT '',
    runs_on          TEXT    NOT NULL DEFAULT '',
    -- The job from the workflow plus its matrix values, as JSON.
    spec             TEXT    NOT NULL,
    status           TEXT    NOT NULL CHECK (status IN ('waiting', 'queued', 'in_progress', 'completed')),
    conclusion       TEXT    CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
    -- JSON outputs, for `needs.<job>.outputs`.
    outputs          TEXT    NOT NULL DEFAULT '{}',
    runner_id        INTEGER REFERENCES runners(id) ON DELETE SET NULL,
    token_hash       TEXT    UNIQUE,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at       TEXT,
    completed_at     TEXT,
    heartbeat_at     TEXT
);

CREATE INDEX ci_jobs_run ON ci_jobs (run_id, id);
CREATE INDEX ci_jobs_queue ON ci_jobs (status, id);

CREATE TABLE ci_steps (
    job_id       INTEGER NOT NULL REFERENCES ci_jobs(id) ON DELETE CASCADE,
    number       INTEGER NOT NULL,
    name         TEXT    NOT NULL,
    status       TEXT    NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'in_progress', 'completed')),
    conclusion   TEXT    CHECK (conclusion IN ('success', 'failure', 'cancelled', 'skipped')),
    started_at   TEXT,
    completed_at TEXT,
    PRIMARY KEY (job_id, number)
);

-- Statuses reported by other CI systems through the API (POST /repos/{o}/{r}/statuses/{sha}).
-- The latest per context counts.
CREATE TABLE commit_statuses (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    sha         TEXT    NOT NULL,
    context     TEXT    NOT NULL,
    state       TEXT    NOT NULL CHECK (state IN ('error', 'failure', 'pending', 'success')),
    description TEXT    NOT NULL DEFAULT '',
    target_url  TEXT    NOT NULL DEFAULT '',
    creator_id  INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX commit_statuses_sha ON commit_statuses (repo_id, sha, context, id);

-- Branch protection can require every check on the head commit to pass.
ALTER TABLE branch_rules ADD COLUMN require_checks INTEGER NOT NULL DEFAULT 0;
