# docs/repoformats/v2.markdown
URL: https://github.com/unisonweb/unison/blob/trunk/docs/repoformats/v2.markdown
Branch: trunk (fetched 2026-10-25)

# Overview

The v2 codebase format is a sqlite3 database with some Unison objects stored relationally, and others stored as binary blobs.

[...]

**The `object` table**

```sql
CREATE TABLE object (
  id INTEGER PRIMARY KEY NOT NULL,
  primary_hash_id INTEGER NOT NULL REFERENCES hash(id),
  type_id INTEGER NOT NULL REFERENCES object_type_description(id),
  bytes BLOB NOT NULL
);
```
The object table stores things that are identified by hash and represented with binary blobs instead of relationally. Currently that includes patches and namespace slices, along with term and decl components ("component" ambiguously meaning the whole cycle, a strongly-connected component of definitions). Note that the entire component is identified by a single hash, like patches and namespace slices are.

[...]

In the previous section, we saw `type Reference = Reference' TextId ObjectId`, but we avoid using this type of reference in the binary blobs because one database will number its `TextId`s and `ObjectId`s differently from another database

In order to avoid fully rewriting the ABT to update `TextId` and `ObjectId` replace when transferring definitions from one codebase to another, each definition is bundled with a lookup array for `TextId` and `ObjectId` values. The references in the ABT are indexed by indexes into these _lookup array_, and when a definition is transferred between codebases, only the lookup array is rewritten, not the ABT itself. And hopefully that's better.

[...]

```haskell
-- | Add new formats here
data DeclFormat = Decl (Vector (LocalIds, Decl Symbol))
```

From the above definition, you can see that each `Decl` is paired with its own `LocalIds` structure, as opposed to having a single `LocalIds` that spans the entire component.

### TermFormat.Term

This works the same way!
