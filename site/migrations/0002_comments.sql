-- Comments on RFCs (/rfcs/<slug>), written by people signed in with GitHub.

-- One row per GitHub account that has signed in. The GitHub access token is never stored.
CREATE TABLE users (
  id INTEGER PRIMARY KEY,              -- GitHub's numeric user id
  login TEXT NOT NULL,
  name TEXT,
  avatar_url TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- A signed-in browser. The cookie holds a random token; only its SHA-256 is kept here.
CREATE TABLE sessions (
  token_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users (id),
  expires_at INTEGER NOT NULL          -- unix seconds
);
CREATE INDEX sessions_user ON sessions (user_id);

-- A comment on one block of an RFC (a paragraph, list item, heading, code block or table), or on
-- the RFC as a whole (block '_page'). `quote` is the start of the block's text when the comment
-- was written, so a comment still finds its block after the text around it moves.
-- Deleting a comment erases its text: the row stays (ids and rate-limit counts stay stable) with body ''
-- and quote NULL. D1 Time Travel keeps earlier database states for up to 30 days. Pages rendered before the delete
-- can stay in edge caches under their old keys for up to 24 h, served again only if doc_versions goes backwards (a
-- Time Travel restore, say); a deploy after a restore moves every key (worker/pages.ts).
CREATE TABLE comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  doc TEXT NOT NULL,                   -- the RFC's slug
  block TEXT NOT NULL,
  quote TEXT,
  body TEXT NOT NULL,
  user_id INTEGER NOT NULL REFERENCES users (id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  deleted_at TEXT                      -- set when its author (or an admin) deletes it; body becomes '' and quote NULL
);
CREATE INDEX comments_doc ON comments (doc, id);
CREATE INDEX comments_user_created ON comments (user_id, created_at);
