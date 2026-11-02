# Requests of mem-codegen-engine (child of mem-codegen, lane mem)

What moves `crates/blaze-engine/` onto `crates/blaze-mem` (docs/v2/plan/design/memory.md). The
crate is lane `codegen`'s (not DONE), so every change is written here for its owner to apply.
Line numbers are of the tree of 2026-10-29, with codegen's uncommitted edits in it.

## Where the engine meets a codegen unit, and who opens the frame

The engine never drives a unit itself. `build` (`src/lib.rs:5232`) hands the whole program to
`CodegenUnit::compile_or_hold` (`lib.rs:5411`); the backend splits it into units and runs them on
every core through `SymbolResolver::in_parallel_beside` (`lib.rs:1429`). Each `work(&db,
&resolver, at)` call there is one worker's whole job loop (`hir-ty-codegen/units.rs:365`,
`everywhere`: "Runs `work`, which takes jobs until there are none"), so the loop over units, the
`CompiledFunction`s, and their symbol, relocation and section lists are all the backend's.

**The backend opens `blaze_mem::frame::task(Tag::Codegen, ..)` around each unit**, inside its job
loop; the engine opens none per unit. Agreed with mem-codegen-unit:
`v2/requests/mem-codegen-unit.md` section 4.

> Revised 2026-10-29 20:50, after pass 17 forwarded it: requests 3 and 5 and the "Depends on"
> section now follow the trait as mem-codegen-unit section 4.2 writes it. Answers are
> `intern::Symbol`s, `makers`/`copies` fill the caller's buffer, no `Frame` crosses the trait,
> `item` keeps `String`, and `closure` returns a `Symbol`. Apply this text, not the one
> forwarded first.

What the engine does do per unit is answer the backend's questions: a symbol for every function
called, a path for every type named, a file name for every panic location, a crate's ids. Today
each answer is worked out again or cloned out of a `Mutex<FxHashMap>`, an allocation per
question per unit. Requests 2 to 5 make a warm unit's questions allocate nothing and take no
lock. Requests 6 and 7 move the per-build lists into the request arena; 8 to 10 make the
no-op rebuild's walk allocate nothing.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

### Depends on: `SymbolResolver` answers without allocating (hir-ty-codegen, mem-codegen-unit)

A question asked again can only allocate nothing if its answer is not a fresh `String`.
Request 5 implements `SymbolResolver` (`crates/hir-ty-codegen/mod.rs:339`) as
`v2/requests/mem-codegen-unit.md` section 4.2 changes it. That request also changes the call
sites in hir-ty-codegen and blaze-wasm's two implementations. The two requests go in together.

```rust
    fn function(&self, func: FunctionId) -> Option<intern::Symbol>;
    fn static_(&self, id: StaticId) -> Option<intern::Symbol>;
    fn path(&self, path: &str) -> Option<&str>;
    fn source_path(&self, file: base_db::FileId) -> Option<intern::Symbol>;
    fn closure(&self, _owner: FunctionId, _expr: hir_def::hir::ExprId) -> Option<intern::Symbol>;
    fn makers(&self, _function: &str, _out: &mut Vec<u64>) {}
    fn copies(&self, _symbol: &str, _out: &mut Vec<(intern::Symbol, u64)>) {}
```

`item` keeps `-> Option<String>`. An `intern::Symbol` is `Copy`, four bytes, and its text is
`&'static str` (`crates/intern/src/symbol.rs:53`, `:190`); interning a string already interned
is a lookup. blaze-engine already depends on `intern` (`Cargo.toml:49`).

---

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 1. Depend on blaze-mem and blaze-alloc-count; the tests count allocations (required)

`crates/blaze-engine/Cargo.toml`, under `[dependencies]`, after `blaze-live = { path =
"../blaze-live" }` (line 100):

```toml
blaze-mem.workspace = true
```

and under `[dev-dependencies]` (line 111), first:

```toml
blaze-alloc-count.workspace = true
```

`crates/blaze-engine/src/lib.rs:4`, after the `#![allow(clippy::print_stderr, ..)]` line:

```rust
#![feature(allocator_api)]
```

`lib.rs:43-52`, before:

```rust
/// The daemon holds millions of small allocations and frees a file's worth of them on each
/// edit, which the system allocator is slow at. jemalloc gives the system back more of what
/// a build freed than mimalloc does. It is not built for Windows.
#[cfg(unix)]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(windows)]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

after:

```rust
/// The daemon holds millions of small allocations and frees a file's worth of them on each
/// edit, which the system allocator is slow at. jemalloc gives the system back more of what
/// a build freed than mimalloc does. It is not built for Windows.
#[cfg(all(unix, not(test)))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(all(windows, not(test)))]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The tests hold the hot paths to allocation budgets.
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

`lib.rs:15-41`, the module list: add `mod memo;` between `mod manifest;` and `mod mirdump;`.

- Why: requests 2 to 10 use `blaze_mem` (`Column`, `swiss::Index`, `Arena`, `Frame`,
  `scratch`); `Vec::new_in` and `Vec<T, &Frame>` need `allocator_api`; the budget tests need the
  counting allocator. The binaries still get jemalloc and mimalloc: `cfg(test)` is set only
  for the lib's own test build.
- Budget test: none of its own. After this change, run
  `PYTHONPATH=python/typed-input/src python3 tools/v2/manifest_lint.py '{"root": "."}'`.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 2. New `src/memo.rs`: per-build memos read without a lock (required)

A new file `crates/blaze-engine/src/memo.rs`, whole:

