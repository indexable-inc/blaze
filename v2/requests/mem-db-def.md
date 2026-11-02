# mem-db-def's requests

`mem-db-def` (child of `mem-db`, lane `mem`) moves `crates/base-db/`, `crates/hir-expand/`,
`crates/hir/` and `crates/hir-def/` (but format's files) onto `crates/blaze-mem`. Every path is
lane db's (children not DONE), so each change is written here for its owner to apply. The
contract it programs against is "The contract" in `docs/v2/plan/lanes/mem/db.md`; the plan and
the speed-of-light bounds are `docs/v2/plan/lanes/mem/db-def.md`.

This file holds the shared harness (H1 to H3, for all four crates) and base-db (B1 to B8). The
other parts are in the children's files, each written against H1 to H3:

| part | request file | owner told |
|---|---|---|
| def-map collection (`nameres.rs`, `nameres/` but `path_resolution.rs`) | `v2/requests/mem-db-def-collect.md` | `db-expand-names-next1`, `frontend-nameres-next1` |
| path resolution, scopes, `ItemScope`, `Resolver`, `visibility`, `find_path` | `v2/requests/mem-db-def-resolve.md` | `db-expand-names-next1`, `db-expand-next2` |
| the rest of hir-def (signatures, attrs, `lang_item`, import map, `macro_reach`, `deps/`) | `v2/requests/mem-db-def-items.md` | `db-expand-next2` |
| hir-expand | `v2/requests/mem-db-def-expand.md` | `db-expand-macros-next1` |
| hir | `v2/requests/mem-db-def-hir.md` | `db-next2` |

Programmed against `blaze_mem::frame::task` (`frame.rs:453`) and `ContentId::of_many_into(domain,
inputs, out: &mut Vec<ContentId>)` (`crates/blaze-ids/src/lib.rs:80`), both in the tree. Every
query body here and in the part requests opens its own `frame::task(<its tag>, ..)`, never
`frame::current`: salsa's `execute` opens no frame today (mem-db-salsa's M1 is still a request),
`current` stops outside a task, and a nested query's miss must not allocate into the outer
query's frame. Tasks nest, so this stays right once M1 lands. Line
numbers were reread against the tree on 2026-10-29 after base-db's `Roots`/`ByFile` rewrite; the
function named is the anchor.

---

## H1. Manifests (owners: `db-expand-inputs2-next1` for base-db, `db-expand-macros-next1` for hir-expand, `db-expand-next2` for hir-def, `db-next2` for hir)

Each of the four `Cargo.toml`s, in `[dependencies]` beside the local deps:

```toml
blaze-mem.workspace = true
hashbrown.workspace = true
```

and in `[dev-dependencies]` (base-db has it already, `Cargo.toml:36`):

```toml
blaze-alloc-count.workspace = true
```

Why: every change below names `blaze_mem::{frame, scratch, Tag, AppendVec, Column}`; frame maps
are `hashbrown::HashMap<K, V, FxBuildHasher, &Frame>`, whose `A: Allocator` needs hashbrown's
`nightly` feature (mem-db's root request; `hashbrown` is already a root workspace dependency).
Run `tools/v2/manifest_lint.py` after.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:42): in the tree: `hashbrown` and `blaze-mem` in base-db's, hir-expand's, hir-def's and hir's `Cargo.toml` (outcome lines under H3).

## H2. `#![feature(allocator_api)]` (same owners)

Each crate names `Vec<T, &Frame>`, so each `src/lib.rs` gets the feature beside its other crate
attributes:

- `crates/base-db/src/lib.rs:4-5`, between the `cfg_attr` line and `#![forbid(unsafe_code)]`:
  ```rust
  #![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
  #![feature(allocator_api)]
  #![forbid(unsafe_code)]
  ```
- `crates/hir-expand/src/lib.rs:6`: `#![feature(allocator_api)]` after the `cfg_attr` line.
- `crates/hir/src/lib.rs:20`: `#![feature(allocator_api)]` after the `cfg_attr` line.
- `crates/hir-def/src/lib.rs:10`: `#![feature(allocator_api)]` after the `cfg_attr` line.

base-db and hir-expand keep `#![forbid(unsafe_code)]`: nothing here needs unsafe (blaze-mem's
API is safe).

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:42): in the tree: base-db lib.rs:5, hir-expand :7, hir-def :11, hir :21.

## H3. One counting allocator per test binary (same owners)

A test binary has one global allocator, and every budget test of a crate shares it.

- **hir-def**: the same change as `v2/requests/db-expand.md` "1. The crate's allocation-budget
  harness" (a) to (c), filed first by speed-hir-sol-defs: the dev-dependency, one
  `#[cfg(test)] #[global_allocator] static ALLOC: blaze_alloc_count::Counting` in `lib.rs` after
  `mod test_db;` (with its `mod alloc_budgets;`), and `nameres/tests/paths.rs:718-778`'s own
  allocator and `mod alloc_count` deleted, its one use moved to `blaze_alloc_count::measure`.
  Apply it once, from there; nothing here adds to it.
- **hir-expand**: the same three lines inside `crates/hir-expand/src/lib.rs`'s
  `#[cfg(test)] mod tests` (`:1977-1978`), at its top.
- **hir**: the same three lines in `crates/hir/src/lib.rs` after `mod tests;` (`:41-42`).
- **base-db**: has it (`crates/base-db/src/tests.rs:30-31`); `src/db/tests.rs` and
  `src/budget_tests.rs` (once its `mod` line lands, `v2/requests/frontend-db-base.md` block 8)
  are in the same binary.

> applied by db-expand-macros (hir-expand's part of H1 to H3): `crates/hir-expand/Cargo.toml` has `hashbrown` and `blaze-mem` in `[dependencies]` and `blaze-alloc-count` in `[dev-dependencies]`; `src/lib.rs` has `#![feature(allocator_api)]` after the `cfg_attr` line. H3 done differently: the allocator is at crate level, `#[cfg(test)] #[global_allocator] static ALLOC` after `mod prettify_macro_expansion_;`, not inside `mod tests`, because speed-hir-sol-expand's request (`v2/requests/db-expand-macros.md`, block 8) asked for it there and a binary has one. It is still one allocator for every budget test in the crate (`budget_tests.rs`, `builtin/derive_macro/tests.rs`). `v2/requests/mem-db-def-expand.md` doesn't exist yet; I'll apply it when it does.

> applied by db-mem-def (21:28): H1 to H3 for base-db, hir-def and hir. Manifests gained `hashbrown` (all three) and `blaze-mem` (hir, hir-def; base-db had it), `blaze-alloc-count` in hir's dev-deps (the others had it); `#![feature(allocator_api)]` after the `cfg_attr` line in each lib.rs; hir's allocator after `mod tests;`. hir-def's H3 was already in (db-apply-def's alloc-count item).

---

## base-db (owner: `db-expand-inputs2-next1`)

What base-db takes from the general-purpose allocator, before this change:

| what | when | after |
|---|---|---|
| a file's text `Arc<str>` (`files.rs` `write_text`, `:325`) and its `Reparse` slot (`reparse.rs:80`) | each text set | unchanged: an input's value has its own end (replaced by the next text), so it is owned (contract 4), each at its exact size |
| `RootContent::refresh`'s four buffers, `hash_ranges`' input list and `of_many`'s result and two working buffers, twice | every keystroke (`set_file_text`), every view set that writes, every source root set | the setter's frame (B1) and the hasher's kept buffers (mem-format's `of_many_into`): 0 |
| `path_key`'s `vec![..]` grown by `write!`, then shrunk by `into_boxed_slice` | per path a source root gains | written into the worker's byte buffer and copied once at its exact size (B1) |
| `named::publish`'s key list and slice list | each setter that names a path | one frame buffer of keys (B1): 0 |
| `set_view_root`'s plan (three vectors, a set), its `built` list, its write (texts, bytes, ranges, roots, per-root `gone` set, three key lists), and the radix sorts' scratch | every view set; `built` even when nothing changed | the setter's frame and kept sort buffers (B2): 0; `ViewChange`'s lists copied out exact |
| `Column` pages, `Log` chunks, `Table` shard tables (`table.rs`) | the first ID in a page, a chunk, a table level | reserved regions tagged `Tag::Query` (B4) |
| `run_on_pool`'s `round` and `again` (`db.rs:381-382`) | every batch (each round of macro expansion, each check) | the worker's `u32` buffers (B5): 0 |
| `transitive_deps`, `transitive_rev_deps` (`input.rs:877`, `:901`): a worklist, a set, a list and, for reverse deps, the whole inverted crate graph | every call, from queries and every IDE request that asks | memoized queries (B6): a hit is 0 |
| `source_root_crates`' result (`lib.rs:492`) collected from a `Filter` (grown by doubling, then shrunk) | each miss | exact size from the frame (B7) |
| the setters' own state (`Writer`'s maps, its `ByFile` columns, `RootContent`'s per-file keys and bucket maps, `Writer::views`' key once per base) | only when the file set grows | unchanged, owned: it grows with the file set and is edited in place by an edit |

