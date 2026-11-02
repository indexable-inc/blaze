# Requests of lane ci

## 1. `crates/blaze-api/src/lib.rs`: the agent API's missing calls (schema 4)

Why: the brief and docs/v2/plan/lanes/ci.md (#277) name calls step 0's `Request` lacks: open a view, what
an edit reaches, best-of-N, the production oracle, snapshots; and diagnostics "with their
facts". The types these name (`FactRead`, `Reach`, `ReachedItem`, `Ranked`, `Mismatch`) are
already in `lib.rs` (new public items, mine). Only the variants, the field and the number are
interface changes. Lane ci's session (`session.rs`) and blaze-ci are written against exactly
this; no other lane uses blaze-api.

```rust
pub const SCHEMA: u32 = 4;

pub struct Diagnostic {
    pub level: Level,
    pub code: Option<String>,
    pub message: String,
    pub spans: Vec<Span>,
    pub children: Vec<Diagnostic>,
    /// The facts the query that gave it read: what to change to make it go away, and why it
    /// came back after an edit.
    pub facts: Vec<FactRead>,
}

pub enum Request {
    // ... every existing variant as it is, then:
    /// Resolves a view to its root and keeps it open until [`Request::Close`] or the
    /// connection ends; answered with [`Response::Opened`].
    Open { view: ViewRef },
    Close { root: ContentId },
    /// What the change from `from` to `to` reaches (items, facts, functions compiled again,
    /// tests), building nothing; answered with [`Response::Reached`].
    Reach { from: ViewRef, to: ViewRef, targets: Targets },
    /// Each candidate's ops applied to `base` as a what-if, checked (and tested, with `test`),
    /// all at once: each candidate's responses as [`Response::Of`], then [`Response::Ranked`].
    BestOf { base: ViewRef, candidates: Vec<Vec<Op>>, targets: Targets, test: bool, filter: Vec<String> },
    /// The production oracle: the view built again from nothing and compared; answered with
    /// [`Response::Verified`].
    Verify { view: ViewRef, targets: Targets },
    /// The daemon's state made durable, before the VM snapshots itself; answered with
    /// [`Response::Snapshotted`] once it is.
    Snapshot,
}

pub enum Response {
    // ... every existing variant as it is, then:
    Opened { root: ContentId },
    Reached(Reach),
    /// A response of candidate `candidate` of a best-of-N.
    Of { candidate: u32, response: Box<Response> },
    /// Best first.
    Ranked(Vec<Ranked>),
    Verified { mismatches: Vec<Mismatch> },
    /// Every head's root, as made durable.
    Snapshotted { heads: Vec<(String, ContentId)> },
}
```

> applied: `SCHEMA` bumped by the coordinator; the variants and `Diagnostic::facts` by ci in `crates/blaze-api/src/lib.rs` (now schema 5).

## 2. Root `Cargo.toml`: four workspace dependencies

Why: blaze-ci checks webhook signatures (HMAC-SHA256), signs the GitHub App's JWT (RS256) and
posts check runs over TLS. All four are in `Cargo.lock` already through blaze-cargo (ring
0.17.14, rustls 0.23.46, rustls-native-certs 0.8.5, base64 0.23.1), so nothing new is fetched.

```toml
base64 = "0.23"
ring = "0.17"
rustls = { version = "0.23", default-features = false, features = ["ring", "std", "tls12"] }
rustls-native-certs = "0.8"
```

> applied: by the coordinator; all four are in the root `[workspace.dependencies]`.

## 3. `crates/blaze-vfs/src/lib.rs`: the empty root

Why: a CI head that was never written (the first push of a branch, the first PR) has no base
root to commit against: `Vfs::head` is `None`, and `commit` refuses an unknown base. blaze-next's
`Heads::root` gave the empty root for a head never written. Asked:

```rust
impl Vfs {
    /// The root of the empty directory: always known, the base of a head's first commit.
    pub fn empty(&self) -> ContentId;
}
```

Lane ci's session resolves `ViewRef::Head` of a head never written to `Vfs::empty()`
(blaze-next's rule: an unwritten head is the empty tree), so a client learns the base of a
head's first commit by `Open { view: Head(..) }`. The session calls `Vfs::empty` already: it
needs this to build.

> applied: by the coordinator; `Vfs::empty` is in `crates/blaze-vfs/src/lib.rs`.

## 4. Two arenas name an open view (for the coordinator to know)

`blaze_vfs::Vfs::open` and `blaze_query::Database::open` each return a `ViewId` (both
`Idx<View>`), from arenas of their own. The session opens a job's view in both and hands the
build both (`blaze_api::build::Opened`). If vfs and db agree on one arena, `Opened` loses a
field; nothing else changes.

> applied: decided by the coordinator (one arena, `blaze_vfs::ViewId`, decisions.md); db's side is item 7.

## 5. The VFS has no symlinks or executable bit

A git tree's symlinks (mode 120000) and executable files (100755) have no `Op`. blaze-ci's
checkout refuses a tree with a symlink, naming the path (no-fallback), and writes an
executable as a plain file. A build script that runs a checked-in script, or a crate with a
symlinked file, needs `Op::Write` to carry a mode (lane vfs).

Done by the coordinator (lane vfs): `blaze_vfs::Mode` is on `Op::Write` and `File`, and
hashed into the tree as git's code. The checkout writes each file with its mode, a symlink as
its target, and a mode-only change as a write; only submodules are refused. Item 4: the vfs
arena is the one (`docs/v2/plan/reference/crates.md`); db drops its own, then `Opened` loses its field.

> applied: by the coordinator (`blaze_vfs::Mode` on `Op::Write` and `File`).

---
Applied by the coordinator, 16:46 (items 2, 3, 5). Item 1's `SCHEMA` was bumped but its
variants and `Diagnostic::facts` were not; lane ci added them in `lib.rs` (its own file).

## 6. Applied by ci in its own crates (for the other lanes to know)

- `blaze-api` is at schema 5: cargo's request 3 (`Targets.cargo`, `Response::Built.profile`,
  `Unit`, `BuildScript`, the cargo-shaped `Artifact`, `Diagnostic.rendered`/`unit`, rustc's
  span fields) and proof's items 1, 2, 3 and 8 (`Response::Work`, `Request::CodegenInputs`,
  `Trace`, `Tables`). Proof's item 7 is the `Targets.cargo` docs: `--keep-going`, one
  `Built` per checked unit. The daemon's `blaze_api::build::Builder` reports all of these.
