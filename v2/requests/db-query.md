# Requests from db-query (crates/blaze-query)

## 1. blaze-format: a `Writer` over a caller's buffer (owner of `crates/blaze-format`)

- What: `Writer::with_buffer(schema: SchemaHash, bytes: Vec<u8>) -> Writer` (clears `bytes`,
  writes the header as `Writer::new` does, keeps the capacity), and `Writer::finish_into(self,
  root) -> Vec<u8>` returning the buffer so the caller can reuse it.
- Where: `crates/blaze-format/src/lib.rs`, beside `Writer::new` (line ~606).
- Why: every query run calls `blaze_query::write_value`, which does `Writer::new`, a fresh
  4 KiB `Vec` for each run. blaze-query then copies the bytes once into its thread's value
  segment. With a reusable buffer, a run allocates nothing for its value once the buffer has
  grown (docs/v2/plan/goals/principles.md, "preallocate and reuse buffers"). Miss path only;
  hits already allocate nothing. Once it exists, blaze-query keeps one buffer per thread in its
  thread-local run stacks (`lib.rs`, `Stacks`).

> applied (format is DONE): `Writer::with_buffer(schema, bytes)` in `crates/blaze-format/src/lib.rs`, which clears `bytes`, keeps its capacity and writes the header; `Writer::new` is it over a fresh 4 KiB `Vec`. No `finish_into`: `Writer::finish` already returns the buffer by value, so hand that back to `with_buffer`. Test `a_reused_buffer_writes_the_same_bytes_in_its_own_allocation` (`tests.rs`).

## 2. Interface note, no action needed: `Query::fingerprint` takes the value's bytes

`Query::fingerprint(value)` is now `Query::fingerprint(value, bytes)`, with `bytes` the value as
written, and its default hashes those bytes instead of writing the value a second time. No
crate in the tree overrides it (`rg 'fn fingerprint' crates` finds no `impl Query` that does),
so no caller changes. If `docs/v2/plan/reference/crates.md` ever lists the trait's methods, it
should show the new signature.

> applied: nothing to change; `docs/v2/plan/reference/crates.md` lists no `Query` methods.

## 3. Root `Cargo.toml`: `blaze-mem` as a workspace dependency (owner of the root manifest)

- What: `blaze-mem = { path = "crates/blaze-mem" }` under `[workspace.dependencies]`, beside
  `blaze-reclaim` (line ~140).
- Why: blaze-query's columns (`table::Rows`: memo rows, value rows, every map's entries) find a
  row through a dependent load of its chunk's base. Over `blaze_mem::Region` (reserved once,
  committed as it grows, never moved) a row's address is base + index × size, which takes two
  dependent L1 loads off every warm hit: from ≈ 1.6× to ≈ 1.2× the hot hit's bound
  (`docs/v2/plan/lanes/db/speed-of-light.md`, "blaze-query", "Warm hit"). Once it exists,
  blaze-query adds `blaze-mem.workspace = true` and moves `Rows` onto regions tagged
  `Tag::Query`.

> applied by requests: `blaze-mem = { path = \"crates/blaze-mem\" }` beside `blaze-reclaim`; new to the lock.

## 4. blaze-graph: run a batch of closures as plane tasks and wait for them, from outside a task (owner of `crates/blaze-graph`)

- What: something like `blaze_graph::scope(|spawn| { spawn(job); ... })`, which runs each job as
  a plane task and returns once all of them have finished. The caller is a thread that is not in
  a task (the daemon calling `Database::collect`).
- Why: `collect`'s marking is one thread walking every reachable memo, about 1.1 s at
  nushell's scale against 6 ms on all cores
  (`docs/v2/plan/lanes/db/speed-of-light.md`, "blaze-query", "Collection"). The marks are
  already a bitset by memo ID, so parallel marking is a `fetch_or` on the shared words, with
  the roots split across jobs. The database must not start threads of its own, so it needs the
  plane's.

> applied (core is DONE; nothing new needed): `blaze_graph::pool::Pool::scope(|scope| { scope.submit(None, job, done); .. })` (`pool.rs:417`) is it: jobs that borrow what outlives the call, run on the pool's workers, the call returning once every one has run (a job's panic goes to its `done`). From a thread that is not a worker it parks; on a worker it pumps. The database takes the daemon's `&Pool` (as `ide-db`'s `run_on_pool` does) and starts no threads.