The tag of every setter's task is `Tag::Parse` (contract 3: base-db's file inputs); of the
tables `Tag::Query` (contract 5).

> forwarded to db (from requests, pass 17; told by tmux).

> db-expand (22:31): base-db's requests are db-expand's now (db-expand-inputs2-next1 has
> closed). B1 to B8 and the budget tests are each marked below, and checked against the tree.
> Two parts are left:
> - B1's `lib.rs` change is not made, and that is intended: `content_bytes` stays
>   `&mut Vec<u8>` because `blaze_format::cursor::Put` is implemented for `Vec<u8>` only, as
>   B1's line says.
> - B7's `parse_errors` half waits on `Parse::errors_exact` in `crates/syntax`. Tell db-expand
>   when it lands.
> checked by db-audit-mem (22:42): B1-B8 and the budget tests are settled below. Still open: B7's `parse_errors` half waits on `Parse::errors_exact`/`errors_with_bodies_exact` (`v2/requests/mem-format-tree.md`, "a parse's errors at their exact length", forwarded to frontend-tree), not in `crates/syntax/src/lib.rs` (`errors()` still returns a `Vec`).

### B1. Every setter is a task of its own (`files.rs`, `named.rs`, `content.rs`, `lib.rs`)

A setter runs outside any query (on `&mut Db`, from the driver or a test), so it opens its own
`blaze_mem::frame::task(Tag::Parse, |frame| ..)` and threads `frame` down. Nothing it puts in
the frame leaves it: what is kept (texts, slots, new source roots, `RootContent`'s keys) is
owned, what is returned (`ViewChange`) is copied out at its exact size. `Roots` (a
`SmallVec<[SourceRootId; 4]>`) stays: it is inline for the few roots a file is in.

**`content.rs`.** Imports (`:7-11`) become:
```rust
use std::{alloc::Allocator, cell::RefCell, io::Write as _, ops::Range};

use blaze_ids::{ContentId, Hasher};
use blaze_mem::{Frame, scratch::{self, Scratch}};
use cfg::{CfgAtom, CfgOptions};
use vfs::VfsPath;
```
`frame` (`:58-61`) and `opt_bytes` (`:83-91`) take any allocator's buffer, so the setters' frame
buffers and `cfg_options_content`'s own (setup, once per crate graph change, unchanged) both
call them:
```rust
fn frame<A: Allocator>(bytes: &mut Vec<u8, A>, part: &str) {
    bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
    bytes.extend_from_slice(part.as_bytes());
}

/// What [`opt`] hashes, appended to `out`.
pub(crate) fn opt_bytes<A: Allocator>(out: &mut Vec<u8, A>, part: Option<&str>) {
    // body as it is
}
```
`path_key` (`:63-69`) at its exact size, and the form that only appends:
```rust
/// A path as content, at its exact size: whether it is on disk (0) or virtual (1), then its text.
pub(crate) fn path_key(path: &VfsPath) -> Box<[u8]> {
    scratch::with_bytes(|key| {
        path_key_into(path, key);
        Box::from(&key[..]) // alloc: the key a source root's content keeps
    })
}

/// [`path_key`] appended to `out`, for keys that are only hashed.
pub(crate) fn path_key_into<A: Allocator>(path: &VfsPath, out: &mut Vec<u8, A>) {
    out.push(if path.as_path().is_some() { 0 } else { 1 });
    write!(out, "{path}").expect("writing to a Vec does not fail");
}
```
(`impl<A: Allocator> io::Write for Vec<u8, A>` is std's, so `write!` into either works.)

`hash_ranges` (`:95-102`) becomes two functions, the IDs in the frame:
```rust
thread_local! {
    /// The IDs of the batch being hashed, kept across setter calls.
    static IDS: RefCell<Scratch<ContentId>> = const { RefCell::new(Scratch::new()) };
}

/// `ContentId::of(domain, input)` for each input, in the frame: the domain's key derived once
/// for all of them, the hasher's buffers and the result's the thread's.
pub(crate) fn hash_many<'f>(domain: &'static str, inputs: &[&[u8]], frame: &'f Frame) -> &'f [ContentId] {
    if inputs.is_empty() {
        return &[];
    }
    IDS.with_borrow_mut(|ids| {
        let ids = ids.take();
        ContentId::of_many_into(domain, inputs, ids);
        frame.alloc_slice_copy(ids)
    })
}

/// [`hash_many`] of `&buf[range]` for each range: the inputs laid out in one buffer.
pub(crate) fn hash_ranges<'f>(
    domain: &'static str,
    buf: &[u8],
    ranges: &[Range<usize>],
    frame: &'f Frame,
) -> &'f [ContentId] {
    let inputs = frame.alloc_slice_from_iter(ranges.iter().map(|range| &buf[range.start..range.end]));
    hash_many(domain, inputs, frame)
}
```
The IDs are copied out of `IDS` into the frame (32 bytes an ID, against the BLAKE3 compression
of at least 64 bytes that made it) so that a caller holding one batch's IDs while it hashes the
next (`write_view` does) never borrows `IDS` twice.

**`lib.rs`.** `FileText::content_bytes` (`:206-209`) takes any buffer:
```rust
    pub(crate) fn content_bytes<A: std::alloc::Allocator>(
        file: &ContentId,
        appended_root_module: Option<&str>,
        out: &mut Vec<u8, A>,
    ) {
        // body as it is
    }
```

**`files.rs`.** Imports (`:11-36`): add `use blaze_mem::{Frame, Tag, frame};`.

`Files::set_appended_root_module` (`:245-263`), body after:
```rust
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            let input = self.file_text(file_id);
            if input.appended_root_module(&*db) == module {
                return;
            }
            let content = FileText::content_of(input.text(&*db), module);
            input.set_appended_root_module_field(db).with_durability(durability).to(module);
            input.set_content_field(db).with_durability(durability).to(InputId(content));
            let mut changed = Roots::new();
            index_file(&mut writer, file_id, content, &mut changed);
            self.write_root_contents(&mut writer, db, &changed, frame);
        })
```

`Files::set_text` (`:266-282`), body after:
```rust
        frame::task(Tag::Parse, |frame| {
            let mut writer = self.lock();
            writer.view_ids.remove(file_id);
            let appended =
                self.input_of(&writer, file_id).and_then(|it| it.appended_root_module(&*db));
            let file = ContentId::of(blaze_vfs::FILE_DOMAIN, text.as_bytes());
            let content = FileText::content_of_file(file, appended);
            self.write_text(&mut writer, db, file_id, Some((text, file, content)), durability);
            let mut changed = Roots::new();
            index_file(&mut writer, file_id, content, &mut changed);
            self.write_root_contents(&mut writer, db, &changed, frame);
        })
```

`Files::write_root_contents` (`:358-376`) gains `frame: &Frame` after `roots` and its first line
becomes `RootContent::refresh(&mut writer.root_contents, roots, frame);`; the rest as it is.

`Files::index_root` (`:380-410`) gains `frame: &Frame` after `root`; its list goes in the frame
and each key is made at its exact size:
```rust
        let mut files = Vec::with_capacity_in(root.len(), frame);
        // ... the loop as it is, pushing (file, content::path_key(path), self.entry_content(db, file))
        RootContent::new(root.is_library, files, old.as_ref(), frame)
```

`Files::set_source_root_with_durability` (`:600-630`): the body inside
`frame::task(Tag::Parse, |frame| { .. })`, with
`self.index_root(&mut writer, &*db, source_root_id, &source_root, frame)`,
`RootContent::refresh(&mut writer.root_contents, &[source_root_id], frame)` and
`writer.named.publish(db, frame)`.

`RootContent::new` (`:757-805`):
```rust
    fn new(
        is_library: bool,
        files: Vec<(FileId, Box<[u8]>, ContentId), &Frame>,
        old: Option<&RootContent>,
        frame: &Frame,
    ) -> RootContent {
        let mut buckets = Vec::with_capacity_in(files.len(), frame);
        let mut hash = Vec::new_in(frame);
        for (at, (file, key, _)) in files.iter().enumerate() {
            match old.and_then(|old| old.keys.get(file)) {
                Some((bucket, known)) if **known == **key => buckets.push(*bucket),
                _ => {
                    buckets.push(0);
                    hash.push(at);
                }
            }
        }
        let keys = frame.alloc_slice_from_iter(hash.iter().map(|&at| &*files[at].1));
        let hashes = content::hash_many(SourceRoot::PATH_DOMAIN, keys, frame);
        for (&at, path) in hash.iter().zip(hashes) {
            buckets[at] = RootContent::bucket(path);
        }
        let mut content = RootContent::empty(is_library);
        content.keys.reserve(files.len());
        for ((file, key, id), bucket) in files.into_iter().zip(buckets) {
            // clone: the bucket's copy of a key, per file of a root set (setup).
            content.buckets[bucket as usize].insert(key.clone(), id);
            content.keys.insert(file, (bucket, key));
        }
        // ... the `old` reuse as it is
```
(the key arrives boxed at its size, so the `into_boxed_slice` shrink goes; the frame `Vec`'s
`into_iter` moves each box out, and dropping the vector drops any left, so nothing leaks.)

`RootContent::bucket_bytes` (`:848-856`) takes `out: &mut Vec<u8, &Frame>`.

`RootContent::refresh` (`:860-916`):
```rust
    pub(crate) fn refresh(
        contents: &mut FxHashMap<SourceRootId, RootContent>,
        roots: &[SourceRootId],
        frame: &Frame,
    ) {
        let mut bytes = Vec::new_in(frame);
        let mut ranges: Vec<Range<usize>, &Frame> = Vec::new_in(frame);
        let mut places = Vec::new_in(frame);
        // ... first loop as it is
        if places.is_empty() {
            return;
        }
        let hashes = content::hash_ranges(SourceRoot::BUCKET_DOMAIN, &bytes, &ranges, frame);
        for (&(root, bucket), &hash) in places.iter().zip(hashes) {
            // ... as it is
        }
        bytes.clear();
        ranges.clear();
        let mut refreshed = Vec::new_in(frame);
        // ... second loop as it is
        let hashes = content::hash_ranges(SourceRoot::DOMAIN, &bytes, &ranges, frame);
        for (root, &hash) in refreshed.into_iter().zip(hashes) {
            // ... as it is
        }
    }
```
The four `// alloc:` comments on these buffers go: they are no longer allocations.

**`named.rs`.** `Named::publish(&mut self, db: &mut Db)` (`:138-188`) becomes
`publish(&mut self, db: &mut Db, frame: &Frame)` (`use blaze_mem::Frame;`), its keys hashed
from one frame buffer, `:142-153` replaced by:
```rust
        let table = &self.table;
        let mut bytes = Vec::new_in(frame);
        let mut ranges: Vec<std::ops::Range<usize>, &Frame> =
            Vec::with_capacity_in(self.pending.len(), frame);
        for &(_, at) in &self.pending {
            let start = bytes.len();
            content::path_key_into(&table.get(at).path, &mut bytes);
            ranges.push(start..bytes.len());
        }
        let ids = content::hash_ranges(SourceRoot::PATH_DOMAIN, &bytes, &ranges, frame);
```
and the loop below reads `for ((hash, _), id) in self.pending.iter().zip(ids)` as it is. Its
callers (`Files::set_source_root_with_durability`, `Files::write_view`) pass their frame. The 64
bucket inputs made once per database (`:166-173`) stay as they are (setup).

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:28): B1 in `content.rs`, `files.rs`, `named.rs`, adapted to the file as it is now: `content.rs` has no `frame()` helper and writes through `blaze_format::cursor::Put`, which is implemented only for `Vec<u8>`, so the byte buffers are the worker's (`scratch::lend_bytes`/`with_bytes`) instead of frame vecs, and `opt_bytes`/`content_bytes` stay `&mut Vec<u8>`. `path_key` returns `Box<[u8]>` via `scratch::with_bytes`; `path_key_into`, `hash_many` and `hash_ranges(.., frame) -> &'f [ContentId]` as written. Setters in `frame::task(Tag::Parse, ..)`; `RootContent::new`/`refresh`, `index_root`, `write_root_contents`, `Named::publish` take the frame.

### B2. `set_view_root` in its task (`view.rs`)

The owner's rewrite already keeps the view index without making a key per call
(`Writer::views` is looked up by `&AbsPath`, its key made once per base); what is left is the
plan's and the write's temporaries, the `built` list (made on every call, even one that writes
nothing, when `built` is not empty) and the sorts' scratch.

Imports (`:21-35`): `use std::{borrow::Cow, cell::RefCell, fmt};`,
`use blaze_mem::{Frame, Tag, frame, scratch::Scratch};`, `use rustc_hash::{FxBuildHasher, FxHashMap};`
(`FxHashSet` goes).

`Files::set_view_root` (`:221-250`): the body inside `frame::task(Tag::Parse, |frame| { .. })`,
with `self.plan_view(&writer, &mut index, &view, frame)` and
`Ok(self.write_view(&mut writer, db, &view, plan?, frame))`.

`Plan` (`:194-204`):
```rust
struct Plan<'f> {
    writes: Vec<Write, &'f Frame>,
    added: Vec<(FileId, SourceRootId, VfsPath), &'f Frame>,
    removed: Vec<(SourceRootId, FileId), &'f Frame>,
    next_file: u32,
}
```
(`added`'s `VfsPath`s are owned and dropped with the vector.)

`Files::plan_view` (`:253-343`) gains `frame: &'f Frame` and returns
`Result<Plan<'f>, ViewError>`:
```rust
        let call = index.begin();
        let mut fresh: hashbrown::HashSet<&str, FxBuildHasher, &Frame> =
            hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
        let mut plan = Plan {
            writes: Vec::new_in(frame),
            added: Vec::new_in(frame),
            removed: Vec::new_in(frame),
            next_file: writer.next_file,
        };
        // ... the loop as it is
        let bytes = frame.alloc_slice_from_iter(plan.writes.iter().map(|it| view.files[it.at].file.bytes));
        let ids = content::hash_many(blaze_vfs::FILE_DOMAIN, bytes, frame);
        for (write, &bytes) in plan.writes.iter().zip(ids) {
            // ... as it is
        }
        let mut built: Vec<Cow<'_, str>, &Frame> = Vec::with_capacity_in(view.built.len(), frame);
        built.extend(view.built.iter().filter_map(|built| match view.base.starts_with(built) {
            true => Some(Cow::Borrowed("")),
            false => Some(rel_key(built.strip_prefix(view.base)?.as_str())),
        }));
        // ... the `index.unseen(call)` loop as it is
```
(`rel_key` makes a `String` only on Windows for a path with `\`, dropped with the vector.)

`Files::write_view` (`:346-443`) takes `plan: Plan<'_>` and `frame: &Frame`. The lists it
returns are gathered in the frame and copied out once at their exact size (contract 4: a value
that leaves is exact, never grown by doubling):
```rust
        let texts = frame.alloc_slice_from_iter(
            plan.writes.iter().map(|it| std::str::from_utf8(view.files[it.at].file.bytes).ok()),
        );
        let mut bytes = Vec::new_in(frame);
        let mut ranges = Vec::new_in(frame);
        for (write, text) in plan.writes.iter().zip(&*texts) {
            // ... as it is
        }
        let mut contents = content::hash_ranges(FileText::DOMAIN, &bytes, &ranges, frame).iter().copied();
        let (mut set, mut added, mut removed, mut no_text) =
            (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));
        let mut changed_roots = Roots::new();
        for (write, &text) in plan.writes.iter().zip(&*texts) {
            // ... as it is, pushing to `no_text`, `added` and `set` in place of `change.*`
        }
        // ... the `set_file_source_root_in` loop as it is
        let mut roots: Vec<SourceRootId, &Frame> =
            Vec::with_capacity_in(plan.added.len() + plan.removed.len(), frame);
        roots.extend(plan.added.iter().map(|it| it.1).chain(plan.removed.iter().map(|it| it.0)));
        sort_roots(&mut roots);
        roots.dedup();
        self.write_root_files(writer, db, &plan, &roots, &mut changed_roots, frame);
        self.write_root_contents(writer, db, &changed_roots, frame);
        writer.named.publish(db, frame);
        for &(_, id) in &plan.removed {
            writer.view_ids.remove(id);
            removed.push(id);
        }
        // ... the `view_ids.insert` loop and `writer.next_file = plan.next_file;` as they are
        for list in [&mut set, &mut added, &mut removed, &mut no_text] {
            sort_files(list);
            list.dedup();
        }
        // alloc: the caller's lists, each at its size; none for an empty one.
        ViewChange {
            set: set.to_vec(),
            added: added.to_vec(),
            removed: removed.to_vec(),
            no_text: no_text.to_vec(),
            roots: roots.to_vec(),
        }
```
`radix::sort_copy_by_key` makes its scratch per call for two or more items; the sorts use the
worker's kept buffers through `sort_copy_by_key_in` (`crates/blaze-radix/src/lib.rs:196`,
reached as `blaze_ids::radix`), added beside `under`:
```rust
thread_local! {
    static ROOT_SORT: RefCell<Scratch<SourceRootId>> = const { RefCell::new(Scratch::new()) };
    static FILE_SORT: RefCell<Scratch<FileId>> = const { RefCell::new(Scratch::new()) };
}

fn sort_roots(roots: &mut [SourceRootId]) {
    ROOT_SORT.with_borrow_mut(|scratch| radix::sort_copy_by_key_in(roots, |it| it.0, scratch.take()));
}

fn sort_files(files: &mut [FileId]) {
    FILE_SORT.with_borrow_mut(|scratch| radix::sort_copy_by_key_in(files, |it| it.index(), scratch.take()));
}
```

