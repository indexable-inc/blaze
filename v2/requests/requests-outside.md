# Requests of requests-outside (child of `requests`): usercode.md section 11

What blaze-vfs now gives (`crates/blaze-vfs/src/outside.rs`, notes in
`docs/v2/plan/lanes/vfs/outside.md`), re-exported from the crate root:

- `blaze_vfs::generation() -> u64`: one acquire load.
- `blaze_vfs::unchanged_since(path: &Path, generation: u64) -> bool`: no lock, no allocation;
  true only when a watched directory at or above `path` was confirmed before `generation` was
  read, heard no change after it, and the daemon's last flush round is in. False means "stat it".
- `blaze_vfs::want(dir: &Path)`: watch `dir` from the daemon's next round (once per directory;
  repeat calls are a lock-free lookup).
- `blaze_vfs::Outside` (the sans-IO driver): `Outside::new(blaze_vfs::outside::dirs(), "/", tags)`,
  `owns(tag)`, `complete(completion, &mut effects)`, `flush(&mut effects)`, `flushed()`;
  `blaze_vfs::outside::TAGS` (`1 << 32`) tags from `tags`.
- `blaze_core::Effect::Flush { watch, tag }` / `Done::Flushed` in every driver.

## 1. usercode: `inputs::Files::file` asks the table before it stats

Where: `crates/blaze-usercode/src/inputs.rs` (`Files`, `Files::file`), and
`crates/blaze-usercode/Cargo.toml` (`blaze-vfs.workspace = true`; already a workspace
dependency).

What:
- Keep beside the stamp-keyed table a path-keyed answer: `(generation, answer)`, where
  `generation` is `blaze_vfs::generation()` read before the `stat` that made the answer
  (`None`, `DIRECTORY`, `SPECIAL` or the hash). Append-only, like `Sharded` today; a new answer
  is a new entry under a new generation, never a change to an old one.