```rust
//! What the resolver works out once a build and the code generator asks again for every unit
//! (docs/v2/plan/design/memory.md, "tables"): a function's symbol, a static's, an item's path,
//! a file's name, a crate's ids. [`Memo::get`] takes no lock and allocates nothing. A first
//! answer is worked out outside any lock and published once; two threads that ask at once may
//! both work it out, and every reader sees the first one published, the same value, since what
//! it is worked out from does not change during the build.
//!
//! Cost of a hit: one FxHash of the key, one probe of a SwissTable group (a word of tags beside
//! its slots, one cache line), one acquire load of the value's ready word, one key compare.

use std::{
    hash::{BuildHasher, Hash},
    sync::Mutex,
};

use blaze_mem::{Column, Tag, swiss};

/// `V`s by `K`, each key filled once: the values in a [`Column`], which never moves, so a `&V`
/// lives as long as the memo; a [`swiss::Index`] over it that readers probe without a lock.
/// `insert` serializes the inserts; no read takes it.
pub(crate) struct Memo<K, V> {
    index: swiss::Index,
    values: Column<(K, V)>,
    insert: Mutex<()>,
}

impl<K: Copy + Eq + Hash, V> Memo<K, V> {
    /// A memo of at most `max` keys: address space for them reserved, nothing committed and
    /// nothing allocated until the first insert. Out of address space stops the build, naming
    /// the tag and sizes.
    pub(crate) fn new(max: usize) -> Memo<K, V> {
        let values = match Column::new(max, Tag::Codegen) {
            Ok(values) => values,
            Err(error) => panic!("{error}"),
        };
        Memo { index: swiss::Index::new(), values, insert: Mutex::new(()) }
    }

    fn hash(key: &K) -> u64 {
        rustc_hash::FxBuildHasher.hash_one(key)
    }

    /// The value published for `key`, if any.
    #[inline]
    pub(crate) fn get(&self, key: K) -> Option<&V> {
        let is = |at: u32| self.values.get(at).is_some_and(|it| it.0 == key);
        let at = self.index.find(Self::hash(&key), is)?;
        self.values.get(at).map(|it| &it.1)
    }

    /// The value for `key`, made by `make` if none is published yet. `make` runs with no lock
    /// held, so it may ask this memo of other keys.
    pub(crate) fn get_or_insert_with(&self, key: K, make: impl FnOnce() -> V) -> &V {
        if let Some(value) = self.get(key) {
            return value;
        }
        let value = make();
        let _held = self.insert.lock().unwrap_or_else(|it| it.into_inner());
        if let Some(value) = self.get(key) {
            return value;
        }
        // Every push is made with `insert` held, so the column's length is the index's.
        let len = self.values.len();
        let at = match self.values.push((key, value)) {
            Ok(at) => at,
            Err(error) => panic!("{error}"),
        };
        let hash_of = |at: u32| match self.values.get(at) {
            Some(it) => Self::hash(&it.0),
            None => unreachable!("an indexed value is published"),
        };
        self.index.insert(len, Self::hash(&key), at, hash_of);
        match self.values.get(at) {
            Some(it) => &it.1,
            None => unreachable!("the value just pushed is published"),
        }
    }

    /// How many keys have a value.
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::Memo;

    #[test]
    fn a_key_is_worked_out_once() {
        let memo: Memo<u32, u64> = Memo::new(1 << 10);
        let made = AtomicUsize::new(0);
        let make = |key: u32| {
            made.fetch_add(1, Ordering::Relaxed);
            u64::from(key) * 3
        };
        assert_eq!(memo.get(7), None);
        assert_eq!(*memo.get_or_insert_with(7, || make(7)), 21);
        assert_eq!(*memo.get_or_insert_with(7, || make(7)), 21);
        assert_eq!(*memo.get_or_insert_with(8, || make(8)), 24);
        assert_eq!(made.load(Ordering::Relaxed), 2);
        assert_eq!(memo.len(), 2);
        assert_eq!(memo.get(7), Some(&21));
    }

    /// Past the index's first levels, every key still finds its own value.
    #[test]
    fn a_memo_that_grew_finds_every_key() {
        let memo: Memo<u32, u32> = Memo::new(1 << 20);
        for key in 0..100_000 {
            memo.get_or_insert_with(key, || key ^ 0x5555);
        }
        for key in 0..100_000 {
            assert_eq!(memo.get(key), Some(&(key ^ 0x5555)));
        }
        assert_eq!(memo.get(100_000), None);
    }

    /// Threads that fill the same keys at once agree on one value each, and no key is in the
    /// column twice.
    #[test]
    fn threads_that_fill_one_key_see_one_value() {
        let memo: Memo<u32, u32> = Memo::new(1 << 16);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for key in 0..4096 {
                        assert_eq!(*memo.get_or_insert_with(key, || key + 1), key + 1);
                    }
                });
            }
        });
        assert_eq!(memo.len(), 4096);
    }

    /// A hit allocates nothing (docs/v2/plan/design/memory.md, "nothing allocated on a cache
    /// hit").
    #[test]
    fn a_hit_allocates_nothing() {
        let memo: Memo<u32, Option<Box<str>>> = Memo::new(1 << 12);
        for key in 0..1000 {
            memo.get_or_insert_with(key, || Some(format!("symbol{key}").into_boxed_str()));
        }
        blaze_alloc_count::assert_at_most(0, "a memo hit", || {
            for key in 0..1000 {
                assert!(memo.get(key).is_some());
                std::hint::black_box(memo.get_or_insert_with(key, || None));
            }
        });
    }
}
```

- Why: the resolver's answers live as long as one build and are read by every worker thread of
  it. That is an append-only table with lock-free readers (memory.md, "tables, interners and
  columns", `Column`), keyed through the shared SwissTable probe (`blaze_mem::swiss::Index`, the
  one every interner uses). Values in a `Column` never move, so a `&V` lives as long as the
  memo; the resolver's symbol memos hold `Copy` `intern::Symbol`s and hand them out by value.
  The `make` closures are where answers are first worked out, at most once per key per build
  in the common case; their allocations are the misses' and are justified there.
- Budget test: `memo::tests::a_hit_allocates_nothing` (0).

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 3. `Resolver` holds the memos instead of `named` (required)

`lib.rs`, a new struct after `struct Prebuilt { .. }` (after line 539):

```rust
/// What the code generator asks of the resolver again for every unit, each worked out once a
/// build ([`memo::Memo`]).
struct Memos {
    /// By whether the build had turned out to be one in full ([`Resolver::in_full`]) when the
    /// symbol was named: in full, what only the executable has is named no more.
    functions: memo::Memo<(FunctionId, bool), Option<intern::Symbol>>,
    privates: memo::Memo<FunctionId, bool>,
    statics: memo::Memo<StaticId, Option<intern::Symbol>>,
    sources: memo::Memo<vfs::FileId, Option<intern::Symbol>>,
    crates: memo::Memo<ide_db::base_db::Crate, CrateFacts>,
}

impl Memos {
    /// Address space for as many as a build of every crate of crates.io names: nothing is
    /// committed until it is filled.
    fn new() -> Memos {
        Memos {
            functions: memo::Memo::new(1 << 23),
            privates: memo::Memo::new(1 << 22),
            statics: memo::Memo::new(1 << 20),
            sources: memo::Memo::new(1 << 18),
            crates: memo::Memo::new(1 << 16),
        }
    }
}

/// What a build asks of a crate for every unit that names an item of it.
struct CrateFacts {
    /// rustc's `StableCrateId`, when rustc built something of it.
    rustc_id: Option<u64>,
    /// Blaze's id of a crate rustc built nothing of ([`Resolver::own_crate_id`]).
    own_id: Option<u64>,
    /// [`SymbolResolver::rustc_built`].
    rustc_built: bool,
    /// How the paths of symbols spell it ([`Resolver::crate_path`]).
    path: Option<Box<str>>,
}
```

`lib.rs:418-420` (in `struct Resolver`), before:

```rust
    /// The symbols of the functions asked about, for every thread of the build: the threads
    /// that compile find them ahead of the one that puts the functions in the object.
    named: Arc<std::sync::Mutex<FxHashMap<FunctionId, Option<String>>>>,
```

after:

```rust
    /// What the threads of the build ask again for every unit: the threads that compile find
    /// the symbols ahead of the one that puts the functions in the object.
    memos: Arc<Memos>,
```

`lib.rs:453` (in `struct ResolverParts`), before:

```rust
    named: Arc<std::sync::Mutex<FxHashMap<FunctionId, Option<String>>>>,
```

after:

```rust
    memos: Arc<Memos>,
```

`lib.rs:474` (in `Resolver::parts`), before: `            named: self.named.clone(),`
after: `            memos: self.memos.clone(),`

`lib.rs:500` and `lib.rs:519` (the destructuring and the constructor in `ResolverParts::over`),
before: `            named,` after: `            memos,` (both lines).

`lib.rs:858` (in `Resolver::with`), before:

```rust
            named: Default::default(),
```

after:

```rust
            // alloc: once a build, shared by its threads.
            memos: Arc::new(Memos::new()),
```

- Why: one `Arc` a build in place of one; the memos are shared the same way `named` was
  (`parts` clones the `Arc` once per worker per `in_parallel` call, not per unit).
- Budget test: request 5's.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 4. A crate's facts are worked out once a build (required)

`lib.rs:1120-1166`, before:

```rust
    /// rustc's `StableCrateId` of a crate it built something of. A crate of the checkout is
    /// one rustc built when a file of the link is its own: the standard library has private
    /// copies of crates (memchr, hashbrown) whose names a dependency at a path has too.
    fn rustc_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let ours = !krate.data(self.db).origin.is_lang();
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        let root = self.crate_root(krate)?;
        if ours && !self.table.links(name.crate_name().as_str(), &root) {
            return None;
        }
        self.table.stable_crate_id(name.crate_name().as_str(), &root)
    }

    /// How the paths of symbols spell a crate. One of the checkout that rustc built nothing
    /// of is spelled with its own id, so that no symbol of another crate of its name (the
    /// standard library's memchr) is taken for one of its items.
    fn crate_path(&self, krate: Crate) -> Option<String> {
        let name = krate.display_name(self.db)?;
        if self.table.is_empty() && krate.origin(self.db).is_lang() {
            return Some(name.crate_name().to_string());
        }
        match self.own_crate_id(krate.into()) {
            Some(own) => Some(format!("{}[{own:x}]", name.crate_name())),
            None => self.table.crate_path(name.crate_name().as_str(), &self.crate_root(krate)?),
        }
    }

    /// What stands for a crate of the checkout that rustc built nothing of, the same after
    /// every edit: its name and where in the checkout its source starts. One of the standard
    /// library, when the build has nothing of rustc's, by its name alone: a program has one
    /// of each name, wherever its source is.
    fn own_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let lang = krate.data(self.db).origin.is_lang();
        if lang && !self.table.is_empty() || self.rustc_crate_id(krate).is_some() {
            return None;
        }
        let name = Crate::from(krate).display_name(self.db)?;
        if lang {
            return Some(short_hash(&format!("blaze crate {}", name.crate_name())));
        }
        let root = self.crate_root(krate.into())?;
        let home = self.contents.cargo_home(self.db);
        let place = crate_place(&root, self.inside(&root), home.as_deref());
        let named = format!("blaze crate {} {place}", name.crate_name());
        Some(short_hash(&named))
    }
```

after:

```rust
    /// rustc's `StableCrateId` of a crate it built something of.
    fn rustc_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        self.crate_facts(krate).rustc_id
    }

    /// How the paths of symbols spell a crate.
    fn crate_path(&self, krate: Crate) -> Option<&str> {
        self.crate_facts(krate.into()).path.as_deref()
    }

    /// What stands for a crate of the checkout that rustc built nothing of.
    fn own_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        self.crate_facts(krate).own_id
    }

    fn crate_facts(&self, krate: ide_db::base_db::Crate) -> &CrateFacts {
        self.memos.crates.get_or_insert_with(krate, || self.crate_facts_of(krate))
    }

    /// Asks nothing of `memos.crates`: it is what fills it.
    fn crate_facts_of(&self, krate: ide_db::base_db::Crate) -> CrateFacts {
        let rustc_id = self.rustc_crate_id_of(krate);
        let own_id = self.own_crate_id_of(krate, rustc_id);
        let rustc_built = self.rustc_built_of(krate, own_id);
        // alloc: a miss, once per crate a build.
        let path = self.crate_path_of(krate.into(), own_id).map(String::into_boxed_str);
        CrateFacts { rustc_id, own_id, rustc_built, path }
    }

    /// rustc's `StableCrateId` of a crate it built something of. A crate of the checkout is
    /// one rustc built when a file of the link is its own: the standard library has private
    /// copies of crates (memchr, hashbrown) whose names a dependency at a path has too.
    fn rustc_crate_id_of(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let ours = !krate.data(self.db).origin.is_lang();
        let krate = Crate::from(krate);
        let name = krate.display_name(self.db)?;
        let root = self.crate_root(krate)?;
        if ours && !self.table.links(name.crate_name().as_str(), &root) {
            return None;
        }
        self.table.stable_crate_id(name.crate_name().as_str(), &root)
    }

    /// How the paths of symbols spell a crate. One of the checkout that rustc built nothing
    /// of is spelled with its own id, so that no symbol of another crate of its name (the
    /// standard library's memchr) is taken for one of its items.
    fn crate_path_of(&self, krate: Crate, own_id: Option<u64>) -> Option<String> {
        let name = krate.display_name(self.db)?;
        if self.table.is_empty() && krate.origin(self.db).is_lang() {
            return Some(name.crate_name().to_string());
        }
        match own_id {
            Some(own) => Some(format!("{}[{own:x}]", name.crate_name())),
            None => self.table.crate_path(name.crate_name().as_str(), &self.crate_root(krate)?),
        }
    }

    /// What stands for a crate of the checkout that rustc built nothing of, the same after
    /// every edit: its name and where in the checkout its source starts. One of the standard
    /// library, when the build has nothing of rustc's, by its name alone: a program has one
    /// of each name, wherever its source is.
    fn own_crate_id_of(&self, krate: ide_db::base_db::Crate, rustc_id: Option<u64>) -> Option<u64> {
        let lang = krate.data(self.db).origin.is_lang();
        if lang && !self.table.is_empty() || rustc_id.is_some() {
            return None;
        }
        let name = Crate::from(krate).display_name(self.db)?;
        if lang {
            return Some(short_hash(&format!("blaze crate {}", name.crate_name())));
        }
        let root = self.crate_root(krate.into())?;
        let home = self.contents.cargo_home(self.db);
        let place = crate_place(&root, self.inside(&root), home.as_deref());
        let named = format!("blaze crate {} {place}", name.crate_name());
        Some(short_hash(&named))
    }

    /// A crate rustc built may have no symbol that says which it is: one of generics alone,
    /// when nothing rustc built gives them their types. Its functions are still not ours to
    /// name.
    fn rustc_built_of(&self, krate: ide_db::base_db::Crate, own_id: Option<u64>) -> bool {
        // The standard library is rustc's prebuilt one, unless the build has nothing of rustc's,
        // where it is blaze's, compiled with the program or before it.
        if self.prebuilt(krate) {
            return true;
        }
        // What the prebuilt unit leaves out ([`library_unit`]), `proc_macro` and `test`, is
        // compiled with the program that links it.
        if krate.data(self.db).origin.is_lang() {
            return !self.table.is_empty();
        }
        if own_id.is_none() {
            return true;
        }
        let named = Crate::from(krate).display_name(self.db);
        let root = self.crate_root(krate.into());
        named
            .zip(root)
            .is_some_and(|(name, root)| self.table.links(name.crate_name().as_str(), &root))
    }
```

`lib.rs:1222` (in `scope_path`), before:

```rust
            let mut path = self.crate_path(module.krate(self.db))?;
```

after:

```rust
            // alloc: a miss: the path is built once, into the memo of what asked for it.
            let mut path = self.crate_path(module.krate(self.db))?.to_owned();
```

