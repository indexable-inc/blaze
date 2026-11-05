-- The blob store's tables (API.md, "Storage"; docs/remote-cache.md, "The API").
CREATE TABLE tenants (
  id TEXT PRIMARY KEY,
  salt BLOB NOT NULL,
  key_version INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE objects (
  object TEXT PRIMARY KEY,
  uploaded INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE blobs (
  ns TEXT NOT NULL,
  key TEXT NOT NULL,
  object TEXT NOT NULL,
  "offset" INTEGER NOT NULL,
  length INTEGER NOT NULL,
  ulength INTEGER NOT NULL,
  codec INTEGER NOT NULL,
  dict TEXT,
  PRIMARY KEY (ns, key)
) WITHOUT ROWID;

CREATE INDEX blobs_object ON blobs (object);