`Files::write_root_files` (`:449-525`) takes `plan: &Plan<'_>` and `frame: &Frame` after
`changed`:
```rust
        let mut keys = Vec::with_capacity_in(plan.added.len(), frame);
        keys.extend(plan.added.iter().map(|(_, _, path)| content::path_key(path)));
        let key_bytes = frame.alloc_slice_from_iter(keys.iter().map(|key| &**key));
        let path_hashes = content::hash_many(SourceRoot::PATH_DOMAIN, key_bytes, frame);
        let mut keys: Vec<Option<(Box<[u8]>, u8)>, &Frame> = {
            let mut placed = Vec::with_capacity_in(keys.len(), frame);
            placed.extend(keys.into_iter().zip(path_hashes).map(|(key, hash)| Some((key, RootContent::bucket(hash)))));
            placed
        };
        for &root in roots {
            let input = self.source_root(root);
            let old = input.source_root(&*db);
            let mut gone: hashbrown::HashSet<FileId, FxBuildHasher, &Frame> =
                hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
            gone.extend(plan.removed.iter().filter(|it| it.0 == root).map(|it| it.1));
            // ... the rest as it is (`FileSet`, `SourceRoot::new_*`, `Arc::new` are the new
            // root's owned input)
```
(`key_bytes` borrows `keys` only until `path_hashes` is made, so the move after is fine.)

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:28): B2 on `view.rs` as it is now (texts from `blaze_vfs::text_of`, `ViewFile::bytes`): `set_view_root` in `frame::task(Tag::Parse, ..)`, `Plan<'f>` of frame vecs, `fresh`/`gone` frame `hashbrown::HashSet`s, `built`, texts, ranges, roots and the key lists in the frame, `ViewChange`'s lists copied out with `to_vec()`, sorts through `ROOT_SORT`/`FILE_SORT` and `radix::sort_copy_by_key_in`. Differs: `write_view`'s byte buffer is `scratch::lend_bytes()` (as in B1), and `plan_view`'s ID check hashes through `content::hash_many`.