`lib.rs:1581-1608` (the trait's `rustc_built` and `stable_crate_id`), before:

```rust
    /// A crate rustc built may have no symbol that says which it is: one of generics alone,
    /// when nothing rustc built gives them their types. Its functions are still not ours to
    /// name.
    fn rustc_built(&self, krate: ide_db::base_db::Crate) -> bool {
        // The standard library is rustc's prebuilt one, unless the build has nothing of rustc's,
        // where it is blaze's, compiled with the program or before it.
        if self.prebuilt(krate) {
            return true;
        }
        // What the prebuilt unit leaves out ([`library_unit`]), `proc_macro` and `test`, is
        // compiled with the program that links it.
        if krate.data(self.db).origin.is_lang() {
            return !self.table.is_empty();
        }
        if self.own_crate_id(krate).is_none() {
            return true;
        }
        let named = Crate::from(krate).display_name(self.db);
        let root = self.crate_root(krate.into());
        named
            .zip(root)
            .is_some_and(|(name, root)| self.table.links(name.crate_name().as_str(), &root))
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        self.rustc_crate_id(krate).or_else(|| self.own_crate_id(krate))
    }
```

after:

```rust
    fn rustc_built(&self, krate: ide_db::base_db::Crate) -> bool {
        self.crate_facts(krate).rustc_built
    }

    fn stable_crate_id(&self, krate: ide_db::base_db::Crate) -> Option<u64> {
        let facts = self.crate_facts(krate);
        facts.rustc_id.or(facts.own_id)
    }
```

`lib.rs:1796` (in `adt_at`), before:

```rust
            .filter(|&it| self.crate_path(it).as_deref() == Some(krate))
```

after:

```rust
            .filter(|&it| self.crate_path(it) == Some(krate))
```

- Why: `rustc_built` and `stable_crate_id` are asked for every type id and every root of a
  unit (`hir-ty-codegen/type_id.rs:180-181`, `mod.rs:1393`, `:4055`, `:7815`, `stable.rs:524`),
  and every symbol named goes through `crate_path`. Today each call clones the crate's root
  path (`crate_root_file`), takes `SymbolTable::real`'s lock and clones a `PathBuf` out of it
  (`symbols.rs:708-712`), builds a `(String, PathBuf)` key (`symbols.rs:701`) and formats.
  Each fact depends only on the crate, the table, `prebuilt` and the checkout, none of which
  change during a build, so worked out once it is the same answer. The `_of` functions keep the
  old bodies and ask each other directly (never `crate_facts`), so filling the memo can't
  recurse into itself.
- Budget test: request 5's (`stable_crate_id`, `rustc_built`).

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 5. The resolver's answers to a unit come from the memos, as `Symbol`s (required, with mem-codegen-unit's section 4)

`lib.rs:1514-1521`, before:

```rust
    fn function(&self, func: FunctionId) -> Option<String> {
        if let Some(named) = self.named.lock().unwrap_or_else(|e| e.into_inner()).get(&func) {
            return named.clone();
        }
        let named = self.function_named(func);
        self.named.lock().unwrap_or_else(|e| e.into_inner()).insert(func, named.clone());
        named
    }

    fn private(&self, func: FunctionId) -> bool {
        !self.is_generic(func) && self.path(func).is_some_and(|path| self.table.private(&path))
    }
```

after:

```rust
    fn function(&self, func: FunctionId) -> Option<intern::Symbol> {
        let in_full = self.in_full.load(std::sync::atomic::Ordering::Relaxed);
        // alloc: a miss, the symbol named and interned once a build.
        let named = || self.function_named(func).map(|it| intern::Symbol::intern(&it));
        *self.memos.functions.get_or_insert_with((func, in_full), named)
    }

    fn private(&self, func: FunctionId) -> bool {
        // alloc: a miss: the path is spelled once a build.
        let private =
            || !self.is_generic(func) && self.path(func).is_some_and(|path| self.table.private(&path));
        *self.memos.privates.get_or_insert_with(func, private)
    }
```

`lib.rs:1541-1545`, before:

```rust
    fn in_full(&self) {
        self.in_full.store(true, std::sync::atomic::Ordering::Relaxed);
        // Some were named after what only the executable has.
        self.named.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
```

after:

```rust
    fn in_full(&self) {
        // Some were named after what only the executable has: from now on `function` asks
        // under `in_full`, where none of them is.
        self.in_full.store(true, std::sync::atomic::Ordering::Relaxed);
    }
```

`lib.rs:1574-1580`, before:

```rust
    fn makers(&self, function: &str) -> Vec<u64> {
        self.table.makers(function).collect()
    }

    fn copies(&self, symbol: &str) -> Vec<(String, u64)> {
        self.table.copies(symbol).map(|(copy, of)| (copy.to_owned(), of)).collect()
    }
```

after:

```rust
    fn makers(&self, function: &str, out: &mut Vec<u64>) {
        out.extend(self.table.makers(function));
    }

    fn copies(&self, symbol: &str, out: &mut Vec<(intern::Symbol, u64)>) {
        // Each copy was interned the first time it was asked for; after that, a lookup.
        out.extend(self.table.copies(symbol).map(|(copy, of)| (intern::Symbol::intern(copy), of)));
    }
```

`lib.rs:1547`, the signature of `closure`, before:

```rust
    fn closure(&self, owner: FunctionId, expr: hir_def::hir::ExprId) -> Option<String> {
```

after:

```rust
    fn closure(&self, owner: FunctionId, expr: hir_def::hir::ExprId) -> Option<intern::Symbol> {
```

and its last line, `lib.rs:1571`, before:

```rust
        self.table.get(&path).map(str::to_owned)
```

after:

```rust
        self.table.get(&path).map(intern::Symbol::intern)
```

(The body between them is unchanged, and so are its `format!`s: a closure is named once per
unit that names it. Each gets `// alloc: a closure's path, once per unit that names it.`)

`lib.rs:1490-1492`, before:

```rust
    fn source_path(&self, file: vfs::FileId) -> Option<String> {
        source_path(self.db, &self.root, self.base.as_deref(), file)
    }
```

after:

```rust
    fn source_path(&self, file: vfs::FileId) -> Option<intern::Symbol> {
        // alloc: a miss, a file named and interned once a build.
        let named = || {
            source_path(self.db, &self.root, self.base.as_deref(), file)
                .map(|it| intern::Symbol::intern(&it))
        };
        *self.memos.sources.get_or_insert_with(file, named)
    }
```

