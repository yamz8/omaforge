-- Issues and pull requests share one table and one number sequence per repository,
-- as on GitHub: #7 is either an issue or a pull request, never both.
CREATE TABLE issues (
    id         INTEGER PRIMARY KEY,
    repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    number     INTEGER NOT NULL,
    author_id  INTEGER NOT NULL REFERENCES users(id),
    title      TEXT    NOT NULL,
    body       TEXT    NOT NULL DEFAULT '',
    state      TEXT    NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed')),
    is_pull    INTEGER NOT NULL DEFAULT 0,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    closed_at  TEXT,
    UNIQUE (repo_id, number)
);

CREATE INDEX issues_repo_state ON issues (repo_id, is_pull, state);

CREATE TABLE pull_requests (
    issue_id     INTEGER PRIMARY KEY REFERENCES issues(id) ON DELETE CASCADE,
    base_branch  TEXT    NOT NULL,
    head_branch  TEXT    NOT NULL,
    -- Last known tips, so a pull request still shows its commits after its branch is
    -- deleted, and exactly what was merged.
    base_sha     TEXT    NOT NULL,
    head_sha     TEXT    NOT NULL,
    merged_at    TEXT,
    merged_by    INTEGER REFERENCES users(id),
    merge_sha    TEXT,
    merge_method TEXT
);

CREATE TABLE comments (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id  INTEGER NOT NULL REFERENCES users(id),
    body       TEXT    NOT NULL,
    -- For review comments on a line of the diff; NULL for conversation comments.
    path       TEXT,
    line       INTEGER,
    side       TEXT CHECK (side IN ('old', 'new')),
    commit_sha TEXT,
    review_id  INTEGER REFERENCES reviews(id) ON DELETE CASCADE,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX comments_issue ON comments (issue_id, created_at);

CREATE TABLE reviews (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author_id  INTEGER NOT NULL REFERENCES users(id),
    state      TEXT    NOT NULL CHECK (state IN ('approved', 'changes_requested', 'commented')),
    body       TEXT    NOT NULL DEFAULT '',
    commit_sha TEXT    NOT NULL,
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Things that happened, for the timeline: closed, reopened, merged, branch deleted.
CREATE TABLE issue_events (
    id         INTEGER PRIMARY KEY,
    issue_id   INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    actor_id   INTEGER NOT NULL REFERENCES users(id),
    kind       TEXT    NOT NULL,
    detail     TEXT    NOT NULL DEFAULT '',
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);