### B3. `ContentId::of_many_into` (owner: lane `format`; filed by `mem-format`)

Landed (`crates/blaze-ids/src/lib.rs:80`, `of_many_into(domain, inputs, out: &mut Vec<ContentId>)`),
from `v2/requests/mem-format.md`, "`crates/blaze-ids/src/many.rs`: `of_many`'s per-call
buffers", a warm call into a buffer with room allocating
nothing, and its test `a_warm_of_many_into_allocates_nothing`. B1 and B2 call it as filed
(through `content::hash_many`); nothing more is asked here.

> forwarded to db (from requests, pass 17; told by tmux).

> checked by db-mem-def (21:40): `ContentId::of_many_into` is in `crates/blaze-ids` (format's); `content::hash_many` and `hash_ranges` call it.

### B4. base-db's tables on reserved regions (`table.rs`)

They are read on every query (lock-free, `docs/v2/plan/design/concurrency.md`) and grow only
when the file set or the notes grow; today each growth is a `Box` from the global allocator
(a 32 KiB page per 4096 IDs, a `Log` chunk, a shard table level). Each moves to a blaze-mem
region tagged `Tag::Query`, with the same read cost or less and no change to what a reader
sees. Imports: `use blaze_mem::{AppendVec, Tag};`.

- `table::Column<T>` (`:28-94`, with `new_page`): a salsa input per dense ID, read with two
  acquire loads through a page directory. After, one `blaze_mem::AppendVec<AtomicU64>` reserved
  for `MAX + 1` words (64 MiB of address space, committed as IDs are used):
  ```rust
  pub(crate) struct Column<T> {
      words: AppendVec<AtomicU64>,
      of: PhantomData<fn() -> T>,
  }

  impl<T: AsId + FromId + Copy> Column<T> {
      pub(crate) const MAX: u32 = span::EditionedFileId::MAX_FILE_ID;

      pub(crate) fn new() -> Column<T> {
          let words = AppendVec::new(Self::MAX as usize + 1, Tag::Query)
              .unwrap_or_else(|error| panic!("{error}"));
          Column { words, of: PhantomData }
      }

      #[inline]
      pub(crate) fn get(&self, raw: u32) -> Option<T> {
          match self.words.get(raw)?.load(Ordering::Acquire) {
              0 => None,
              bits => Some(T::from_id(Id::from_bits(bits))),
          }
      }

      /// Sets (or with `None` clears) the input of `raw`. Called only by the setters, with the
      /// files' writer lock held: the one writer `AppendVec` asks for. Panics naming it past
      /// [`Column::MAX`].
      pub(crate) fn set(&self, raw: u32, value: Option<T>) {
          assert!(raw <= Self::MAX, "ID {raw} is past {}, the largest file ID the front end takes", Self::MAX);
          let bits = value.map_or(0, |it| it.as_id().as_bits());
          if bits == 0 && raw >= self.words.len() {
              return;
          }
          while self.words.len() <= raw {
              self.words.append(AtomicU64::new(0));
          }
          self.words.as_slice()[raw as usize].store(bits, Ordering::Release);
      }

      pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, T)> + '_ {
          self.words.iter().enumerate().filter_map(|(raw, word)| match word.load(Ordering::Acquire) {
              0 => None,
              bits => Some((raw as u32, T::from_id(Id::from_bits(bits)))),
          })
      }
  }
  ```
  A read is one acquire load of the length, a bounds compare and the word's acquire load, with
  no directory. File IDs are dense from 0 (`Files::unused_file_id` hands out the lowest free),
  so the words appended below an ID are the ones a page would have held. Every `set` caller
  holds `Files::lock`: `write_text` and `set_file_source_root_in` take `&mut Writer`,
  `set_source_root_with_durability` locks; recheck after further edits, since `AppendVec`
  stops on a second writer at once.
- `table::Log<T>` (`:96-141`): exactly `blaze_mem::Column<T>` (any number of appenders, one
  `fetch_add` to claim, a ready bit to publish, `&T` stable). After:
  ```rust
  pub(crate) struct Log<T>(blaze_mem::Column<T>);

  impl<T> Log<T> {
      /// At most `max` values.
      pub(crate) fn new(max: usize) -> Log<T> {
          Log(blaze_mem::Column::new(max, Tag::Query).unwrap_or_else(|error| panic!("{error}")))
      }

      pub(crate) fn push(&self, value: T) -> u32 {
          self.0.push(value).unwrap_or_else(|error| panic!("{error}"))
      }

      #[inline]
      pub(crate) fn get(&self, raw: u32) -> Option<&T> {
          self.0.get(raw)
      }

      /// The values appended, in order; one still being appended is left out.
      pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
          self.0.iter().map(|(_, value)| value)
      }
  }
  ```
  `FIRST`, `CHUNKS` and `chunk_of` go.
- `table::Table<T>` (`:143-300`): `Table::new(max: usize)` passes `max` to its values' `Log`; a
  shard's levels (`tables: [OnceLock<Box<[AtomicU32]>>; TABLES]`) become
  `[OnceLock<AppendVec<AtomicU32>>; TABLES]`, each level its own exact reservation, filled with
  its zeros when made (growth only, with the shard's lock held):
  ```rust
  fn table(shard: &Shard, level: usize) -> &[AtomicU32] {
      shard.tables[level]
          .get_or_init(|| {
              let places = TABLE << level;
              let table = AppendVec::new(places, Tag::Query).unwrap_or_else(|error| panic!("{error}"));
              for _ in 0..places {
                  table.append(AtomicU32::new(0));
              }
              table
          })
          .as_slice()
  }
  ```
  `find` reads `shard.tables[level].get()?.as_slice()`. `Shard` itself
  (`Box::new(Shard { .. })`, once per shard on its first insert) stays a box: 64 bytes of locks
  and level, made once.
- `new_page`, `PAGE` and `PAGE_BITS` go.

`Files::default` (`files.rs:184-201`): `included: Table::new(1 << 22)` and the same for the other
five tables, `views: Log::new(1 << 16)`; `Named::default` (`named.rs:94-107`)
`Arc::new(Table::new(1 << 22))`. Four million includes, env reads or named paths is past any
build; past it, `MemError` names `Tag::Query`.

Address space per database: three columns at 64 MiB, seven tables at about 160 MiB each
(`(u64, T)` of at most 40 bytes times four million) plus their ready bits: about 1.3 GiB
reserved, nothing committed until used. Tests make many databases at once; at 47 bits of
address space that is room for 100 000 of them.

The test `a_table_finds_what_a_hash_map_does` (`tests.rs:422`) holds the new `Table` to the
same answers (its `Table::new()` becomes `Table::new(1 << 16)`); add one for `Column` (set and
clear random IDs below 100 000 with a `Vec` as the reference; `get` and `iter` agree) and one
for `Log` (four threads push 10 000 each; every index is read back to its value, `iter` has
40 000 in index order).

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:43), after db-expand-paths finished speed-hir's `Table` onto `intern::swiss::Index` and handed `table.rs` over: `Column` on `AppendVec<AtomicU64>` and `Log` on `blaze_mem::Column` as written; `Table::new(max)` passes `max` to its `Log`. The `Table` levels part is moot (a shard is now `{ lock, index: Index }`, whose growth is swiss's). `Files::default` sizes its six tables `1 << 22` and `views` `1 << 16`; `Named::default` `1 << 22`. Every `Column::set` caller holds the writer (`write_text`, `set_file_source_root_in` take `&mut Writer`; `set_source_root_with_durability` locks). Tests: `Table::new(1 << 16)` in both table tests, `Log::new(1 << 16)` in `a_log_and_a_column_give_back_what_was_put` (which already holds `Column` to a model), and a new `threads_pushing_to_a_log_each_get_their_own_index` (four threads, 10 000 each).