`lib.rs:1712` (the trait's `static_`): the method's whole body, lines 1712-1777, moves unchanged
into the inherent `impl Resolver<'_>` that starts at `lib.rs:888`. Move the body as it is when
this is applied: if frontend-tree has moved it onto `syntax::view` by then, keep that version.
It becomes:

```rust
    /// [`SymbolResolver::static_`], worked out.
    fn static_named(&self, id: StaticId) -> Option<String> {
        // (lines 1713-1776, unchanged)
    }
```

and the trait method becomes:

```rust
    fn static_(&self, id: StaticId) -> Option<intern::Symbol> {
        // alloc: a miss, a static named and interned once a build.
        let named = || self.static_named(id).map(|it| intern::Symbol::intern(&it));
        *self.memos.statics.get_or_insert_with(id, named)
    }
```

`lib.rs:1779-1781`, before:

```rust
    fn path(&self, path: &str) -> Option<String> {
        self.table.get(path).map(str::to_owned)
    }
```

after:

```rust
    fn path(&self, path: &str) -> Option<&str> {
        self.table.get(path)
    }
```

`item` (`lib.rs:1783`) is unchanged: the trait keeps it `-> Option<String>` (mem-codegen-unit
4.2: not on a unit's warm path). Its line gets `// alloc: an item's path, which the caller
extends into the symbol it spells.`

- Why: these are the questions a unit asks for every callee (`function`, 17 call sites in
  hir-ty-codegen, and `private`), every panic location (`source_path`, `later.rs:163`,
  `mod.rs:7185`, `stable.rs:244`), every static (`static_`), every path rustc named
  (`path`), and every generic instance's copies (`makers`, `copies`, `mod.rs:6699-6791`).
  Today `function` takes a `Mutex` twice on a miss and once on a hit, and clones a `String`
  on every call; the others work out and format their answer on every call. After this
  change a hit is a lock-free probe that returns a four-byte `Copy` symbol, and `path` is a
  borrow of the table. `makers` and `copies` fill the buffer the caller keeps
  (`spare::with_copies`, mem-codegen-unit 4.4). A copy is interned on its first call and only
  looked up after that.
- Semantics: unchanged. Every memoized answer is a function of the key, the build's
  `SymbolTable`, `prebuilt`, the checkout and the database revision, all fixed for the
  `Resolver`'s life (it is made per build, `lib.rs:5245`). `in_full` used to clear `named`;
  keying `functions` by the flag gives the same answers: a name worked out before the build
  turned out to be in full is never read after. `makers` and `copies` keep the table's order.
  Two threads that miss at once both run `function_named`, as they do today, and get the same
  answer.
- Budget test, in `lib.rs`'s `mod tests` (after `a_file_shared_by_targets_is_read_within_the_crates_given`,
  line 11757):

```rust
    /// The questions a unit asks of the resolver for each function it calls, each panic it
    /// can make and each crate, asked again by the same unit compiled again on the same
    /// thread, allocate nothing: the first answers are the build's.
    #[test]
    fn a_warm_unit_asks_the_resolver_without_allocating() {
        use test_fixture::WithFixture;
        let (db, file) = RootDatabase::with_single_file(
            r#"
pub struct Point { x: u32 }
pub fn callee(point: &Point) -> u32 { point.x }
pub fn caller() -> u32 { callee(&Point { x: 1 }) }
"#,
        );
        let args = ["/ws", "--crate", "ra_test_fixture", "--out", "/dev/null"].map(str::to_owned);
        let args = Args::parse(args.into_iter()).unwrap();
        let (table, contents, listed) =
            (SymbolTable::default(), content::Contents::default(), Listed::default());
        hir::attach_db(&db, || {
            let resolver = Resolver::of_rustcs(&db, &table, &args, &contents, &listed);
            let sema = hir::Semantics::new(&db);
            let file = file.file_id(&db);
            let module = sema.file_to_module_defs(file).next().unwrap();
            let krate = ide_db::base_db::Crate::from(module.krate(&db));
            let mut functions = Vec::new();
            for def in module.declarations(&db) {
                if let ModuleDef::Function(func) = def {
                    functions.extend(FunctionId::try_from(func).ok());
                }
            }
            assert_eq!(functions.len(), 2);
            // The buffers the backend keeps across units (`spare::with_copies`).
            let (mut makers, mut copies) = (Vec::with_capacity(8), Vec::with_capacity(8));
            let mut ask = |resolver: &Resolver<'_>| {
                for &func in &functions {
                    std::hint::black_box(resolver.function(func));
                    std::hint::black_box(resolver.private(func));
                }
                std::hint::black_box(SymbolResolver::source_path(resolver, file));
                std::hint::black_box(resolver.stable_crate_id(krate));
                std::hint::black_box(resolver.rustc_built(krate));
                makers.clear();
                resolver.makers("callee", &mut makers);
                copies.clear();
                resolver.copies("callee", &mut copies);
                std::hint::black_box(SymbolResolver::path(resolver, "ra_test_fixture::callee"));
            };
            ask(&resolver);
            blaze_alloc_count::assert_at_most(0, "a warm unit's questions of the resolver", || {
                ask(&resolver)
            });
            // What the memos answer is what working it out again says.
            for &func in &functions {
                let again = resolver.function_named(func).map(|it| intern::Symbol::intern(&it));
                assert_eq!(resolver.function(func), again);
            }
            // A build that turns out to be in full names every function again.
            let named = resolver.memos.functions.len();
            resolver.in_full();
            for &func in &functions {
                std::hint::black_box(resolver.function(func));
            }
            assert_eq!(resolver.memos.functions.len(), named + functions.len());
        });
    }
```

`SymbolResolver::path` and `SymbolResolver::source_path` are called by the trait's name because
`Resolver` has an inherent `path(FunctionId)` (`lib.rs:896`), which `resolver.path(..)` would
pick.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 6. `in_parallel_beside`: the worker handles in the caller's frame (required)

`lib.rs:1442-1464`, before:

```rust
        let stopped = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let (db, next, parts) = (self.db.clone(), &next, self.parts());
                    let counted = counted.clone();
                    scope.spawn(move || {
```

and the end of that `.map(..)` at lines 1462-1464:

```rust
                    })
                })
                .collect();
```

after (the whole statement down to the closing `});` of the scope at line 1483):

```rust
        let stopped = blaze_mem::frame::task(blaze_mem::Tag::Codegen, |frame| {
            std::thread::scope(|scope| {
                let mut workers = Vec::with_capacity_in(threads, frame);
                for _ in 0..threads {
                    let (db, next, parts) = (self.db.clone(), &next, self.parts());
                    let counted = counted.clone();
                    // alloc: a thread a core per batch of units, not per unit.
                    workers.push(scope.spawn(move || {
                        let _worker = trace::cold_span("worker", batch);
                        let resolver = parts.over(&db);
                        hir_ty_codegen::counted::within(counted, || {
                            hir::attach_db(&db, || {
                                loop {
                                    let at =
                                        next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    if at >= count {
                                        break;
                                    }
                                    work(&db, &resolver, at);
                                }
                            })
                        })
                    }));
                }
                {
                    let _caller = trace::cold_span("caller", batch);
                    beside();
                }
                // From when the caller is done to when the last thread is.
                join = trace::cold_span("join", batch);
                // dyn: a panic's payload, only when a worker panics.
                let mut stopped: Option<Box<dyn std::any::Any + Send>> = None;
                for worker in workers {
                    let Err(panic) = worker.join() else { continue };
                    let cancelled = panic.is::<ide_db::base_db::salsa::Cancelled>();
                    if stopped
                        .as_ref()
                        .is_none_or(|it| cancelled && !it.is::<ide_db::base_db::salsa::Cancelled>())
                    {
                        stopped = Some(panic);
                    }
                }
                stopped
            })
        });
```

(lines 1465-1483 move inside unchanged; only the indentation and the `workers` vector change.)

- Why: the handles live exactly as long as the scope: the caller's frame (memory.md, "a
  task"). `beside()` runs inside this task; a unit it compiles opens its own `frame::task` and
  gets another frame (`frame.rs:440`, "A task inside a task gets another frame"), so nothing of
  it lands in this one. On the caller thread this frame sits under the backend's five at most
  (mem-codegen-unit 4.1: job, `build`/`prepared`, `resolve`, `shape::value`), six in all. That
  is within the eight a thread keeps warm (`frame.rs:35`), so no frame is unmapped and
  reserved again. The thread spawns themselves allocate (std's `Packet` and the thread's
  stack): once per core per batch, not per unit. Taking them out means a persistent worker pool
  (blaze-core's plane at integration, docs/v2/plan/design/concurrency.md); that is a scheduling
  change, not a memory one, and is codegen's.
- Budget test: none of its own (a batch spawns threads). The per-unit budget is request 5's:
  what a worker does per unit, outside the backend's own frame, is answer those questions.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 7. `build`: what the build lists lives in the request arena (required)

`lib.rs:5246-5249`, before:

```rust
    let hir_db: &Db = db;
    let mut functions = Vec::new();
    let mut kept = Vec::new();
    let mut every_static: Vec<StaticId> = Vec::new();
```

after:

```rust
    let hir_db: &Db = db;
    // What the build lists to compile lives until it returns: the request's arena, its chunks
    // warm from the build before.
    let arena = blaze_mem::Arena::new(blaze_mem::Tag::Codegen);
    let bump = arena.local();
    let mut functions: Vec<(FunctionId, bool), _> = Vec::new_in(&bump);
    let mut kept: Vec<StaticId, _> = Vec::new_in(&bump);
    let mut every_static: Vec<StaticId, _> = Vec::new_in(&bump);
```

`lib.rs:5253-5258`, before:

```rust
    let mut local = Vec::new();
    let asked = args.crates().map(|it| crates_to_build(db, table, it, args));
    let asked: Vec<Vec<Crate>> = asked.collect::<anyhow::Result<_>>()?;
    // On every core: an edit to a crate others depend on has each of them listed again, and
    // the build waits for all of it.
    listed.of_all(db, &asked.iter().flatten().copied().collect::<Vec<_>>());
