---
status: plan
summary: "Row H0 of runtime.md: what blaze publishes (the settled classes only, by one predicate on a record's lifetime class and namespace), the origin's layout (a directory of digest-named immutable objects and one signed prefix that a reader over-asks in one read), the trust chain every byte passes before it is parsed, the reader's three round-trip waves, the publish job that lays a set out the same bytes whatever order it is given, and what stays out of this piece."
date: 2026-11-17
---
# Published records

The owner's revision of 2026-11-17: published records (std, popular registry crates) are fetched; a
project's records stay on its machine; only CI writes the shared cache; a fresh home does not rebuild
std. The hosted object store is parked; the origin here is a directory with the layout a store would
serve, read by byte range. Everything below builds on blaze-remote's read path (remote-read.md),
which since D130/D132 is exactly the path for records blaze publishes: one home (rule 30), no second
reader. New: the predicate and the layout job (`crates/blaze-remote/src/published.rs`), the prefix
object and the reader's prefix wave (`read/form.rs`, `read/machine.rs`).

## 1. The published set

A record is published when, and only when, both hold:
- its namespace is `Namespace::PUBLIC`. blaze-keyed `namespace_of` gives PUBLIC only on a trusted
  writer (CI) and only for the toolchain's crates or a registry archive known by its checksum, and
  blaze-remote `privacy::namespace_for` keeps it only when every record its key names is PUBLIC and the
  key reads nothing outside the package (no environment variable, no file outside it). So a record
  whose key or read list names a checkout path, a member crate, or a build script that read the
  machine is never PUBLIC, and on a developer's machine nothing is.
- its lifetime class is a settled one (runtime.md design point 2, roots.rs): `Toolchain` (the root
  `tool.<id>`: std, core, alloc's names records with their expansion records beside them, their frozen
  heads) or `Registry` (a package at a version, by its archive's checksum). `Answer` and `Program` are
  never published: they are a checkout's or a program's, whatever their namespace says.

Both, because each guards the other's mistake: a PUBLIC record under a checkout's answer root is
still a checkout's answer. The test is one function, `published::publishable(namespace, class)`,
which returns the proof `Publishable` that the layout job alone takes (rule 76); a record without it
cannot reach a segment. Code records (the tier the plan names) join the set in a later piece; the
predicate already covers them, as std's code is rooted `Toolchain`.

Why safe: a settled record's inputs cannot change under a fixed key (the toolchain's bytes, a
checksummed archive), so it is valid by its key alone, and it names nothing of anyone's machine.
Why useful: it is exactly what a fresh home would otherwise rebuild before checking one line of its
own code (std's names and heads, the dependencies' names).

## 2. The origin's layout

One directory per (toolchain, target), `<origin>/public/<toolchain>/<target>/`, the two parts the
validated identities of blaze-remote `name` (`[0-9a-z._-]`), joined by the driver, never by a byte
read from the origin. In it, every name built by `read::Object::name` from a digest's hex alone:
- `seg-<d>`: a plain segment, byte for byte (blaze-cas `seal_segment`), records in key order;
- `idx-<d>`: its entries, each the store's 48 bytes and the value's BLAKE3 digest, in pages;
- `sum-<d>`, `manifest-<d>`, `head`: as remote-read.md "Objects";
- `prefix`: the one object a fresh reader asks first: `head || manifest || each row's summary`, in
  the manifest's row order. It is the "prefetch list in the head" of runtime.md design point 9: the
  same three objects, so the same checks, in one read. It is replaced last, by rename.