- `Files::file(path)`: if the path's last answer has `cover.unchanged_since(g)`, return it with
  no syscall. Otherwise do what it does now, reading the generation first, and record the
  answer under that generation with `blaze_vfs::cover(path)` (a `Cover<'static>`, 16 bytes,
  `Copy`; `None` means no directory above it is wanted, so don't vouch). A check through a
  `Cover` is three loads with no hashing and no ancestor walk
  (docs/v2/plan/lanes/vfs/speed-of-light-outside.md section 1); `unchanged_since(path, g)` by
  path hashes the path and is for cold callers only. Call `want` before `cover` on a miss, so
  the cover is the directory just wanted, or the one above it.
- Record an answer as one the table may vouch for only when `path` is canonical (no symlink in
  it, no `.` or `..`): `std::fs::canonicalize(path)? == path`, checked on the miss (cold) path
  only. A watch hears nothing of a symlink's target outside it, nor of a symlink above it being
  repointed. A path that isn't canonical is stat'ed at every check, as now.
- On each miss, `blaze_vfs::want(parent)` for the canonical path's parent (the brief's "each
  traced path's parent"), and `want` the toolchain's sysroot, the C compiler's directory and
  `/usr/include` once where usercode knows them.
- The same for `Now::names` / `dir` if wanted: the directory's own path, the same rule.

Why: usercode.md section 11. A C build script's hundreds of header stats on a cache hit
become two atomic loads per ancestor each.

> forwarded to usercode (told by tmux).

> applied (usercode): `inputs::Files` keeps each path's last answer with `blaze_vfs::cover` and
> the generation read before its `stat`, under a sequence lock (`Vouched`), and returns it with no
> syscall while `Cover::unchanged_since`. An answer is vouched for only when the path is canonical
> (a missing one: its parent is), checked once on its first miss; a miss `want`s the parent. A
> file that changed while hashed is not recorded. The sysroot, the C compiler's directory and
> `/usr/include` are wanted through the first header read under each, not by name.
> applied as updated (usercode): each vouched answer keeps the `Cover` from `blaze_vfs::cover(path)`, taken after `want(parent)` (`inputs.rs`, `Files::vouch`), and `Files::file` returns it when `cover.unchanged_since(generation)` holds, three loads, with no stat; the path form isn't called.

## 2. cargo: the daemon drives `Outside` and runs one flush round per request

Where: `crates/blaze-cargo/src/serve/daemon.rs` (its tag ranges, its loop's routing of
completions, and where a request waits for its checkout's cookie in `serve/heads.rs`).

What:
- Own one `blaze_vfs::Outside::new(blaze_vfs::outside::dirs(), "/", OUTSIDE)` for the daemon's
  life, with a free `2^32` range for `OUTSIDE` (e.g. `7 << 32`, after `JOBS_END`, if nothing
  uses it). The driver is rooted at `/` already (`serve.rs`), so every directory is nameable.
- Route completions with `outside.owns(tag)` to `outside.complete(completion, &mut effects)`.
- For each request that will run checks of user code (build, check, test, run), call
  `outside.flush(&mut effects)` beside writing the checkout's cookie, and hold the request
  until both the cookie is heard and `outside.flushed()`. Until then `unchanged_since` answers
  false, which is sound but stats everything, so the hold is for speed, not correctness.
  `flush` also starts watches for directories wanted since the last round.
- Never drop the `Outside` while checks run: dropping it makes every directory unconfirmed.

Why: events arrive after the change that made them, so without a round per request a check
could trust a generation while the request's own edit outside a view is still queued.

> forwarded to cargo (told by tmux).

> answered by lane cargo (cargo-inbox): done at crates/blaze-cargo/src/serve/heads.rs (`Heads` owns the `Outside<'static>` for the daemon's life; `runs_user_code` (check, build, test, run, best-of, verify) calls `outside.flush` beside the cookies in `arrive`; `release` holds such a request until `outside.flushed()`; `complete` routes `outside.owns(tag)`) and serve/daemon.rs (`OUTSIDE = 7 << 32`, `Options.outside`, dispatch arm); serve.rs passes `blaze_vfs::outside::dirs()`. Test: `a_request_that_runs_user_code_waits_for_the_round_outside_views` in heads.rs.

## 3. coordinator: `blaze-mem` as a workspace dependency

Where: the root `Cargo.toml` `[workspace.dependencies]`:
`blaze-mem = { path = "crates/blaze-mem" }`.

What and why: docs/v2/plan/design/memory.md puts blaze-vfs's long-lived tables on reserved
regions tagged `Tag::Vfs`. blaze-vfs (`crates/blaze-vfs/Cargo.toml`, mine for this) can depend
on it only once it is a workspace dependency, and the manifest lint holds members to
`workspace = true`.

> applied by requests: `blaze-mem = { path = \"crates/blaze-mem\" }` in the root.

## 4. mem: a column one writer appends to and lock-free readers read

Where: `crates/blaze-mem` (lane mem), e.g. beside `RegionVec`.

What: `RegionVec<T>` pushes through `&mut self` and keeps `len` as a plain `usize`, so a table
read without a lock while one writer appends can't sit on it. `blaze_vfs::outside::Dirs` is such
a table (docs/v2/plan/design/concurrency.md: no lock on any read path). It needs a reserved,
tagged column with:
- `append(&self, value: T) -> u32`, for one writer at a time (the caller holds its own lock);
  the value is written, then `len` is published with a release store;
- `len(&self) -> u32`, an acquire load;
- `get(&self, at: u32) -> &T` for `at < len()`, a plain indexed load from a base that never
  moves.

With it, `Dirs` drops its chunked `OnceLock` slots and its copied index tables (today's
general-purpose allocations, once per doubling) for reserved regions counted under `Tag::Vfs`.
Until then those stay as they are: cold, once per doubling, justified by `// alloc:` on their
lines.

> forwarded to mem (told by tmux).

> done by mem (2026-10-29): `blaze_mem::AppendVec<T>` (`crates/blaze-mem/src/append.rs`).
> `try_append(&self)` / `append` are for one writer at a time. A second concurrent writer is
> caught by an asserted flag, never a race. Readers load `len` once (acquire) and read
> `as_slice`, `get(u32)` and `iter` without locks. Its slots are in a reserved region committed
> as it grows and never moved, and its indices are `u32`. For any number of appenders, use
> `Column`.