```

after:

```rust
    let mut local: Vec<Crate, _> = Vec::new_in(&bump);
    let asked = args.crates().map(|it| crates_to_build(db, table, it, args));
    // alloc: setup, a list per crate asked for, which `crates_to_build` returns.
    let asked: Vec<Vec<Crate>> = asked.collect::<anyhow::Result<_>>()?;
    // On every core: an edit to a crate others depend on has each of them listed again, and
    // the build waits for all of it.
    let mut all: Vec<Crate, _> = Vec::new_in(&bump);
    all.extend(asked.iter().flatten().copied());
    listed.of_all(db, &all);
```

`lib.rs:5293-5310`, the `if table.is_empty() {` block's sets: `asked`, `own`, `linked` stay
`FxHashSet`s (std's sets take no allocator; one entry per crate, once a build). Each `let`
gets `// alloc: setup, a set of the build's crates.` on its line. Line 5310, before:

```rust
        listed.of_all(db, &linked.iter().copied().collect::<Vec<_>>());
```

after:

```rust
        let mut all: Vec<Crate, _> = Vec::new_in(&bump);
        all.extend(linked.iter().copied());
        listed.of_all(db, &all);
```

After the `if table.is_empty() { .. }` block closes (after line 5321), add:

```rust
    // Read by every thread below: a slice of the arena, which `Sync` lets them share.
    let local: &[Crate] = bump.keep(local);
```

`lib.rs:5322-5342`, before:

```rust
    // A library built before the programs that use it has every static a generic function
    // of it uses, for the instances they compile.
    let offered: Vec<StaticId> = match main.is_none() && table.is_empty() {
        true => every_static
            .into_iter()
            .filter(|id| !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_)))
            .collect(),
        false => Vec::new(),
    };
    // ... (the comment at lines 5331-5334, unchanged)
    let list = start.elapsed();
    // With a program to start from, only what it reaches is compiled, and what a runtime
    // looks up by name. Without one any function may be the one called.
    let (entries, on_demand): (Vec<_>, Vec<_>) = functions
        .into_iter()
        .partition(|&(func, exported)| exported || main.is_none() || main == Some(func));
    let unzip = |it: Vec<(FunctionId, bool)>| it.into_iter().map(|it| it.0).collect::<Vec<_>>();
    let (entries, on_demand) = (unzip(entries), unzip(on_demand));
```

after:

```rust
    // A library built before the programs that use it has every static a generic function
    // of it uses, for the instances they compile.
    let mut offered: Vec<StaticId, _> = Vec::new_in(&bump);
    if main.is_none() && table.is_empty() {
        let defined =
            |id: &StaticId| !matches!(id.lookup(db).container, ItemContainerId::ExternBlockId(_));
        offered.extend(every_static.iter().copied().filter(defined));
    }
    // ... (the comment at lines 5331-5334, unchanged)
    let list = start.elapsed();
    // With a program to start from, only what it reaches is compiled, and what a runtime
    // looks up by name. Without one any function may be the one called.
    let mut entries: Vec<FunctionId, _> = Vec::new_in(&bump);
    let mut on_demand: Vec<FunctionId, _> = Vec::new_in(&bump);
    for (func, exported) in functions {
        if exported || main.is_none() || main == Some(func) {
            entries.push(func);
        } else {
            on_demand.push(func);
        }
    }
```

`lib.rs:5374-5402`, before:

```rust
    let stale: Option<Vec<_>> = std::thread::scope(|scope| {
        let parts: Vec<_> = (0..threads)
            .map(|part| {
                let (db, known, local, next) = (db.clone(), &*known, &local, &next);
                let parts = resolver.parts();
                let counted = Some(counting.counted().clone());
                scope.spawn(move || {
                    // (lines 5381-5397, unchanged)
                })
            })
            .collect();
        parts.into_iter().map(|it| it.join().ok().flatten()).collect()
    });
```

after:

```rust
    // alloc: what each core found stale, once a build, which `set_stale` keeps.
    let stale: Option<Vec<_>> = std::thread::scope(|scope| {
        // Joined on this thread alone, so the arena's local (not `Sync`) can hold them.
        let mut parts = Vec::with_capacity_in(threads, &bump);
        for part in 0..threads {
            let (db, known, next) = (db.clone(), &*known, &next);
            let parts_of = resolver.parts();
            let counted = Some(counting.counted().clone());
            parts.push(scope.spawn(move || {
                PROBING.set(true);
                let resolver = parts_of.over(&db);
                // (lines 5383-5397, unchanged)
            }));
        }
        parts.into_iter().map(|it| it.join().ok().flatten()).collect()
    });
```

(The spawned closure's body is the old one, with `parts` renamed `parts_of` on its first line,
since `parts` now names the vector; `local` is the `&[Crate]` frozen above, which the `move`
closure copies.)

`lib.rs:5494`, before: `        unset: unset_in(db, &local),` after: `        unset: unset_in(db, local),`

The rest of `build` is the edge (the result `Build` outlives the request: `unwritten`,
`reached`, `skipped`, `refused_exports`, the report and `--isolate` files) and stays, each
line justified: `lib.rs:5437` `// alloc: the edge: the files the build writes.`, `:5468` and
`:5481` `// alloc: the edge: what the build reports.`, `:5503`, `:5507`, `:5524`
`// alloc: the edge: the report and --isolate's files.`

- Why: `functions` grows by doubling to every function of every crate built (tens of thousands
  for a large program), `kept`, `every_static`, `offered`, `entries`, `on_demand`, `local` and
  the `of_all` lists with it, all of it freed when `build` returns: one request's memory
  (memory.md, "a request": `Arena`). The arena's chunks come back warm from the last build's
  (`arena.rs`, "the next request starts warm"), so a build that lists what the last one did
  calls the global allocator for none of it. `Roots` takes slices (`hir-ty-codegen/mod.rs:761`),
  which the arena's vectors give by deref; `local` is frozen into a `&[Crate]` (`keep`) before
  the threads read it, because an `ArenaLocal` is not `Sync`.
- Semantics: unchanged. The same elements in the same order (`partition` kept order; the
  loop does too), `every_static` read in order instead of moved.
- Budget test, in `lib.rs`'s `mod tests`:

```rust
    /// A build's lists in its arena come from the chunk cache: a second build of the same
    /// size takes nothing of the global allocator for them.
    #[test]
    fn a_build_lists_its_functions_in_its_arena() {
        let fill = || {
            let arena = blaze_mem::Arena::new(blaze_mem::Tag::Codegen);
            let bump = arena.local();
            let mut functions: Vec<(u32, bool), _> = Vec::new_in(&bump);
            for at in 0..50_000u32 {
                functions.push((at, at % 3 == 0));
            }
            let (mut entries, mut on_demand) = (Vec::new_in(&bump), Vec::new_in(&bump));
            for (func, exported) in functions {
                if exported { entries.push(func) } else { on_demand.push(func) }
            }
            let entries: &[u32] = bump.keep(entries);
            (entries.len(), on_demand.len())
        };
        assert_eq!(fill(), (16_667, 33_333));
        let lists = blaze_alloc_count::assert_at_most(0, "a build's lists", fill);
        assert_eq!(lists, (16_667, 33_333));
    }
```

  The whole of `build` is not measured: it compiles (the backend's budget) and writes files.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 8. The no-op rebuild's look at the files: one path buffer for all of them (required)

Depends on lane db adding to `crates/base-db/src/files.rs`, after `Files::on_disk` (line 443):

```rust
    /// [`Files::on_disk`] into `out`, emptied first: for a caller that asks it of every file on
    /// every look and keeps one buffer for it.
    pub fn on_disk_into(&self, path: &Path, out: &mut PathBuf) {
        out.as_mut_os_string().clear();
        let over = self
            .views_in_use()
            .iter()
            .filter(|it| !it.built.iter().any(|built| path.starts_with(built)))
            .find_map(|it| Some((&it.over, path.strip_prefix(&it.base).ok()?)));
        match over {
            Some((over, rest)) => {
                out.push(over);
                out.push(rest);
            }
            None => out.push(path),
        }
    }

    /// The files outside the VFS that `include_bytes!` or `include_str!` named, in no order.
    pub fn included_paths(&self) -> impl Iterator<Item = &AbsPath> + '_ {
        self.included.iter().map(|it| it.path.as_path())
    }
```

(`over.join(rest)` is `over` pushed with `rest`; pushing onto an empty buffer gives what was
pushed: the same path `on_disk` returns.) The engine reaches them as `db.files()`
(`base-db/src/db.rs:125`; `RootDatabase` is `Db`, `ide-db/src/lib.rs:85`).

`lib.rs`: the scan at lines 10271-10316 moves into a function. A new free function after
`fn written(..)` (line 10888):

```rust
/// One look at every watched file: the index of each that changed goes in `touched`, each
/// that is gone in `gone`, and when the first change was noticed in `noticed`. Files under
/// `switched` are compared by their text. A look that finds nothing changed allocates nothing.
fn look_at_files(
    files: &mut [WatchedFile],
    db: &RootDatabase,
    switched: &[PathBuf],
    sent: &FxHashMap<vfs::FileId, (String, Instant)>,
    touched: &mut FxHashSet<usize>,
    gone: &mut FxHashSet<usize>,
    noticed: &mut Option<(Instant, Duration)>,
) {
    let mut lent = blaze_mem::scratch::lend_path();
    for (index, file) in files.iter_mut().enumerate() {
        db.files().on_disk_into(&file.path, &mut lent);
        let on_disk: &Path = &lent;
        let Ok(now) = std::fs::metadata(on_disk).and_then(|m| m.modified()) else {
            let changed = match file.file {
                Watched::Source(_) => {
                    touched.remove(&index);
                    gone.insert(index)
                }
                // Still watched, so the input put back invalidates the macro again.
                Watched::Included(_) => file.mtime.take().is_some() && touched.insert(index),
            };
            if changed {
                noticed.get_or_insert((Instant::now(), Duration::ZERO));
            }
            continue;
        };
        gone.remove(&index);
        if switched.iter().any(|base| file.path.starts_with(base)) {
            file.mtime = Some(now);
            let same = match file.file {
                Watched::Source(id) => source_on_disk(on_disk)
                    .is_ok_and(|text| *text == **db.file_text(id).text(db)),
                Watched::Included(ref path) => db.included_is_read(path),
            };
            if !same {
                touched.insert(index);
                noticed.get_or_insert((Instant::now(), Duration::ZERO));
            }
        } else if Some(now) != file.mtime {
            let was = file.mtime.replace(now);
            // An editor that sent its text writes it after: the same text again.
            let built = match file.file {
                Watched::Source(id) if was.is_some() => {
                    !sent.contains_key(&id)
                        && source_on_disk(on_disk)
                            .is_ok_and(|text| *text == **db.file_text(id).text(db))
                }
                _ => false,
            };
            if !built {
                touched.insert(index);
                noticed.get_or_insert((Instant::now(), now.elapsed().unwrap_or_default()));
            }
        }
    }
}
```

and `lib.rs:10271-10316` (from `for (index, file) in files.iter_mut().enumerate() {` to the
`}` that closes it, before `switched.clear();`), become:

```rust
            look_at_files(
                &mut files,
                &db,
                &switched,
                &sent,
                &mut touched,
                &mut gone,
                &mut noticed,
            );
```

- Why: on a no-op rebuild every watched file is looked at once a pass, and today each look
  allocates a `PathBuf` (`Files::on_disk` returns a fresh one: `base-db/src/files.rs:442`). One
  lent path buffer serves the whole pass (memory.md, "per-call scratch"). `std::fs::metadata`
  puts a path shorter than 384 bytes in a stack buffer (std's `run_path_with_cstr`), so the look
  at an unchanged file allocates nothing. Reading a file that changed is the edge.
- Semantics: unchanged; the body is the loop's, with `&db` passed in and `on_disk` borrowed
  from the buffer. `db.file_text(id).text(db)` takes the `&RootDatabase` it took as `&db`.
- Budget test, in `lib.rs`'s `mod tests`:

```rust
    /// A look at files that changed nothing allocates nothing: what a no-op rebuild does
    /// before it answers that nothing changed.
    #[test]
    fn a_look_at_unchanged_files_allocates_nothing() {
        use test_fixture::WithFixture;
        let dir = std::env::temp_dir().join(format!("blaze-look-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut files: Vec<WatchedFile> = (0..64)
            .map(|at| {
                let path = dir.join(format!("{at}.rs"));
                std::fs::write(&path, "fn f() {}\n").unwrap();
                let mtime = std::fs::metadata(&path).and_then(|it| it.modified()).ok();
                let included = Watched::Included(AbsPathBuf::assert_utf8(path.clone()));
                WatchedFile { file: included, path, mtime }
            })
            .collect();
        let (db, _) = RootDatabase::with_single_file("");
        let sent = FxHashMap::default();
        let (mut touched, mut gone, mut noticed) =
            (FxHashSet::default(), FxHashSet::default(), None);
        let mut look = || {
            look_at_files(&mut files, &db, &[], &sent, &mut touched, &mut gone, &mut noticed)
        };
        look();
        blaze_alloc_count::assert_at_most(0, "a look at unchanged files", &mut look);
        assert!(touched.is_empty() && gone.is_empty() && noticed.is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
```

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 9. `WatchedFile::unwatched` and `immutable`: no set and no list per request (required)

`lib.rs:6757-6780`, before:

```rust
    fn immutable(path: &Path) -> bool {
        let names: Vec<&str> = path.components().filter_map(|c| c.as_os_str().to_str()).collect();
        let unedited = lock(&UNEDITED);
        path.starts_with("/nix/store")
            || cargo::toolchain::library::has(path)
            || unedited.installed.iter().any(|it| path.starts_with(it))
            || WatchedFile::fetched(&names)
            || unedited.targets.iter().any(|it| path.starts_with(it))
                && !cargo::script::in_out_dir(path)
    }

    /// Whether a file is in what cargo fetched: `registry/src/<index>-<hash>/` or
    /// `git/checkouts/<repository>-<hash>/`. A directory of the checkout that is only named
    /// `registry` is edited like any other (cargo's own `src/cargo/sources/registry`).
    fn fetched(names: &[&str]) -> bool {
        let hashed = |name: &str| {
            name.rsplit_once('-').is_some_and(|(_, hash)| {
                hash.len() == 16 && hash.bytes().all(|it| it.is_ascii_hexdigit())
            })
        };
        names.windows(3).any(|it| {
            matches!((it[0], it[1]), ("registry", "src") | ("git", "checkouts")) && hashed(it[2])
        })
    }
```

after:

```rust
    fn immutable(path: &Path) -> bool {
        let unedited = lock(&UNEDITED);
        path.starts_with("/nix/store")
            || cargo::toolchain::library::has(path)
            || unedited.installed.iter().any(|it| path.starts_with(it))
            || WatchedFile::fetched(path)
            || unedited.targets.iter().any(|it| path.starts_with(it))
                && !cargo::script::in_out_dir(path)
    }

    /// Whether a file is in what cargo fetched: `registry/src/<index>-<hash>/` or
    /// `git/checkouts/<repository>-<hash>/`. A directory of the checkout that is only named
    /// `registry` is edited like any other (cargo's own `src/cargo/sources/registry`).
    fn fetched(path: &Path) -> bool {
        let hashed = |name: &str| {
            name.rsplit_once('-').is_some_and(|(_, hash)| {
                hash.len() == 16 && hash.bytes().all(|it| it.is_ascii_hexdigit())
            })
        };
        let mut names = path.components().filter_map(|c| c.as_os_str().to_str());
        let (mut first, mut second) = (names.next(), names.next());
        for third in names {
            let above = matches!((first, second), (Some("registry"), Some("src")) | (Some("git"), Some("checkouts")));
            if above && hashed(third) {
                return true;
            }
            (first, second) = (second, Some(third));
        }
        false
    }
```

`lib.rs:6850-6856`, before:

```rust
    fn unwatched(files: &[WatchedFile], db: &RootDatabase) -> bool {
        let watched: FxHashSet<&Path> = files.iter().map(|it| &*it.path).collect();
        db.included_files()
            .iter()
            .map(AsRef::<Path>::as_ref)
            .any(|it| !watched.contains(it) && !WatchedFile::immutable(it))
    }
```

after:

```rust
    fn unwatched(files: &[WatchedFile], db: &RootDatabase) -> bool {
        // The watched files by their path's hash, high half, and their index, low half: sorted,
        // a path's candidates are one range of them.
        let key = |path: &Path| rustc_hash::FxBuildHasher.hash_one(path) >> 32;
        blaze_mem::scratch::with_u64s(|watched| {
            watched.extend(files.iter().enumerate().map(|(at, it)| key(&it.path) << 32 | at as u64));
            watched.sort_unstable();
            db.files().included_paths().map(AsRef::<Path>::as_ref).any(|it| {
                let hash = key(it);
                let from = watched.partition_point(|&at| at >> 32 < hash);
                let mut alike = watched[from..].iter().take_while(|&&at| at >> 32 == hash);
                !alike.any(|&at| *files[at as u32 as usize].path == *it) && !WatchedFile::immutable(it)
            })
        })
    }
```

with `use std::hash::BuildHasher as _;` at the top of the function (or `std::hash::BuildHasher`
added to the `use std::{..}` at `lib.rs:56`).

- Why: `unwatched` runs on every request for a project (`lib.rs:9844`) and on every look
  (`:9675`). Today it builds a set of every watched path and clones the sorted list of every
  included file (`base-db/src/files.rs:560-565`). The sorted `u64` keys are a radix-friendly
  replacement for the set (common.md, "precomputed `u64` order keys"), in the thread's
  scratch, warm after the first request. `immutable` built a `Vec` of the path's names for each
  call; `fetched` now walks the names three at a time, the same windows over the same
  filtered names.
- Semantics: unchanged. `any` over the included paths gives the same answer in any order. A
  hash match is confirmed by comparing the paths, so a collision can't make a file look watched.
- Budget test, in `lib.rs`'s `mod tests`, beside `a_directory_of_the_checkout_named_registry_is_watched`:

```rust
    /// Whether a build included a file nobody watches is asked on every request: once warm, it
    /// allocates nothing, and says what the set it replaces said.
    #[test]
    fn asking_for_unwatched_files_allocates_nothing() {
        use test_fixture::WithFixture;
        let (db, _) = RootDatabase::with_single_file("");
        let files: Vec<WatchedFile> = (0..256)
            .map(|at| {
                let path = PathBuf::from(format!("/ws/src/{at}.rs"));
                let included = Watched::Included(AbsPathBuf::assert_utf8(path.clone()));
                WatchedFile { file: included, path, mtime: None }
            })
            .collect();
        assert!(!WatchedFile::unwatched(&files, &db));
        let unwatched = blaze_alloc_count::assert_at_most(0, "asking for unwatched files", || {
            WatchedFile::unwatched(&files, &db)
        });
        assert!(!unwatched);
        let fetched = Path::new("/h/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/a/lib.rs");
        assert!(blaze_alloc_count::assert_at_most(0, "an immutable path", || {
            WatchedFile::fetched(fetched)
        }));
        assert!(!WatchedFile::fetched(Path::new("/ws/crates/registry/src/lib.rs")));
    }
```

  `a_directory_of_the_checkout_named_registry_is_watched` (`lib.rs:11598`) keeps checking what
  `immutable` says.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## 10. `Settled::still`: the files outside the checkout looked at through one buffer (required)

`lib.rs:10849-10860`, before:

```rust
    fn others(
        files: &[WatchedFile],
        base: &Path,
        written: impl Fn(&WatchedFile) -> Option<SystemTime>,
    ) -> u64 {
```

after:

```rust
    fn others(
        files: &[WatchedFile],
        base: &Path,
        mut written: impl FnMut(&WatchedFile) -> Option<SystemTime>,
    ) -> u64 {
```

`lib.rs:10873-10874`, before:

```rust
        // A file outside every checkout is read from where it is.
        let outside = |file: &WatchedFile| written(&db.on_disk(&file.path));
```

after:

```rust
        // A file outside every checkout is read from where it is.
        let mut lent = blaze_mem::scratch::lend_path();
        let outside = |file: &WatchedFile| {
            db.files().on_disk_into(&file.path, &mut lent);
            written(&lent)
        };
```

- Why: `still` is what answers `ok unchanged` (`lib.rs:9869`), the no-op rebuild itself; it
  looked up every file outside the checkout with a fresh `PathBuf` each. The other caller of
  `others` (`lib.rs:10845`, `|file| file.mtime`) is unchanged by the `FnMut` bound.
- Budget test: `a_look_at_unchanged_files_allocates_nothing` holds `on_disk_into` and
  `metadata` to zero; `still` adds a hash over the same files.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## Outside the engine, for the no-op walk (for lane mem to route)

- `crates/blaze-cargo/src/tree.rs:86` and `:96` (lane cargo): `Tree::relisted` and
  `Tree::added` call `db.on_disk(path)` for every directory on every look. With request 8's
  `Files::on_disk_into` they take one `blaze_mem::scratch::lend_path()` for the pass:
  `let mut lent = scratch::lend_path(); ... db.files().on_disk_into(path, &mut lent);
  std::fs::metadata(&*lent)`.
- `crates/base-db/src/files.rs` (lane db): `on_disk_into` and `included_paths`, as in request 8.

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).

## What stays, and why

- The bodies of `Resolver::closure` (`lib.rs:1547`; only its return type changes, request 5),
  `item`, `declared_in`, `impl_place` and `adt_at`: each is asked once per closure, per item
  spelled, per type declared in a body or per `impl`. Their answers are built once per unit
  that names them, and the backend keeps them. Each formatting line gets `// alloc: a
  closure's or item's name, once per unit that names it.`
- `function_named`, `path(FunctionId)`, `scope_path`, `type_path`, `static_named`,
  `own_static_name`, `function_named_in`: they now run only inside the memos' `make` closures,
  once per key per build. Their `format!`s, `Vec`s and `join`s are the misses'.
- `content_id` (`lib.rs:1610`): `Contents` keeps its own answers (`content.rs`); `crate_root` is
  cloned there per call for a library crate. That is the store's identity of a crate, not a
  name, and is left to codegen with the store lane.
- Everything outside `build`, the resolver and the watch walk (loading, `Checkouts`, linking,
  splicing, the test harness, doc tests, the client protocol) is setup or the edge: once per
  load, per link or per request, never per unit.
```
```

> (requests, pass 25: the stray fence above is closed by an empty one, so the outcome lines below read as outcomes.)

> forwarded to codegen (`crates/blaze-engine/`) (from requests, pass 17; told by tmux).
> forwarded to codegen (codegen-next1: the `// alloc:` justifications it names in `blaze-engine`) (requests, pass 19; told by tmux).
