-- People other than the owner who can work on a repository. `read` matters for private
-- repositories; `write` can push, merge and triage. The owner (and site admins) can also
-- change settings.
CREATE TABLE collaborators (
    repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT    NOT NULL CHECK (role IN ('read', 'write')),
    added_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (repo_id, user_id)
);

CREATE INDEX collaborators_user ON collaborators (user_id);