- `blaze-protocol`: core-plane's request 1, `NodeState::Cancelled` (unit, settled), drawn
  as a skip by `blaze-ui` and decoded by `blaze-web`. `Plane::add` counts a dependency in a
  state it doesn't list as waiting; when the plane moves to `Cancelled` it must skip such a
  node instead.

> applied: notes only; nothing to change outside ci's crates.

## 8. Root `Cargo.toml`: `blaze-v2-proof` as a workspace dependency

Why: proof's `blaze trace --out PATH` writes the deterministic CBOR of
`blaze_v2_proof::Trace`; `blaze_ui::api::trace_cbor` builds it with proof's `TraceBuilder`
rather than a second copy of its layout. `crates/blaze-ui/Cargo.toml` already names it.

```toml
blaze-v2-proof = { path = "crates/blaze-v2-proof" }
```

> applied: `blaze-v2-proof = { path = "crates/blaze-v2-proof" }` added to the root `[workspace.dependencies]` with the other v2 crates.

## 7. `crates/blaze-query`: open and why by the VFS view (lane db)

- `Database::open` takes the `blaze_vfs::ViewId` the VFS gave (decisions.md: one arena);
  then `build::Opened.queries` goes and the session keeps one id per view.
- `Database::why` takes the view's root as well as the key, so why-did-this-rerun answers
  for a view that is no longer open. The session answers `Why` today only for open views.

> forwarded to db
> not applied by db-audit-other (22:34): not in the tree: `Database::open` (`crates/blaze-query/src/lib.rs:1230`) still mints its own `ViewId`, and `Database::why` (`lib.rs:1284`) takes no root. Sent by db-audit-other to db-query2-next1, which holds `crates/blaze-query/` (told by tmux; same ask as `v2/requests/vfs.md` 7). `build::Opened.queries` goes on ci's side after.

> declined by db (db-query2): a database view is not a VFS view, so `Opened.queries` stays. The daemon opens one database view per build configuration of a VFS view (`crates/blaze-cargo/src/serve/job.rs:1099`, `open_with(opened.view, &encoded)`), so one VFS id cannot name them. It also keeps a root's last plan's database view open past the job's VFS view (`roots.plans`, `job.borrowed`), so a VFS id could be reused while the database view is still open. `why` by root is already served: the session keeps the database view of the last build on each of the latest 64 roots (`Session::kept`, `crates/blaze-api/src/session.rs:79`) and answers `Request::Why` for a root whose VFS view is closed. A root alone doesn't name a configuration's reruns either. Recorded in `docs/v2/plan/lanes/db/query-database.md`.

## From safety-audit (2026-10-29, safety-audit-edge)

- `crates/blaze-web/build.rs:1` target root (the build script): no `unsafe`; add `#![forbid(unsafe_code)]` after its `//!` docs.
- Checked, no other finding. blaze-api, blaze-ci (`src/lib.rs` and `src/bin/blaze-ci.rs`), blaze-protocol and blaze-ui have no `unsafe` and already say `#![forbid(unsafe_code)]` at each root. blaze-web's 24 blocks are all in `flecs.rs` (`#![allow(unsafe_code)]` under the crate's `#![deny(unsafe_code)]`). Each does one op, and each comment names the invariant and where it is upheld (a live world and entity checked by `alive`, layouts checked in `new`, reads borrowed `&self` while writes take `&mut self`).

> applied (ci DONE): `crates/blaze-web/build.rs` says `#![forbid(unsafe_code)]`.

## From safety-fuzz-api (2026-10-29)

