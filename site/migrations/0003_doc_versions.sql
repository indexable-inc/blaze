-- A counter per discussable doc, bumped whenever one of its comments is written or deleted. The worker caches the
-- anonymous rendering of a page under its URL plus this version (worker/pages.ts), so a new comment makes a new key
-- and nothing has to be purged. A doc with no row is version 0.
CREATE TABLE doc_versions (
  doc TEXT PRIMARY KEY,
  version INTEGER NOT NULL
);

-- Docs that already have comments start at 1, so the profile trigger below (an UPDATE) has a row to bump for them.
INSERT OR IGNORE INTO doc_versions (doc, version) SELECT DISTINCT doc, 1 FROM comments;

-- The bump is a trigger, so it is part of the very statement that writes the comment: the rate-limited
-- INSERT ... SELECT ... WHERE stays one atomic statement, and a refused insert (no row) bumps nothing.
CREATE TRIGGER comments_insert_bump AFTER INSERT ON comments
BEGIN
  INSERT INTO doc_versions (doc, version) VALUES (NEW.doc, 1)
  ON CONFLICT (doc) DO UPDATE SET version = version + 1;
END;

-- Deleting a comment sets deleted_at and erases its text (worker/comments.ts); any other change to a row
-- (an edit by hand) bumps too, since it changes what the page shows.
CREATE TRIGGER comments_update_bump AFTER UPDATE ON comments
BEGIN
  INSERT INTO doc_versions (doc, version) VALUES (NEW.doc, 1)
  ON CONFLICT (doc) DO UPDATE SET version = version + 1;
END;

CREATE TRIGGER comments_delete_bump AFTER DELETE ON comments
BEGIN
  INSERT INTO doc_versions (doc, version) VALUES (OLD.doc, 1)
  ON CONFLICT (doc) DO UPDATE SET version = version + 1;
END;

-- Pages show each commenter's login, name and avatar; a sign-in that changes them bumps the docs they commented on.
CREATE TRIGGER users_profile_bump AFTER UPDATE OF login, name, avatar_url ON users
WHEN OLD.login IS NOT NEW.login OR OLD.name IS NOT NEW.name OR OLD.avatar_url IS NOT NEW.avatar_url
BEGIN
  UPDATE doc_versions SET version = version + 1 WHERE doc IN (SELECT doc FROM comments WHERE user_id = NEW.id);
END;