## 5. tools/v2/hot_lint.py: its `crates` field can't be read (owner of `tools/v2/hot_lint.py` / `python/typed-input`)

- What: `PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": ".", "crates": ["blaze-query"]}'`
  stops with `TypeError: typed_input: crates has a type it does not read: list[str]`. Either
  typed-input learns `list[str]` (it is the shape every "which crates" input takes) or the
  field becomes a type it reads (a `tuple[str, ...]`).
- Why: the command the coordinator gave every lane fails before it reads a file. blaze-query was
  linted by calling `hot_lint.lint_file` on each of its non-test files directly; that is clean.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list (with the other hot_lint `crates` reports).

> checked by db-audit-dbsub (2026-10-29 23:20): in. `crates` is a `tuple[str, ...]` (`tools/v2/hot_lint.py:39`), and the command above runs.

## 6. blaze-cargo's daemon: the database's image API as it now is (owner of `crates/blaze-cargo/src/serve`; adds to frontend-image's request in `v2/requests/cargo.md`)

`blaze-query` has moved its image onto `blaze-image` (frontend-image's request in
`v2/requests/db.md`, items 1–4). These four call sites stop compiling until they change;
nothing else in the tree uses the database's image API.

- `daemon.rs:260-266`, start: `Database::from_image_with(config, image: Arc<Image<Mapping>>)`,
  called after `intern::restore_symbols(image.clone(), &jobs)` returned `Ok`; it returns
  `DbError::Symbols` if the symbols weren't restored from this image. `DbError::Image(_)` is
  gone. Match `Err(error) if error.is_image()` (it covers `Refused`, `Corrupt`, `Invalid` and
  `Symbols`), print it, and start empty, as today.
- `daemon.rs:369` and `:564`, and `job.rs:1658`:
  `db.write_image(&mut builder, &mut live) -> Result<(), DbError>`. It writes the database's
  five sections into the daemon's `Builder` and marks the symbols its values name. It returns
  `Err(DbError::Unknown(kind))`, before writing anything, if a memo's kind is not registered.
  The daemon then adds its own sections, calls `intern::write_symbols(&live, &mut builder)`,
  and pushes `builder.finish()` as one `Effect::Write`.
- `daemon.rs:568-583`, settle, and `job.rs:1606`: `Database::settled()` is gone, and so are
  the database's `ImageWrite` and `ImageMap` effects; the image file is the daemon's now. On
  its write's `Done::Written`, the daemon maps the file (`Effect::Map`). On `Done::Mapped`, it
  calls `Arc::new(Image::open(mapping)?)` and then `db.settle(image, &mut effects) ->
  Result<(), DbError>`. Before, settle returned `Ok(true)`; now it returns `Ok(())`. The
  "image mapped" flag `settled()` gave becomes the daemon's own, set when the mapping arrives.
- New: `db.take_corrupt() -> Vec<blaze_image::Corrupt>`, the database's image groups found
  corrupt since the last call, each once. Their keys were computed again. Report each by
  section and group, as the VFS's `corrupt()` is reported.
- The tests at `serve/tests.rs:732` and `:960` wait on `it.settled()`. That is the daemon's
  own `settled`, which keeps working once it tracks the flag above.

> forwarded to cargo (`serve/` is its; cargo-sync told).

> answered by lane cargo (cargo-inbox): done at `crates/blaze-cargo/src/serve/daemon.rs:279` (`restore_symbols` then `Database::from_image_with`, `DbError::is_image` printed and an empty db, any other `DbError` stops the daemon), `crates/blaze-cargo/src/serve/image.rs:128` (`write_image(&mut builder, &mut live)`), `crates/blaze-cargo/src/serve/daemon.rs:647` (`settle` once the image is mapped and nothing holds the db, then `reclaim`) and `crates/blaze-cargo/src/serve/daemon.rs:621` (`take_corrupt` printed after each job).

## 7. Answer to frontend-image ("the daemon image on `blaze-image`", `v2/requests/db.md`)

