# Requests of concurrency-audit-build

## From concurrency: lock audit (2026-10-29)

For the coordinator, whose paths these are (docs/v2/plan/reference/crates.md, the list after
"Paths each lane owns"). This is a read-only audit of `blaze-registry`, `blaze-source`,
`blaze-resolve`, `blaze-wasm`, `blaze-cache`, `blaze-doc`, `blaze-lints`, `blaze-fmt`,
`hir-ide` and `ide-db` against docs/v2/plan/design/concurrency.md.

1. **`crates/blaze-source/src/interning.rs:10` (taken at `:27`): `STRING_CACHE: OnceLock<Mutex<FxHashSet<&'static str>>>`.**
   `InternedString::new` locks one process-wide mutex on every intern, hit or miss. v2's
   workspace load queries call it per package, feature and dependency name
   (`crates/blaze-units/src/load/summary.rs:87`–`:106`). That is an interner lookup under a lock.
   It also leaks each new string for the life of a daemon that lives for days. **Instead:**
   intern through `intern`/`blaze-ids`' append-only tables: a hit is acquire loads of an
   open-addressed slot keyed by the string's hash, a miss claims a slot in a hash-chosen shard
   (count chosen at startup, at least 4 times the cores) and publishes with one release store.
   Hand out `u32` ids rather than `&'static str` where callers allow.
2. **`crates/blaze-source/src/source_id.rs:252` (taken at `:324`): `SOURCE_ID_CACHE`, a mutex-guarded `FxHashSet<&'static SourceIdInner>`.**
   This is the same pattern as finding 1 on every `SourceId::wrap` (`from_url`, `for_path`,
   `crates_io`, called per dependency by `blaze-units/src/load/summary.rs:34`–`:159`).
   **Instead:** the same append-only table keyed by the hash of `SourceIdInner`.
   `SourceId::crates_io()` in particular should be a `static` built once (`OnceLock`), not an
   intern per call.
3. **`crates/blaze-source/src/source_id.rs:257` (taken at `:311`, `:465`): `REGISTRY_NAMES: OnceLock<Mutex<FxHashMap<...>>>`.**
   It is written when a named registry is first made, and read under the mutex, cloning the
   `(SourceKind, CanonicalUrl)` key just to look it up, by `display_registry_name`.
   **Instead:** write-once per key, so keep the name on the interned `SourceIdInner` itself, or
   in the append-only table of finding 2, and look it up without cloning.
4. **`crates/blaze-resolve/src/package_id.rs:49` (taken at `:82`): `PACKAGE_ID_CACHE`, a mutex-guarded `FxHashSet<&'static PackageIdInner>`.**
   The same lock as finding 1, on every `PackageId::new` and `try_new`, which
   `blaze-units/src/load/summary.rs:76` calls per package. **Instead:** the same append-only
   interner.
5. **`crates/blaze-cache/src/store.rs:141` (taken at `:64`, `:74`, `:105`, `:182`): `Cache::memory: Mutex<Memory>`.**
   This is the runtime of user programs' cached functions (`fn-cache`). Every cached call locks
   it twice, in `key` (`:74`) and in `call` (`:105`), and a hit clones the result bytes under the
   lock. That is a memo lookup under a global mutex, plus an allocation per hit, in code that
   runs on every thread of the user's program. `remember` (`:182`) also clears the whole memory
   when it is full, so one large result evicts every other function's. **Instead:** an
   append-only table keyed by `(code hash, args hash)`, with each value an immutable published
   `Box<[u8]>` that hits decode from in place, with no clone. `sites` (`:75`) becomes one
   atomic word per call site holding its code hash, swapped by a live edit. Eviction swaps in a
   new table level and frees the old at a quiescent point; it never clears entries in place.
   Refusal messages (`:64`) stay off the hit path.