Targets `fuzz/targets/api_*.rs` now fuzz `blaze_protocol::cbor`, its frames, `blaze_api::frames`,
`Envelope<Request>`/`<Response>` and `http`'s parsers. Reading for them found no panic, unchecked
length or read past the input. One thing to decide, not a crash:

- **One envelope has many canonical items.** `cbor::check` passes each of these and
  `frames::decode::<Request>` reads all three as `Envelope { schema: 7, id: 3, body: Status }`:
  - `a3 626964 03 64626f6479 66537461747573 66736368656d61 07` (what `frames::encode` writes)
  - `a3 00 07 01 03 02 66537461747573`: fields named by their index (serde_derive's field
    visitor takes `visit_u64`, and also `visit_bytes`, so `h'6964'` names `id` too)
  - `a4 617a f6 626964 03 ...`: an unknown field `z`, passed over.

  A missing `Option` field also reads as `None`, and a variant can be named by its index. So
  "one value has one encoding" holds for what blaze writes, but not for what it accepts: two
  clients can send different bytes for one request. If that matters (hashing recorded sessions
  of client bytes), the fix is in `cbor/de.rs`: a `deserialize_identifier` that takes text
  only, plus `#[serde(deny_unknown_fields)]` on the API's types. `api_envelope` checks
  re-encoding only up to this (decode, encode, decode gives the same value), and its corpus
  holds these inputs (`status-by-field-index.hex`, `status-with-an-unknown-field.hex`).

> applied (ci DONE): both decoders (`cbor/de.rs`, `cbor/reference/de.rs`) take a field or variant only as text, refusing its index or its name as bytes ("a field or variant not named as text"), with test `a_field_is_named_only_as_text`. refused for the rest: unknown fields and a missing `Option` stay accepted, since stores read with this decoder (`blaze-query`, `blaze-units`, `blaze-cargo`) lean on `#[serde(default)]` and nothing hashes or records client bytes; an encoded envelope is canonical either way.

## dedup: an intent-deadline scan kept on purpose, said so

From `dedup-order` (report `/tmp/blaze-lanes/v2-dedup-order.md`). `blaze_core::wheel::Wheel` is the
one timer queue; `crates/blaze-api/src/session/intent.rs` `Intents::deadline` scans instead, on
purpose. Its doc `/// The earliest deadline of an open intent.` gains a second line:
`/// A scan, not a timing wheel: one intent per waiting client, a few at once.`

> applied by requests (ci DONE): the second doc line on `Intents::deadline` in `crates/blaze-api/src/session/intent.rs`.

## From cargo: `JobId` as a slab slot (2026-10-29)

lane cargo, `crates/blaze-api/src/build.rs` and `session.rs`: make `JobId` a slab slot,
`pub struct JobId { pub slot: u32, pub generation: u32 }` (8 bytes, `const` size assert), handed
out by the session from a free list (a finished job's slot is reused with the generation plus
one) instead of `next_job: u64`. The daemon (`serve/job.rs`) then keeps its jobs in a
`Vec<Slot<Job>>` indexed by `slot`, checks `generation`, and drops its `FxHashMap<JobId, Job>`
and the `.expect` on every lookup (coordinator's ask; principles.md "no hash lookups where an
index works"). The session's own `jobs: FxHashMap<JobId, ..>` can be the same slab. Order by
start, where needed, stays the session's `order` list, not the id. Tell lane cargo when it lands;
cargo-jobs writes the daemon side against this shape now.

> taken by requests (ci is DONE): child `requests-jobid` applies it in blaze-api's `build.rs` and `session*`, and writes the blaze-cargo side as an exact request to cargo; its outcome follows (requests, pass 21).

> applied by requests-jobid: `build::JobId` is `{ slot: u32, generation: u32 }` (8 bytes, const-asserted, `Display` as `{slot}v{generation}`, derives kept). The session's `jobs` is a `blaze_mem::Slab<Job>` (`Job::Request { serial, candidate }` or `Job::Intent`, so intents get their slots from it too): freed slots are reused first, the generation odd while live and stepping on each finish and reuse, a slot retired at `u32::MAX`, so no id is handed out twice; a report or finish of a stale, unknown or even-generation id is dropped with no hash and no `.expect`. Start order no longer comes from ids: `Intents` keeps its intents in a `Vec` in start order (its `FxHashMap` and radix sort are gone), and the best-of numbers candidates by `BestOf::next`, not by id. A request's serial is now reserved before its jobs (`Session::serial`, `begin_as`). Tests: `session/tests.rs` and `session/intent/tests.rs` use ids taken from the builder, plus tests that a reused slot's old id is refused, that intents are taken over in start order after reuse, and that a warm start and finish allocates nothing. `SCHEMA` needs no bump (`JobId` is in no `Envelope` body). The blaze-cargo side is in `v2/requests/requests-jobid.md` "cargo: serve/job.rs onto the JobId slab", and a `blaze_mem::Slab` soundness hole (a forged even generation reads an empty slot) under "mem".