A reader never lists. It asks `prefix` from 0 for `Limits::prefix_most` bytes; the origin answers
what it holds (a short answer is the object's end, not an error, for this ask only). The prefix's
length is then the head's `manifest_len` plus each verified row's `summary_len`: an answer longer or
shorter than that is refused, and a declared length past `prefix_most` is `TooLarge`.

Rollback and freshness: every other object is immutable and named by its digest, so a stale or
replayed `prefix` names older objects that are still true. A reader then finds fewer records, never
a wrong one: a record is valid by its key (rule 25: the origin holds what a local build makes, byte
for byte). A prefix naming an object that is gone gives `Unfetched::Origin { Missing }` for the keys
under it, and those are built locally. No state is kept between commands to refuse an older
generation (rule 70), and none is needed.

Bounds on every field read from the origin (each refusal typed, naming the object): the prefix
`prefix_most` (16 MiB at most, `PREFIX_MOST`); a manifest `MANIFEST_MOST` (4,096 rows); a summary
`SUMMARY_MOST`; an index page `PAGE_ENTRIES_MOST` entries (64 KiB); a record `record_most`; a range
`range_most`; a batch's bytes, keys, asks and pages, `Limits`.

## 3. Trust

Signed: the head (Ed25519 over its first 96 bytes, which bind the namespace, the generation and the
manifest's BLAKE3 digest and length). Verified by `crates/blaze-ed25519` through `HeadCheck`; signing
is `ring`'s under blaze-remote's `sign` feature, which only CI links. The chain from there: manifest
by its digest in the head; each summary by its digest and length in its row (which binds the
segment's digest and length); each index page by its digest among the summary's fences; each payload
by its value digest in its index entry. That per-record digest is the range proof: a range read of a
segment is checked record by record, so no whole-segment digest and no hash tree is needed, and the
tree has it today. Nothing is mapped, parsed as a record or written through before its own check.

Which keys a reader trusts: today the keys its caller gives (`Ed25519Keys`). Where they come from (a
key list signed by an anchor, blaze-remote `trust`; or a key pinned in the toolchain's own manifest)
is the next piece, not this one.

## 4. The reader's waves

A fresh home checking a small project, its keys computed from text before anything is fetched (rule
65), in one batch:
- wave 1: `prefix` (head, manifest, every summary: each key goes through a filter, then the fences,
  to the one index page that may hold it, or to none: absent);
- wave 2: those index pages, each once, deduplicated;
- wave 3: the payload ranges, coalesced (`gap_most`, `range_most`).

So at most 3 waves a batch, 2 for a later batch of the same command (the prefix is held), and no wave
per record. The floor is 3: the pointer must come before what it names, and a payload's place is in
an entry. The report counts `waves`, asks and bytes; the simulation prints them for a small set.

## 5. The publish job

On a home that built the toolchain (CI): walk the toolchain class root `tool.<id>` and the registry
package roots, take each record with its key, tag, namespace and class, and give them to
`published::lay_out`, which:
1. keeps only records with a `Publishable` proof (the others are counted, not refused: a walk meets
   them);
2. sorts by key; refuses two values under one key (rule 21); one value given twice is one;
3. cuts the sorted run into segments of at most `segment_bytes` payload bytes (a longer record is a
   segment of its own), each sealed by blaze-cas `seal_segment`;
4. seals them with `read::publish` into objects, the prefix among them.
Two publishers of one set, in any order, write equal bytes (rule 25): the cut depends only on the
sorted run, a segment's bytes only on its records, a manifest on its rows sorted by digest, and the
signature is deterministic Ed25519. The directory writer then writes every object but `prefix` and
`head`, then those two, each by an atomic rename.

## 6. Not in this piece

- The walk of the roots in blaze-cli (store/roots.rs) feeding `lay_out`, and the directory writer.
- The reader as a namespace of the command's store (store/plain_place.rs; lane RG1 is in that file).
- Turning `prefix_most` on in blaze-cli's reader (store.rs, `Limits::DEFAULT` there): its HTTP
  origin takes only a 206 of exactly the range asked (blaze-h1 `Response::ranged`), so an over-asked
  prefix needs a 206 that ends at the object's end to be taken first.
- HTTP, code records, CI's job file, where the signing keys come from.