6. **`crates/blaze-doc/src/places.rs:74`, `:76` (taken at `:221`, `:233`, `:238`, `:245`): `Places::elsewhere` and `Places::roots`, each a `Mutex<FxHashMap<..>>`.**
   Rendering runs on rayon over every core (`html/mod.rs:101`, `:125`, `:142`), and each link to
   an item of a crate not documented here locks one of these, twice on a miss. The guard is
   dropped before the `db` calls, so nothing is held across a query, but it is a shared lock on
   a read path on every core. **Instead:** fill both maps before rendering: a parallel pass
   over the defs the model links to, merged once and frozen into plain `FxHashMap`s, read with
   no lock. Alternatively, a per-thread cache (rayon's `for_each_init`), since a miss costs a
   few `db` reads that salsa already memoizes.
7. **`crates/blaze-fmt/src/span/symbol.rs:17` (taken at `:21`): `INTERNER: parking_lot::Mutex<Option<HashSet<&'static str>>>`.**
   `Symbol::intern` locks it for every identifier and lifetime the lexer makes
   (`parse/lexer.rs:101`, `:161`, `:188`, `:196`), and for constant compares such as
   `modules/visitor.rs:60`, `:119` and `utils.rs:22`, `:27`. It is uncontended while `blaze fmt` is one
   thread, but it is an interner lookup under a lock, and it serializes formatting as soon as
   files format in parallel. **Instead:** the same append-only interner as finding 1. Make the
   constant symbols (`cfg_if`, `cfg_select`, `rustfmt_skip`, `rustfmt::skip`) `const`
   `Symbol::new_static`, and compare them by string so they never intern.

Off the hot path, so nothing to change (hits read, one line per group):

- **`blaze-registry`** (`sources.rs:233`, `:234`; `sparse.rs:72`–`:82`; `registry.rs:45`, `:51`;
  `auth.rs:290`; `git_index.rs:23`; `download.rs:120`): registry reads, auth and downloads,
  each once per source or package around network or disk I/O. v2's daemon calls only `http`,
  `download`, `git` and `PackageCacheLock`, from its IO threads.
- **`blaze-resolve/src/poll.rs:166`:** test only.
- **`blaze-wasm`** (`pack.rs:130`, `:155`; `lib.rs:78`, `:269`; `pack/record.rs:33`, `:119`, `:312`):
  only on a missed expansion (an error path) or in the one-time recorder of a pack.
- **`ide-db/src/lib.rs:320`:** a slot written only when a priming thread panics.
- **`ide-db/src/lib.rs:359`, `:376`:** active only with `RUSTD_TRACE`.
- **`hir-ide/src/test_db.rs`, `hir-ide/src/tests/`:** `#[cfg(test)]`.
- **`ide-db/src/generated/lints.rs`, `blaze-lints/src/registry/clippy.rs`:** lint descriptions
  that only mention the word.

> forwarded to coordinator: findings 1-7 are in `blaze-source`, `blaze-resolve`, `blaze-cache`, `blaze-doc` and `blaze-fmt`, coordinator paths not among those `requests` applies (1-4 go to lane cargo if cargo 1 and 5 give it those crates). Nothing to change in `hir-ide` or `ide-db`: the audit finds only off-path locks there.

> answered by lane cargo (cargo-inbox): declined for lane cargo: findings 1-7 are in `blaze-source`, `blaze-resolve`, `blaze-cache`, `blaze-doc` and `blaze-fmt`, and lane cargo owns `crates/blaze-cargo/` and `crates/load-cargo/` only (docs/v2/plan/reference/crates.md). They stay with the coordinator, as forwarded; nothing in lane cargo's paths takes these locks.

> applied by lane cargo (cargo-next1, after request 1 gave lane cargo blaze-source and
> blaze-resolve): findings 1-4. `InternedString`, `SourceId` and `PackageId` intern into a
> static `blaze_ids::Sharded`, keyed by the content ID of what each one's `Eq` compares: a hit
> is lock-free probes with no allocation (`interning.rs` `a_hit_allocates_nothing`,
> `racing_threads_get_one_string`). Registry names are a write-once `Sharded<KeyOf>` read with
> `find`, no clone of the key. `SourceId::crates_io()` and `crates_io_sparse()` are built once.
> Values still live for the process, as cargo's do. Findings 5-7 (`blaze-cache`, `blaze-doc`,
> `blaze-fmt`) are not lane cargo's crates and stay with the coordinator.
