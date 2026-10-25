CREATE TABLE labels (
    id          INTEGER PRIMARY KEY,
    repo_id     INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL COLLATE NOCASE,
    -- "#rrggbb"
    color       TEXT    NOT NULL,
    description TEXT    NOT NULL DEFAULT '',
    UNIQUE (repo_id, name)
);

CREATE TABLE issue_labels (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label_id INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, label_id)
);

CREATE TABLE issue_assignees (
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    user_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (issue_id, user_id)
);

-- Give repositories that already exist the same starter labels new ones get.
INSERT INTO labels (repo_id, name, color, description)
SELECT r.id, d.name, d.color, d.description
FROM repos r, (
    SELECT 'bug' AS name, '#d73a4a' AS color, 'Something isn''t working' AS description
    UNION ALL SELECT 'enhancement', '#a2eeef', 'New feature or request'
    UNION ALL SELECT 'documentation', '#0075ca', 'Improvements or additions to documentation'
    UNION ALL SELECT 'question', '#d876e3', 'Further information is requested'
    UNION ALL SELECT 'good first issue', '#7057ff', 'Good for newcomers'
    UNION ALL SELECT 'duplicate', '#cfd3d7', 'This issue or pull request already exists'
    UNION ALL SELECT 'wontfix', '#ffffff', 'This will not be worked on'
) d;