### B5. `run_all`: each task in a frame of its own, tagged by its caller (`db.rs`)

A batch task's body runs outside salsa's executor: `hir/src/check.rs:117` checks modules in it,
and the collector's tasks ask queries from it. So each task gets its own `frame::task`, which
also keeps a task the pump runs on a waiting thread out of the waiting caller's frame (the pump
nests tasks strictly, `blaze_graph::pump`, so frames stay a stack). The caller names the
subsystem. Imports (`:16`): add `use blaze_mem::{Tag, frame, scratch};`.

`Db::run_all` (`db.rs:278-297`):
```rust
    pub fn run_all<F: Fn(&Db, usize) + Sync>(&self, tag: Tag, count: usize, task: F) {
        if count == 0 {
            return;
        }
        let run = |db: &Db, at: usize| frame::task(tag, |_| task(db, at));
        match self.run {
            Run::Pool => run_on_pool(self, blaze_graph::exec::shared(), count, &run),
            Run::Here => match RUN_ORDER.get() {
                RunOrder::InOrder => (0..count).for_each(|at| run(self, at)),
                RunOrder::Reversed => (0..count).rev().for_each(|at| run(self, at)),
                RunOrder::Strided(stride) => {
                    let stride = stride.max(1);
                    (0..stride)
                        .flat_map(|first| (first..count).step_by(stride))
                        .for_each(|at| run(self, at));
                }
                RunOrder::LeftToReaders => {}
            },
        }
    }
```

`run_on_pool` (`db.rs:373-414`): `round` and `again` are the worker's `u32` buffers, so a batch
allocates nothing of its own (a job's box is blaze-graph's, `mem-core-plane`'s request on
`Job::erase`):
```rust
pub fn run_on_pool<F: Fn(&Db, usize) + Sync>(
    db: &Db,
    pool: &blaze_graph::pool::Pool,
    count: usize,
    task: &F,
) {
    let count = u32::try_from(count).expect("a batch of more than `u32::MAX` tasks");
    let near = blaze_graph::pool::group();
    let panicked = Mutex::new(None);
    let again = Mutex::new(scratch::lend_u32s());
    let mut round = scratch::lend_u32s();
    round.extend(0..count);
    while !round.is_empty() {
        pool.scope(|scope| {
            for &at in round.iter() {
                let (panicked, again) = (&panicked, &again);
                scope.submit(
                    near,
                    move || task(db, at as usize),
                    move |ran| {
                        // ... as it is, pushing `at` (a `u32`)
                    },
                );
            }
        });
        round.clear();
        std::mem::swap(&mut *round, &mut **again.lock().unwrap_or_else(|it| it.into_inner()));
        if panicked.lock().unwrap_or_else(|it| it.into_inner()).is_some() {
            break;
        }
    }
    if let Some(panic) = panicked.into_inner().unwrap_or_else(|it| it.into_inner()) {
        panic::resume_unwind(panic);
    }
}
```
(`Lent<Vec<u32>>` is `Send`, so the `Mutex` is `Sync` as before.) `run_on_pool`'s only other
callers are its own tests (`tests.rs:698`, `:713`), which pass a bare task: right, as they test
the pool.

Callers pass their tag (each a one-word edit before the count):
- `crates/hir-def/src/nameres/collector.rs:147` (`expand_all`): `db.run_all(Tag::Expand, calls.len(), ..)`;
  `:899` (the preludes' def maps): `self.db.run_all(Tag::Nameres, prelude.len(), ..)` (owner
  `db-expand-names-next1`).
- `crates/hir-def/src/nameres/facts/collect.rs:217` (preludes): `Tag::Nameres`; `:294`
  (expansions): `Tag::Expand` (owner `frontend-nameres-next1`).
- `crates/hir/src/check.rs:117` (`check_modules`): `Tag::Infer`, a check being the body checks
  hir-ty runs (owner `db-next2`).
- `crates/ide-db/src/symbol_index.rs:247`, `:255` (library and crate symbols): `Tag::Query`
  (owner per `docs/v2/plan/reference/crates.md`, lane db).