> applied by db-query, items 1–4 (nothing built or run):
> 1. `crates/blaze-query/Cargo.toml`: `blaze-image` and `intern` added.
> 2. `image.rs` is five sections, `DB_MEMOS`, `DB_EDGES`, `DB_TABLE`, `DB_INPUTS` and
>    `DB_BLOB`, each with its own schema (`... 2: ...`). Records name offsets into `DB_BLOB`,
>    and values go at `align(16)`. `ImageMap` holds the `Arc<Image<Mapping>>` and the five
>    `Section`s, and opening reads no byte of them. Reads go through `Image::range`:
>    - `Crc` and `CrcTable` causes come back as `Err(Corrupt)`;
>    - `Bounds` and `Misaligned` are a writer bug and panic, naming it.
>    A corrupt group gives the key an empty slot, so it is recomputed and the group is never
>    read again. It is noted once and taken with `Database::take_corrupt`. `inputs()` failing
>    at start is `DbError::Corrupt`, and the daemon starts empty. The test
>    `a_corrupt_group_drops_only_the_memos_in_it` flips one byte in each group of each db
>    section (4 KiB groups, 300 keys). It checks that each flipped group is reported once by
>    section and group, that values stay right, that some keys and not all were recomputed,
>    and that a group holding paths empties the database at open.
> 3. `from_image_with(config, Arc<Image<Mapping>>)` returns `DbError::Symbols` unless
>    `intern::restored_from() == Some(image.hash())`. Settle opens its own just-written image
>    with this process's symbols, so it skips that check.
> 4. `write_image(&mut Builder, &mut intern::Live) -> Result<(), DbError>`. Each kind keeps
>    `<Q::Value as Persist>::symbols` from `register`, and every ID it pushes is `mark_id`ed.
>    A memo of an unregistered kind is `DbError::Unknown`, before anything is written.
> Items 5 (hir-def and hir-ty persist) and 6 (salsa interned tables, item IDs) are those
> crates' owners', not db-query's. The daemon's side is request 6 above.

## 8. load-cargo: `Tree::file` is a `Result` of `Bytes` (owner of `crates/load-cargo`, lane cargo)

`blaze_query::Tree::file` is now
`Result<Option<(ContentId, Mode, blaze_vfs::Bytes)>, blaze_vfs::Corrupt>`, following
`blaze_vfs::Reader::file` (frontend-journal-tree, `v2/requests/db.md`). For `Reader` it gives the
VFS's `Bytes` with no copy. `crates/load-cargo/src/v2/files.rs:91` no longer compiles: it
matches on an `Option` of an `Arc<[u8]>`. What I'd do:
1. `Node::File(ContentId, blaze_vfs::Bytes)`, and `read` returns `Option<blaze_vfs::Bytes>`
   (`Deref<Target = [u8]>`, so `crates.rs:359` reads it as it does now).
2. `match self.tree.file(root, &under).map_err(|corrupt| LowerError::at(format!("`{path}`: {corrupt}"), &path))? {`.
   An `Err` must fail the load naming the image's section and group. Never read it as `None`:
   the file would look absent, which is a fallback.
Status: open

> forwarded to cargo (`crates/load-cargo`) (from requests, pass 16; told by tmux).

> checked by db-audit-dbsub (2026-10-29 23:20): in. `crates/load-cargo/src/v2/files.rs:91-94` fails the load with `LowerError::at` naming the corrupt group, and `Node::File` holds the `Bytes` (`:102`).

## 9. Answer to frontend-journal-tree ("`Reader::file` is a `Result` of `Bytes`", `v2/requests/db.md`)

> Done in `crates/blaze-query`, as proposed:
> 1. `Tree::file` returns `Result<Option<(ContentId, Mode, Bytes)>, Corrupt>`. For `Reader` it is
>    `blaze_vfs::Reader::file`, with no clone. `MemTree` holds `Bytes`.
> 2. `InputData::File` holds `Bytes`, and `Cx::file` lends `&**bytes`.
> 3. The input is read before it is kept, with the same race as before: a racing read's value
>    wins. On `Err(corrupt)` nothing goes into `state.inputs`. The corrupt is set in a
>    thread-local, `UNREADABLE`, and the run stops with `Cancelled`, which every query already
>    passes up with `?`. `Pinned::get`, the only entry point that runs queries, clears it on
>    entry. On `Cancelled` it fails with `DbError::Corrupt(corrupt)` if one is set, else
>    `DbError::Cancelled`, so the daemon's retry on `Cancelled` never loops on a bad file.
>    Nothing is kept: no memo is made for a stopped run, so the next request reads the file
>    again. The test `a_file_that_fails_its_check_fails_the_request_and_is_read_again` checks
>    that the request fails with the exact `Corrupt`, that another file of the same view still
>    reads, and that the file is read again once it is mended.
> load-cargo, the other caller of `Tree::file`, is request 8 above.
> applied (an answer, nothing to apply; frontend-journal is DONE).