- `crates/base-db/src/db/tests.rs:44`, `:53`: `db.run_all(Tag::Query, ..)`.

Each caller imports `blaze_mem::Tag` (H1 gives the dependency; ide-db's manifest needs
`blaze-mem.workspace = true` if it lacks it).

> forwarded to db (from requests, pass 17; told by tmux).

> applied by frontend-db-basereq (10-29): B5 as written. `db.rs` imports
> `blaze_mem::{Tag, frame, scratch}`; `run_all(tag, count, task)` wraps each task in
> `frame::task(tag, ..)`; `run_on_pool` takes `round`/`again` from `scratch::lend_u32s()`
> (the two `// alloc:` comments went with the allocations). base-db's `Cargo.toml` gained
> `blaze-mem.workspace = true` (H1's line for base-db; `hashbrown` is left to B1, which uses
> it). Callers tagged as listed, each with `use blaze_mem::Tag;`: `hir-def` `nameres/collector.rs`
> (`expand_all` Expand, preludes Nameres), `nameres/facts/collect.rs` (preludes Nameres,
> expansions Expand), `hir` `check.rs` (Infer), `base-db` `db/tests.rs` (Query, both).
> `ide-db/src/symbol_index.rs` already passed `Tag::Query`. The `hir-def` and `hir` manifests
> still lack `blaze-mem` (H1, not mine): until H1 lands they don't build (hir-def's
> `item_scope.rs` already names `blaze_mem::scratch` without it too).

> applied by frontend-db-basereq (10-29): the `db/tests.rs` test
> `a_batch_run_here_allocates_nothing_and_frames_each_task` from "Budget tests" below.

> frontend-nameres (2026-10-29 21:21): seen and kept. `nameres/facts/collect.rs:218` passes `Tag::Nameres` (preludes) and `:295` `Tag::Expand` (expansions), with `use blaze_mem::Tag;` at `:11`; nothing left for `facts/`. hir-def's manifest now has `blaze-mem.workspace = true` (`crates/hir-def/Cargo.toml:52`), so the missing-dependency note above no longer holds for hir-def.

### B6. `transitive_deps` and `transitive_rev_deps` memoized (`input.rs:872-928`)

Both rebuild everything per call (`vec![self]`, an `FxHashSet`, a `Vec`; for reverse deps an
`FxHashMap<Crate, Vec<Crate>>` of the whole graph) and are asked from queries and from IDE and
build requests, 33 call sites across the tree. As tracked queries over the crate input their hit
is a memo read; a miss keeps its temporaries in the frame and its result at its exact size.
`impl Crate` (`:872-928`) becomes:

```rust
impl Crate {
    /// The crate and every crate it depends on, directly or not, in the order a walk from it
    /// first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_deps(self, db: &Db) -> &[Crate] {
        transitive_deps(db, self)
    }

    /// The crate and every crate that depends on it, directly or not, in the order a walk from
    /// it first reaches them.
    ///
    /// **Warning**: do not use this query in `hir-*` crates! It kills incrementality across crate metadata modifications.
    pub fn transitive_rev_deps(self, db: &Db) -> &[Crate] {
        transitive_rev_deps(db, self)
    }
}

#[salsa::tracked(returns(deref))]
fn transitive_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        let mut seen: hashbrown::HashSet<Crate, FxBuildHasher, &Frame> =
            hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame);
        let mut deps = Vec::new_in(frame);
        while let Some(krate) = worklist.pop() {
            if !seen.insert(krate) {
                continue;
            }
            deps.push(krate);
            worklist.extend(krate.data(db).dependencies.iter().map(|dep| dep.crate_id));
        }
        Box::from(&deps[..])
    })
}

#[salsa::tracked(returns(deref))]
fn transitive_rev_deps(db: &Db, krate: Crate) -> Box<[Crate]> {
    blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
        let all = all_crates(db);
        // The reverse edges as CSR: for each crate (by its place in `all`), the crates that
        // depend on it.
        let mut place: hashbrown::HashMap<Crate, u32, FxBuildHasher, &Frame> =
            hashbrown::HashMap::with_capacity_and_hasher_in(all.len(), FxBuildHasher, frame);
        place.extend(all.iter().enumerate().map(|(at, &krate)| (krate, at as u32)));
        let starts = frame.alloc_slice_fill_with(all.len() + 1, |_| 0u32);
        for &from in all.iter() {
            for dep in &from.data(db).dependencies {
                starts[place[&dep.crate_id] as usize + 1] += 1;
            }
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        let next = frame.alloc_slice_copy(&starts[..all.len()]);
        let edges = frame.alloc_slice_fill_with(starts[all.len()] as usize, |_| krate);
        for &from in all.iter() {
            for dep in &from.data(db).dependencies {
                let to = place[&dep.crate_id] as usize;
                edges[next[to] as usize] = from;
                next[to] += 1;
            }
        }
        let seen = frame.alloc_slice_fill_with(all.len(), |_| false);
        let mut order = Vec::new_in(frame);
        let mut worklist = Vec::new_in(frame);
        worklist.push(krate);
        seen[place[&krate] as usize] = true;
        while let Some(krate) = worklist.pop() {
            order.push(krate);
            let at = place[&krate] as usize;
            for &rev_dep in &edges[starts[at] as usize..starts[at + 1] as usize] {
                let slot = &mut seen[place[&rev_dep] as usize];
                if !*slot {
                    *slot = true;
                    worklist.push(rev_dep);
                }
            }
        }
        Box::from(&order[..])
    })
}
```
Imports (`:19`): `use rustc_hash::{FxBuildHasher, FxHashMap, FxHashSet};` stays (others use
`FxHashSet`), plus `use blaze_mem::Frame;`.

Order: `transitive_deps` keeps its order (the same walk). `transitive_rev_deps` was the
iteration order of an `FxHashSet`, and is now the order of a walk from the crate: deterministic
and independent of any table's layout. Every reader below sorts, collects into a set, counts or
tests membership, or extends an IDE list whose order was already the hash set's.

Callers, `&[Crate]` in place of `Vec`/`Box`. The rule: where the result is iterated by value,
`.into_iter()` becomes `.iter().copied()`, and a bare result in a `for` or `flat_map`/`extend`
gets `.iter().copied()`; `.len()` and indexing read as before.
- `crates/hir/src/lib.rs:308`, `:313`, `:4103` (`db-next2`).
- `crates/hir-ty/src/method_resolution.rs:1170`: `krate.transitive_deps(db).iter().copied().filter(..).collect()`.
- `crates/blaze-items/src/index.rs:525`: `krate.transitive_rev_deps(db)` returned as a slice
  (its function's return type `&[Crate]`, or `.into()` where a `Box` is wanted).
- `crates/blaze-engine/src/frozen.rs:256`; `outline.rs:530` (unchanged, `.len()`), `:861`, `:951`;
  `doctest.rs:1018`, `:1113`; `lib.rs:599`, `:2440`, `:2571` (`.to_vec()`), `:3547`, `:4300`,
  `:5032`, `:5298`, `:9376`, `:11097`, `:11109`.
- `crates/hir-ty-codegen/stable.rs:374`; `mod.rs:1620`, `:1690`, `:1698`, `:1713`, `:1752`;
  `settled.rs:69`; `patch.rs:732`, `:746`, `:1866`.
- `crates/blaze-diagnostics/src/lints/names/items/runtime_symbols.rs:63`;
  `lints/bodies/calls/methods/non_fmt_panic.rs:326`.

(`crates/project-model/src/workspace.rs:2074`, `:2077` call `CrateGraphBuilder::transitive_deps`,
a different function: unchanged.)

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:28; 21:40 moved to `frame::task(Tag::Query, ..)` as the revised text says): B6 as written in `input.rs` (`use blaze_mem::Frame;`), and hir's three callers (`lib.rs` `transitive_reverse_dependencies`, `notable_traits_in_deps`, the impls walk). The other crates' callers are filed in `v2/requests/db.md` under their owners.

### B7. `source_root_crates` at its exact size (`lib.rs:489-504`)

```rust
pub fn source_root_crates(db: &Db, id: SourceRootId) -> &[Crate] {
    #[salsa::tracked(returns(deref))]
    pub fn source_root_crates(db: &Db, root: SourceRootInput) -> Box<[Crate]> {
        let crates = AllCrates::get(db).crates(db);
        blaze_mem::frame::task(blaze_mem::Tag::Query, |frame| {
            let mut found = Vec::new_in(frame);
            found.extend(crates.iter().copied().filter(|&krate| {
                let root_file = krate.data(db).root_file_id;
                db.source_root(db.file_source_root(root_file).source_root_id(db)) == root
            }));
            Box::from(&found[..]) // alloc: the memo's value, made again only when the crate graph changes.
        })
    }
    source_root_crates(db, db.source_root(id))
}
```
`EditionedFileId::parse_errors` and `parse_errors_with_bodies` (`editioned_file_id.rs:43-61`)
turn `errors()`'s list into a box with `.into()`, which reallocates when the list has spare
capacity. mem-format has requested `Parse::errors_exact()` and `errors_with_bodies_exact()`
(`v2/requests/mem-format-tree.md`, end; each `Option<Box<[SyntaxError]>>`, `None` for no
errors). They are in `crates/syntax/src/lib.rs` (`:167`, `:172`; frontend-tree, 2026-10-30), so
this is ready to apply: the two bodies (`editioned_file_id.rs:45-51`, `:56-62`) become:
```rust
    pub fn parse_errors(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_exact()
    }

    pub fn parse_errors_with_bodies(self, db: &Db) -> Option<Box<[SyntaxError]>> {
        self.parse(db).errors_with_bodies_exact()
    }
```
(attributes and doc comments as they are): one exact allocation per file with errors, none
without.

> forwarded to db (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:28; 21:40 moved to `frame::task(Tag::Query, ..)` as the revised text says): `source_root_crates` as written. The `parse_errors` half waits on `Parse::errors_exact`/`errors_with_bodies_exact`, not in `crates/syntax` yet.

### B8. The executor path (no change; what was checked)

base-db's query functions are `EditionedFileId::parse`, `is_library`, `parse_errors`
(`editioned_file_id.rs:26-50`), `line_index` (`lib.rs:239-240`), `source_root_crates`'s inner
function (`lib.rs:491-492`) and, with B6, `transitive_deps` and `transitive_rev_deps`: all
`#[salsa::tracked]` over `&Db`. A call is the macro's `fn_ingredient(db).fetch(db, key)`
(`vendor/salsa/src/function/fetch.rs`): a hit returns the memo; a miss goes `fetch_cold`
(`fetch.rs:132`) → `self.execute` (`fetch.rs:197`; also `maybe_changed_after.rs:266`, a deep
verify that has to recompute) → `execute.rs:39` `execute` → `execute_maybe_iterate` (`:122`,
one call per fixpoint iteration) → `execute_query` (`:328`) → `C::execute(db, input)` (`:340`),
the one place a query function's body runs. `mem-db-salsa`'s M1 asks for it to be wrapped in
`frame::task(C::MEM_TAG, ..)`, but today `vendor/salsa` opens no frame (it uses blaze-mem only
for memo tables), and `frame::current` stops outside a task (`frame.rs:497`). So B6 and B7
open their own `frame::task(Tag::Query, ..)`; tasks nest (`frame.rs:453`), so they stay right
after M1 lands. Nothing in base-db calls a query body any other way: no inner
function is called directly (`source_root_crates`' inner tracked function is reached only
through the outer wrapper), and `Reparse::parse`/`reparse::whole` run only inside `parse`
(and `assert_whole` in debug builds and tests, inside it). The one place base-db runs code
outside the executor is `run_all`'s tasks, which B5 frames; the setters run outside any query
and B1, B2 frame them. No base-db query calls `frame::current`.

> forwarded to db (from requests, pass 17; told by tmux).

> checked by db-mem-def (21:40): no change asked, and it holds. B6 and B7 open `frame::task(Tag::Query, ..)`, and no base-db query calls `frame::current` (`db/tests.rs:62` calls it inside a `run_all` task, which B5 frames).
> checked by db-mem-def (22:45): M1 is in. `vendor/salsa/src/function/execute.rs:343` runs each query body in `blaze_mem::frame::task(C::MEM_TAG, ..)`, as do the cycle paths at 208, 267 and 278, so the "`vendor/salsa` opens no frame" above is out of date. B6's and B7's own `frame::task(Tag::Query, ..)` now nests inside salsa's: still right, one frame more per run. A `frame::current` in a query body no longer stops. Only a plain function that calls it from outside any task (a test) still does.

### Budget tests

**`crates/base-db/src/budget_tests.rs`** (`speed-hir-sol-basedb`'s, wired by
`frontend-db-base`'s block 8) already holds reads at 0 and an unchanged view at 0. With B1 and
B2 its `a_changed_file_costs_a_constant` (`:144-166`) tightens to what is left, a change's owned
values:
```rust
    // The call's one allocation, `ViewChange::set` at its size; its batches are in its frame.
    const PER_CALL: u64 = 1;
    // The file's `Arc<str>` and its reparse slot.
    const PER_CHANGED_FILE: u64 = 2;
```
and `an_unchanged_view_allocates_nothing` (`:130-141`) also sets its view with
`built: &[base.join("target")]` (the `built` list was the one allocation of an unchanged call
that has one): `set_view` there gains a `built: &[AbsPathBuf]` parameter, passed `&[]` by the
other test.

**The rest of B's budget tests, also in `budget_tests.rs`** (one set of budget tests for
base-db, on that file's helpers; nothing goes in `tests.rs`). A view edit needs no test of its
own: `a_changed_file_costs_a_constant` with one edited file holds it at
`PER_CALL + PER_CHANGED_FILE` = 3.

Imports (`:13-16`) gain `CrateBuilderId`, `CrateName`, `DependencyBuilder` and `FileText`.
`loaded`'s `graph.add_crate_root(..)` call (`:33-50`) moves into a helper it calls as
`lib_root(&mut graph, files[0]);`:
```rust
/// A library crate rooted at `root`, with no cfg, env or dependencies.
fn lib_root(graph: &mut CrateGraphBuilder, root: FileId) -> CrateBuilderId {
    graph.add_crate_root(
        root,
        Edition::Edition2021,
        None,
        None,
        Default::default(),
        None,
        Env::default(),
        CrateOrigin::Local { repo: None, name: None },
        Vec::new(),
        TargetKind::Lib { is_proc_macro: false },
        Arc::new(AbsPathBuf::assert_utf8(std::env::temp_dir())),
        Arc::new(CrateWorkspaceData {
            target: Err("no target in budget tests".into()),
            toolchain: None,
            const_eval: Default::default(),
        }),
    )
}

#[test]
fn a_keystroke_allocates_only_its_text_and_slot() {
    let (mut db, files) = loaded();
    for _ in 0..4 {
        db.set_file_text(files[1], "fn f() { 2 }\n");
        db.set_file_text(files[1], "fn f() { 1 }\n");
    }
    let content = db.source_root(SourceRootId(0)).content(&db);
    let ((), used) = blaze_alloc_count::measure(|| db.set_file_text(files[1], "fn f() { 2 }\n"));
    // The file's `Arc<str>` and its reparse slot; the setter's batches are in its frame.
    assert!(used.allocations <= 2, "a keystroke in a root of {FILES} files: {used:?}");
    assert_ne!(db.source_root(SourceRootId(0)).content(&db), content);
    assert_eq!(db.file_text(files[1]).content(&db), FileText::content_of("fn f() { 2 }\n", None));
}

#[test]
fn crate_deps_are_memo_hits() {
    let (mut db, files) = loaded();
    let mut graph = CrateGraphBuilder::default();
    let a = lib_root(&mut graph, files[0]);
    let c = lib_root(&mut graph, files[2]);
    graph.add_dep(a, DependencyBuilder::new(CrateName::new("c").unwrap(), c)).unwrap();
    let ids = graph.set_in_db(&mut db);
    let (a, c) = (ids[&a], ids[&c]);
    assert_eq!(a.transitive_deps(&db), &[a, c]);
    assert_eq!(c.transitive_rev_deps(&db), &[c, a]);
    let (_, used) = blaze_alloc_count::measure(|| {
        a.transitive_deps(&db).len() + c.transitive_rev_deps(&db).len()
    });
    assert_eq!(used.allocations, 0, "crate deps, warm: {used:?}");
}
```
The `Column` and `Log` reference tests of B4 are tests of `table.rs`, not budgets: they go
beside `a_table_finds_what_a_hash_map_does` (`tests.rs:422`).

**`crates/base-db/src/db/tests.rs`**, beside `run_all_of_nothing_runs_nothing`:
```rust
#[test]
fn a_batch_run_here_allocates_nothing_and_frames_each_task() {
    let db = Db::for_tests();
    let seen = std::sync::atomic::AtomicUsize::new(0);
    let task = |_: &Db, _: usize| {
        let tag = blaze_mem::frame::current(|frame| frame.tag());
        assert_eq!(tag, blaze_mem::Tag::Expand);
        seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
    db.run_all(blaze_mem::Tag::Expand, 8, task);
    blaze_alloc_count::assert_at_most(0, "a batch of 8 run on the calling thread", || {
        db.run_all(blaze_mem::Tag::Expand, 8, task)
    });
    assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 16);
}
```
The existing `a_memo_hit_allocates_nothing` (`db/tests.rs:79`) holds the parse-level memo hit.

> forwarded to db (from requests, pass 17; told by tmux).

> applied by frontend-db-basereq (10-29), the `budget_tests.rs` part and the `db/tests.rs`
> test: block 8 has landed. `PER_CALL` 64 -> 1, `PER_CHANGED_FILE` 6 -> 2; `set_view` takes
> `built: &[AbsPathBuf]` (`&[]` from `a_changed_file_costs_a_constant`), and
> `an_unchanged_view_allocates_nothing` sets the view once, then checks a repeat set both with
> `built: &[]` and `built: &[base.join("target")]` allocates nothing. These are the budgets
> after B1 and B2, which are not in base-db yet (no setter runs in a `frame::task`, and
> `set_view_root` still collects `built` and its batches on the heap): until both land,
> `a_changed_file_costs_a_constant` and the `built` case fail, by design, as the old
> unchanged-view test did before block 2. The `crates/base-db/src/tests.rs` tests (keystroke,
> view edit, crate deps) are not in my brief and stay for whoever applies B1, B2 and B6.

> applied by db-mem-def (21:28): `lib_root`, `a_keystroke_allocates_only_its_text_and_slot` and `crate_deps_are_memo_hits` in `budget_tests.rs` (where the text puts them; nothing in `tests.rs`). The view edit is `a_changed_file_costs_a_constant`, as the text says. B4's `Log` test is in `tests.rs` (see B4).

## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; its API is listed in `v2/requests/mem-backend-link.md` under
the same heading. The RNG is `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`).
`hir-def` already depends on `blaze-format`. In `crates/hir-def/src/nameres/frozen.rs`:

- `Enc::u32` becomes `pub fn u32(&mut self, it: u32) { blaze_format::leb::write_u64(&mut self.out, it.into()); }`.
- `Dec::u32` becomes
  ```rust
      pub fn u32(&mut self) -> R<u32> {
          blaze_format::leb::read_u32(self.bytes, &mut self.at).map_err(|error| error.to_string())
      }
  ```
  (it now also refuses a 5th byte carrying bits past 32, which the loop dropped).

> forwarded to mem-db-def (from requests, pass 17; told by tmux).
> applied by db-expand-names (owner of `frozen.rs`; checked by mem-db-def, 2026-10-29 21:16): nothing left to do. The cursor request
> below covers both bullets and db-expand-names applied it: `frozen.rs:575-576` `Enc::u32` is
> `self.out.put_uleb(it.into())` and `:2133-2134` `Dec::u32` is `Ok(self.r.uleb32()?)`
> (`blaze_format::leb::read_u32`, refusing a 5th byte past 32 bits); no `self.bytes`/`self.at`
> is left. The seeded-RNG half names no site in mem-db-def's crates.

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-front` (report `/tmp/blaze-lanes/v2-dedup-cursor-front.md`; the cursor is
`crates/blaze-format/src/cursor.rs`, API in `docs/v2/plan/lanes/dedup/cursor.md`). `hir-def`
already depends on `blaze-format`; no `Cargo.toml` change. This replaces the `Dec::u32` bullet of
the LEB128 request just above (that one reads `self.bytes`/`self.at`, which go away here); the
`Enc::u32` there and here write the same bytes, apply either.

`crates/hir-def/src/nameres/frozen.rs`:

1. Imports: add `use blaze_format::cursor::{Put, Reader};`.
2. `pub struct Dec<'a>`: the two fields
   ```rust
       bytes: &'a [u8],
       at: usize,
   ```
   become
   ```rust
       r: Reader<'a>,
   ```
   and in `Dec::new`, `bytes,` and `at: 0,` become `r: Reader::new(bytes),`.
3. `Dec::finish` body `match self.bytes.len() - self.at {` becomes `match self.r.left() {`
   (same arms, same words).
4. `fn decode`: 
   ```rust
       if dec.at != bytes.len() {
           return Err(format!("{} bytes left over", bytes.len() - dec.at));
       }
   ```
   becomes `dec.finish()?;` (the same message).
5. `impl Dec<'_>`:
   ```rust
       pub fn u8(&mut self) -> R<u8> {
           Ok(self.r.u8()?)
       }

       pub fn u32(&mut self) -> R<u32> {
           Ok(self.r.uleb32()?)
       }
   ```
   replace the two bodies (the `u8` that indexed `self.bytes`, and the LEB loop). `uleb32` is
   `blaze_format::leb::read_u32`: at most 5 bytes, and a 5th byte with bits past 32 is refused,
   where the loop dropped them (Enc never writes one, so no bytes that were read change meaning).
6. `Dec::str`:
   ```rust
           let bytes = self.bytes.get(self.at..self.at + len).ok_or("a string past the end")?;
           self.at += len;
   ```
   becomes `let bytes = self.r.take(len)?;` (and `self.at + len` can no longer overflow).
7. `Dec::files`, the hash of a file outside its crate:
   ```rust
                   true => {
                       let bytes =
                           self.bytes.get(self.at..self.at + 16).ok_or("a hash past the end")?;
                       self.at += 16;
                       Some(u128::from_le_bytes(bytes.try_into().expect("16 bytes")))
                   }
   ```
   becomes `true => Some(self.r.u128()?),`.
8. `Dec::len_and_room`: `Ok((n, n.min(self.bytes.len() - self.at)))` becomes
   `Ok((n, n.min(self.r.left())))`.
9. `Enc` (writer): `Enc::u32` becomes
   `pub fn u32(&mut self, it: u32) { self.out.put_uleb(it.into()); }` (the same bytes); in `Enc::finish` `self.out.extend_from_slice(&hash128(text.as_bytes()).to_le_bytes());`
   becomes `self.out.put_u128(hash128(text.as_bytes()));`; in `Enc::call` and in the context
   writer (`if self.fingerprinting { self.u8(1); let print = self.context_hash(ctx)?; ..`) the two
   `self.out.extend_from_slice(&print.to_le_bytes());` become `self.out.put_u128(print);`
   (`print` is the `u128` of `call_hash`/`context_hash`).

Errors stay `String` (`R`): `?` on a `cursor::Error` is its `From<cursor::Error> for String`
("at byte 12: 4 bytes wanted, 2 left"). No test matches the old words ("the bytes end early",
"a string past the end", "a hash past the end", "a number over u32"); "bytes left over" stays.
Same bytes accepted and refused, except the 5th-byte case of item 5.
> applied by db-expand-names (owner of `frozen.rs`): all nine edits; `Dec` reads through `blaze_format::cursor::Reader`, `Enc::u32` and the three `u128`s write through `Put`. This also covers the LEB128 request's `Enc::u32` and `Dec::u32` above. Same bytes written; a 5th LEB byte with bits past 32 is now refused.
> applied by its holder (checked: `nameres/frozen.rs` imports `blaze_format::cursor::{Put, Reader}` and `Dec` holds `r: Reader<'a>`) (requests, pass 18).
