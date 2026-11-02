<!-- From mem-db-def-hir (child of mem-db-def). Notes: docs/v2/plan/lanes/mem/db-def-hir.md.
     Report: /tmp/blaze-lanes/v2-mem-db-def-hir.md. Line numbers are as read on 2026-10-29;
     the files move under other lanes, so the function named with each line is what to match. -->

# mem-db-def-hir: `crates/hir` off the general allocator on its hit paths

Holder of every path below: **db-next2** (`crates/hir/`). Parts R10 and R11 also ask things of
other owners, named there. Contract: `docs/v2/plan/lanes/mem/db.md`, points 1 to 7.
Manifests, `#![feature(allocator_api)]` and the counting allocator in `src/lib.rs` are
mem-db-def's H1 to H3 (`v2/requests/mem-db-def.md`); they are not refiled here.

What the slice is, for the allocator:

- **No query runs of its own.** `crates/hir/` has no `#[salsa::tracked]` or `#[salsa::interned]`
  function (`rg 'salsa::tracked|salsa::interned' crates/hir/src` is empty). `db.rs` is
  re-exports; `check.rs` runs plane tasks through `Db::run_all`, which are not query runs;
  `evict.rs` and `match_lints.rs` are plain functions. So no file here opens
  a frame, with one exception: `SymbolCollector` (`symbols.rs`) runs only
  inside ide-db's tracked `module_symbols`, `library_symbols` and `extern_prelude_symbols`, so it
  takes the frame they open (R9).
- **Everything else is a hit path outside any query run**: an IDE request makes a `Semantics`,
  asks it things and drops it. Its per-call buffers become per-thread shelves (R1), and the
  `Semantics` caches stop being rebuilt from empty per request (R2).

Rules every change keeps:

- Public signatures of hit-path functions stay as they are. New forms sit beside the old ones.
- A shelf is a `blaze_mem::scratch::Shelved` type of this crate (R1), lent with
  `scratch::with`, so a call inside a call (a `descend_into_macros` callback that descends again)
  gets a buffer of its own, and nothing panics on a `RefCell` borrowed twice.
- Element types with a `'db` lifetime cannot sit in a `'static` thread-local. Those buffers are
  kept as `Vec<T<'static>>` and moved to `Vec<T<'db>>` empty by `scratch::recycle`, which is
  std's in-place `collect` (same size and alignment, so the allocation is reused). That is safe
  code. The budget tests in R12 check the reuse happens.
- No fallback: a shelf that is empty makes a new buffer and keeps it, the same path.
- No new `dyn`. R3's walk keeps the `&mut dyn FnMut` callback `descend_into_macros_impl` already
  takes. If dedyn's agents have made that generic by the time this is applied, the new
  `descend_into_macros_walk` takes the same generic `F` instead. Their edits stay.
- No new syntax handles. Code that is new here finds syntax through `syntax::view`, never by
  walking `SyntaxNode`/`SyntaxToken`. The R12 tests find their targets as views and keep them
  as `TextRange`s and `SyntaxNodePtr`s; they make a handle only where `Semantics`' public API
  still takes one. The shelves of R1 and the carried tables of R2 hold the types the existing
  walk already passes around: `SyntaxToken`, and `SyntaxNode` as a map key. They change no
  handle code into new handle code. When frontend-tree moves the walk to views, those element
  types become `NodeId`s or token indices, since a borrowed view cannot sit on a `'static`
  shelf. frontend-tree's agents may already have moved some functions named here to views. If
  so, apply the change to the moved function, keep their edits, and keep the buffer change as
  written.

## R1. New module `crates/hir/src/scratch.rs`, and `mod scratch;` in `src/lib.rs`

In `src/lib.rs`, after `mod display;` (line 39):

```rust
mod scratch;
```

The whole new file:

```rust
//! Per-thread buffers for the hit paths that run outside any query run (`Semantics`, display),
//! so with no frame: each kind is a `blaze_mem::scratch` shelf of this crate's, lent empty and
//! kept at its high-water mark, so a warm call allocates nothing. A call inside a call gets
//! another buffer of the kind, up to `scratch::DEPTH` deep.

use std::{
    cell::RefCell,
    ops::{Deref, DerefMut},
};

use blaze_mem::scratch::{self, DEPTH, Shelved};
use hir_expand::{HirFileId, MacroCallId};
use span::SyntaxContext;
use syntax::{SmolStr, SyntaxToken};

use crate::{EnumVariant, Field, InFile, semantics::LintAttr};

macro_rules! shelves {
    ($($(#[$doc:meta])* $name:ident, $shelf:ident: $elem:ty;)*) => {$(
        $(#[$doc])*
        #[derive(Default)]
        pub(crate) struct $name(Vec<$elem>);

        thread_local! {
            static $shelf: RefCell<Vec<$name>> = const { RefCell::new(Vec::new()) };
        }

        impl Deref for $name {
            type Target = Vec<$elem>;
            fn deref(&self) -> &Vec<$elem> {
                &self.0
            }
        }

        impl DerefMut for $name {
            fn deref_mut(&mut self) -> &mut Vec<$elem> {
                &mut self.0
            }
        }

        impl Shelved for $name {
            fn empty(&mut self) {
                self.0.clear();
            }

            fn held(&self) -> usize {
                self.0.capacity()
            }

            fn unshelve() -> Option<Self> {
                $shelf.try_with(|shelf| shelf.borrow_mut().pop()).ok().flatten()
            }

            fn shelve(self) {
                // A thread that is ending, or a shelf already `DEPTH` deep: the buffer is freed.
                let _ = $shelf.try_with(|shelf| {
                    let mut shelf = shelf.borrow_mut();
                    if shelf.len() < DEPTH {
                        // alloc: the shelf's own `DEPTH` slots, once per thread
                        shelf.push(self);
                    }
                });
            }
        }

        impl $name {
            /// Lends the thread's buffer of this kind, empty, for `f`.
            pub(crate) fn with<R>(f: impl FnOnce(&mut Vec<$elem>) -> R) -> R {
                scratch::with::<$name, R>(|buffer| f(&mut buffer.0))
            }
        }
    )*};
}

shelves! {
    /// `descend_into_macros_impl`'s groups still to walk: a file and how many of the pending
    /// tokens at the top are its.
    DescendGroups, DESCEND_GROUPS: (HirFileId, u32);
    /// `descend_into_macros_impl`'s tokens: those pending, and the group being walked.
    DescendTokens, DESCEND_TOKENS: (SyntaxToken, SyntaxContext);
    /// The derive and derive-attribute calls of an ADT, as `descend_into_macros_impl` walks them.
    DescendCalls, DESCEND_CALLS: MacroCallId;
    /// The first token's descendants in `descend_node_into_attributes`.
    DescendFirsts, DESCEND_FIRSTS: InFile<SyntaxToken>;
    /// The fields `display` shows.
    Fields, FIELDS: Field;
    /// The variants `display` shows.
    Variants, VARIANTS: EnumVariant;
    /// `Semantics::lint_attrs`' attributes, before they are handed out.
    LintAttrs, LINT_ATTRS: (LintAttr, SmolStr);
}

/// `buffer`, emptied, as a vector of `U`: the same allocation when `T` and `U` have one size and
/// alignment (they differ only in a lifetime here), by std's in-place `collect`.
pub(crate) fn recycle<T, U>(mut buffer: Vec<T>) -> Vec<U> {
    const { assert!(size_of::<T>() == size_of::<U>() && align_of::<T>() == align_of::<U>()) };
    buffer.clear();
    buffer.into_iter().map(|_| -> U { unreachable!("the buffer was emptied") }).collect()
}
```

Imports: `syntax::SmolStr` is what `semantics.rs` imports (line 50); `InFile` is the crate's
(`semantics.rs` takes it from `crate`, line 59); `LintAttr` is `pub` in `semantics.rs`
(line 195), so `crate::semantics::LintAttr` resolves.

Why a type per buffer: a `thread_local!` cannot be generic, and `Shelved` needs `'static`. The
shelf is the one in `blaze_mem::scratch` (`with` unshelves, empties, runs, empties, shelves), so
the nesting and the `DEPTH` bound are blaze-mem's.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R1, `crates/hir/src/scratch.rs` and `mod scratch;`. Differs: R10b's `Importables`, `IMPORTABLES_SEEN` and `LentCopied` are not in it yet (they wait with R10b); `recycle`, `with_clauses` and R5's `LentDrain` are.

## R2. `Semantics` caches carried from one request to the next on a thread

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:42): applied as a whole below (db-mem-def 21:40); in the tree at crates/hir/src/semantics/source_to_def.rs:835 (`Carry`), 859/884 (`take`/`give`).

### What allocates now

`SemanticsImpl::new` (`semantics.rs:457`) makes `s2d_cache`, `macro_call_cache` and
`signature_anon_consts_cache` empty. A `Semantics` lives for one IDE request, so every request:

- grows `SourceToDefCache`'s five maps (`semantics/source_to_def.rs:125`) from empty: about
  \( \log_2 n \) reallocations each;
- rebuilds every `DynMap` it reads (`cache_for`, line 495, calls `child_by_source`), which is the
  dominant cost of `to_def`: a map per key kind, one entry per child;
- grows `macro_call_cache` from empty.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:42): notes for R2, applied below.

### The change

The tables that are a function of the database alone stay whole while the database's nonce and
revision are the same: `dynmap_cache`, `file_to_def_cache`, `included_file_cache` and the macro
call cache. The rest keep only their capacity: `root_to_file_cache` is emptied (so
`find_file`'s "only query nodes derived from this instance of Semantics" check is unchanged), and
`expansion_info_cache` holds `ExpansionInfo<'db>`, so it is emptied and its storage moved through
`recycle`. A `Semantics` made by `new_within` answers `file_to_def` and includes for its crates
only, so it neither reads nor leaves those two tables.

`signature_anon_consts_cache` stays per request: its values are `AnonConstId<'db>` maps, made only
for expressions in signatures (array lengths, const arguments), which are rare on a request.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).

#### `semantics/source_to_def.rs`

1. The cache's expansion table becomes a vector with a `'static` index, so its storage can be
   carried. Before (lines 124 to 134):

```rust
#[derive(Default)]
pub(super) struct SourceToDefCache<'db> {
    pub(super) dynmap_cache: FxHashMap<(ChildContainer, HirFileId), DynMap>,
    expansion_info_cache: FxHashMap<MacroCallId, ExpansionInfo<'db>>,
    pub(super) file_to_def_cache: FxHashMap<FileId, SmallVec<[ModuleId; 1]>>,
    pub(super) included_file_cache: FxHashMap<EditionedFileId, Option<MacroCallId>>,
    /// Rootnode to HirFileId cache
    pub(super) root_to_file_cache: FxHashMap<SyntaxNode, HirFileId>,
    /// The crates a file is taken to be a module of, of those it may be, when any is.
    pub(super) within: Option<Arc<FxHashSet<Crate>>>,
}
```

After:

```rust
#[derive(Default)]
pub(super) struct SourceToDefCache<'db> {
    pub(super) dynmap_cache: FxHashMap<(ChildContainer, HirFileId), DynMap>,
    /// Where each macro file's expansion is in `expansions`.
    expansion_at: FxHashMap<MacroCallId, u32>,
    expansions: Vec<ExpansionInfo<'db>>,
    pub(super) file_to_def_cache: FxHashMap<FileId, SmallVec<[ModuleId; 1]>>,
    pub(super) included_file_cache: FxHashMap<EditionedFileId, Option<MacroCallId>>,
    /// Rootnode to HirFileId cache
    pub(super) root_to_file_cache: FxHashMap<SyntaxNode, HirFileId>,
    /// The crates a file is taken to be a module of, of those it may be, when any is.
    pub(super) within: Option<Arc<FxHashSet<Crate>>>,
}
```

2. `get_or_insert_expansion` (line 182). Before:

```rust
        self.expansion_info_cache.entry(macro_file).or_insert_with(|| {
            let exp_info = macro_file.expansion_info(db);
            ...
            exp_info
        })
```

After (the body of the closure unchanged, now in the vacant arm):

```rust
        let at = match self.expansion_at.entry(macro_file) {
            std::collections::hash_map::Entry::Occupied(it) => *it.get(),
            std::collections::hash_map::Entry::Vacant(it) => {
                let exp_info = macro_file.expansion_info(db);

                // Ensure that the cache contains syntax nodes from expanded macros,
                // whose root may be in another file.
                let InMacroFile { file_id, value } = exp_info.expanded();
                Self::cache(&mut self.root_to_file_cache, value, file_id.into());

                // include!("foo.rs") invocations are awkward: in addition to the
                // expansion site there's the included file (foo.rs), so we need to
                // ensure that it exists in the cache too.
                if macro_file.is_include_macro(db) {
                    let arg = exp_info.arg();
                    if let Some(arg_node) = arg.value {
                        Self::cache(&mut self.root_to_file_cache, arg_node.tree_top(), arg.file_id);
                    }
                }

                let at = u32::try_from(self.expansions.len())
                    .expect("more than u32::MAX macro expansions in one `Semantics`");
                self.expansions.push(exp_info);
                *it.insert(at)
            }
        };
        &self.expansions[at as usize]
```

(`FxHashMap` is std's `HashMap` with `FxBuildHasher`, so `std::collections::hash_map::Entry` is
its entry type; import it at the top as `use std::collections::hash_map::Entry;` to match the
file's style.)

3. `crates_of` (line 136) stops making two vectors per call. Before:

```rust
fn crates_of(
    db: &Db,
    within: Option<&FxHashSet<Crate>>,
    file: FileId,
) -> [Vec<Crate>; 2] {
    let (some, rest) = (relevant_crates(db, file).iter())
        .partition(|it| within.is_some_and(|within| within.contains(it)));
    [some, rest]
}
```

After (doc comment unchanged above it):

```rust
fn crates_of<'a>(
    db: &'a Db,
    within: Option<&'a FxHashSet<Crate>>,
    file: FileId,
    first: bool,
) -> impl Iterator<Item = Crate> + 'a {
    relevant_crates(db, file)
        .iter()
        .copied()
        .filter(move |it| within.is_some_and(|within| within.contains(it)) == first)
}
```

`get_or_insert_include_for` (line 169). Before:

```rust
        for crates in crates_of(db, self.within.as_deref(), file.file_id(db)) {
            for crate_id in crates {
```

After:

```rust
        let within = self.within.as_deref();
        for first in [true, false] {
            for crate_id in crates_of(db, within, file.file_id(db), first) {
```

(the rest of the loop body unchanged).

`file_to_def` (line 219). Before:

```rust
            let [some, rest] = crates_of(self.db, self.cache.within.as_deref(), file);
            let mut mods = self.modules_in(&some, file);
            if mods.is_empty() {
                mods = self.modules_in(&rest, file);
            }
```

After (an `Arc` count, not a copy: `modules_in` writes the cache while the crates are read):

```rust
            let within = self.cache.within.clone();
            let mut mods = self.modules_in(crates_of(self.db, within.as_deref(), file, true), file);
            if mods.is_empty() {
                mods = self.modules_in(crates_of(self.db, within.as_deref(), file, false), file);
            }
```

`modules_in` (line 229): `crates: &[Crate]` becomes `crates: impl Iterator<Item = Crate>`, and
`for &crate_id in crates {` becomes `for crate_id in crates {`.

4. The carry. Add at the end of the file:

```rust
/// What a `Semantics` leaves to the next one made on its thread, at the database nonce and
/// revision it was made at. A request's `Semantics` would otherwise grow every table from
/// empty and rebuild every `DynMap` it reads.
#[derive(Default)]
struct Carry {
    at: Option<(base_db::Nonce, salsa::Revision)>,
    dynmap_cache: FxHashMap<(ChildContainer, HirFileId), DynMap>,
    expansion_at: FxHashMap<MacroCallId, u32>,
    /// Always empty: its storage, for the next `Semantics`' expansions.
    expansions: Vec<ExpansionInfo<'static>>,
    file_to_def_cache: FxHashMap<FileId, SmallVec<[ModuleId; 1]>>,
    included_file_cache: FxHashMap<EditionedFileId, Option<MacroCallId>>,
    root_to_file_cache: FxHashMap<SyntaxNode, HirFileId>,
    macro_call_cache: FxHashMap<InFile<ast::MacroCall>, MacroCallId>,
}

/// More dyn maps than this are not carried: a thread that walked many files keeps their
/// tables' storage but not the maps.
const CARRIED_DYN_MAPS: usize = 4096;

thread_local! {
    static CARRY: std::cell::RefCell<Option<Carry>> = const { std::cell::RefCell::new(None) };
}

impl<'db> SourceToDefCache<'db> {
    /// The thread's carried caches for a `Semantics` of `db`, and its macro call cache: whole
    /// when the last `Semantics` on this thread was of the same database at the same
    /// revision, else empty with their capacity.
    pub(super) fn take(
        db: &'db Db,
    ) -> (SourceToDefCache<'db>, FxHashMap<InFile<ast::MacroCall>, MacroCallId>) {
        let mut carry = CARRY.try_with(|it| it.borrow_mut().take()).ok().flatten().unwrap_or_default();
        if carry.at != Some(db.nonce_and_revision()) {
            carry.dynmap_cache.clear();
            carry.file_to_def_cache.clear();
            carry.included_file_cache.clear();
            carry.macro_call_cache.clear();
        }
        let cache = SourceToDefCache {
            dynmap_cache: carry.dynmap_cache,
            expansion_at: carry.expansion_at,
            expansions: crate::scratch::recycle(carry.expansions),
            file_to_def_cache: carry.file_to_def_cache,
            included_file_cache: carry.included_file_cache,
            root_to_file_cache: carry.root_to_file_cache,
            within: None,
        };
        (cache, carry.macro_call_cache)
    }

    /// Leaves `self` and `macro_call_cache` to the next `Semantics` on this thread. A cache
    /// that answered for some crates only (`within`) leaves no file answers.
    pub(super) fn give(
        mut self,
        db: &Db,
        macro_call_cache: FxHashMap<InFile<ast::MacroCall>, MacroCallId>,
    ) {
        if self.within.is_some() {
            self.file_to_def_cache.clear();
            self.included_file_cache.clear();
        }
        if self.dynmap_cache.len() > CARRIED_DYN_MAPS {
            self.dynmap_cache.clear();
        }
        self.expansion_at.clear();
        self.root_to_file_cache.clear();
        let carry = Carry {
            at: Some(db.nonce_and_revision()),
            dynmap_cache: self.dynmap_cache,
            expansion_at: self.expansion_at,
            expansions: crate::scratch::recycle(self.expansions),
            file_to_def_cache: self.file_to_def_cache,
            included_file_cache: self.included_file_cache,
            root_to_file_cache: self.root_to_file_cache,
            macro_call_cache,
        };
        // A `Semantics` alive inside another one's request leaves nothing: the outer one's
        // tables are the larger.
        let _ = CARRY.try_with(|it| {
            let mut slot = it.borrow_mut();
            if slot.is_none() {
                *slot = Some(carry);
            }
        });
    }
}
```

Imports this needs in `source_to_def.rs`: `ast` is imported (line 115), `InFile` is
(`crate::InFile`, line 120), `ExpansionInfo`, `MacroCallId`, `EditionedFileId`, `HirFileId` are
(line 106). `salsa` is a dependency of `hir` (`Cargo.toml`). `base_db::Nonce` is public
(`crates/base-db/src/lib.rs:322`) and `Db::nonce_and_revision` is
`crates/base-db/src/db.rs:301`.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).

#### `semantics.rs`

1. `SemanticsImpl::new` (line 457). Before:

```rust
    fn new(db: &'db Db) -> Self {
        SemanticsImpl {
            db,
            s2d_cache: Default::default(),
            macro_call_cache: Default::default(),
            signature_anon_consts_cache: Default::default(),
        }
    }
```

After:

```rust
    fn new(db: &'db Db) -> Self {
        let (s2d_cache, macro_call_cache) = SourceToDefCache::take(db);
        SemanticsImpl {
            db,
            s2d_cache: RefCell::new(s2d_cache),
            macro_call_cache: RefCell::new(macro_call_cache),
            signature_anon_consts_cache: Default::default(),
        }
    }
```

2. `Semantics::new_within` (line 211): after `impl_.s2d_cache.get_mut().within = Some(crates);`
   add the two lines below, since the carried answers were for all crates:

```rust
        impl_.s2d_cache.get_mut().file_to_def_cache.clear();
        impl_.s2d_cache.get_mut().included_file_cache.clear();
```

3. A `Drop` for `SemanticsImpl`, after its `impl` block:

```rust
impl Drop for SemanticsImpl<'_> {
    fn drop(&mut self) {
        let cache = mem::take(self.s2d_cache.get_mut());
        let macro_calls = mem::take(self.macro_call_cache.get_mut());
        cache.give(self.db, macro_calls);
    }
}
```

(`mem` is imported, line 9; `SourceToDefCache` is, line 63.) Nothing destructures a
`SemanticsImpl` (`rg 'SemanticsImpl \{' crates/hir/src` finds only `new`), so the `Drop` breaks
no move.

Soundness of keeping the tables: within one nonce and revision the database answers every query
the same, so a `DynMap`, a file's modules, an include and a macro call's id are the same as a
fresh `Semantics` would build. The database cannot change revision while a `Semantics` borrows
it (`&'db Db`; a new revision takes `&mut Db`), so the revision read in `give` is the one the
tables were built at. Memory: a carried `macro_call_cache` holds syntax nodes of its revision
until the thread's next `Semantics` clears it; that is one request's worth.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R2, both halves: `source_to_def.rs` (`expansion_at`/`expansions`, `crates_of`, `Carry`, `take`/`give`) and `semantics.rs` (`SemanticsImpl::new` takes the carry, `new_within` clears the file answers, `Drop` gives it back; the `Drop` impl sits just before `impl SemanticsImpl`).

## R3. `descend_into_macros_impl`'s stack on shelves (`semantics.rs:1232`)

What allocates now, per call: `stack: Vec<(_, SmallVec<[_; 2]>)> = vec![]` grows on the first
push; every expansion that maps a span to more than two tokens spills its `SmallVec`; the derive
branch copies each derive's id list (`c.to_owned()`) into a fresh `Vec` and gathers them in a
`SmallVec<[_; 2]>`.

After: the groups and the tokens are three shelf buffers, and the derive calls a fourth, all
lent for the call. The walk is the same: a group is popped, its tokens are walked first to last
(the old code reversed and popped), and groups pushed while walking it are walked after it, last
pushed first.

1. The whole function body after `let span = ...;` (line 1241) goes inside the lends:

```rust
        DescendGroups::with(|groups| {
            DescendTokens::with(|pending| {
                DescendTokens::with(|current| {
                    self.descend_into_macros_walk(
                        file_id,
                        token,
                        span,
                        always_descend_into_derives,
                        f,
                        groups,
                        pending,
                        current,
                    )
                })
            })
        })
```

and the old body moves into a new private method beside it:

```rust
    #[allow(clippy::too_many_arguments)]
    fn descend_into_macros_walk<T>(
        &self,
        file_id: HirFileId,
        token: SyntaxToken,
        span: span::Span,
        always_descend_into_derives: bool,
        f: &mut dyn FnMut(InFile<SyntaxToken>, SyntaxContext) -> ControlFlow<T>,
        groups: &mut Vec<(HirFileId, u32)>,
        pending: &mut Vec<(SyntaxToken, SyntaxContext)>,
        current: &mut Vec<(SyntaxToken, SyntaxContext)>,
    ) -> Option<T> {
```

(`span::Span` is what `span_for_range` returns; match the name in the tree, it may be imported
as `SpanData` or a type alias in `span`.) Inside it, with `let db = self.db;` first:

2. `process_expansion_for_token` (line 1244). Before:

```rust
        let process_expansion_for_token =
            |ctx: &mut SourceToDefCtx<'_, '_>, stack: &mut Vec<_>, macro_file| {
                let InMacroFile { file_id, value: mapped_tokens } = ctx
                    .cache
                    .get_or_insert_expansion(ctx.db, macro_file)
                    .map_range_down(span)?
                    .map(SmallVec::<[_; 2]>::from_iter);
                // we have found a mapping for the token if the vec is non-empty
                let res = mapped_tokens.is_empty().not().then_some(());
                // requeue the tokens we got from mapping our current token down
                stack.push((HirFileId::from(file_id), mapped_tokens));
                res
            };
```

After:

```rust
        let process_expansion_for_token = |ctx: &mut SourceToDefCtx<'_, '_>,
                                           groups: &mut Vec<(HirFileId, u32)>,
                                           pending: &mut Vec<(SyntaxToken, SyntaxContext)>,
                                           macro_file| {
            let InMacroFile { file_id, value: mapped_tokens } =
                ctx.cache.get_or_insert_expansion(ctx.db, macro_file).map_range_down(span)?;
            // requeue the tokens we got from mapping our current token down
            let before = pending.len();
            pending.extend(mapped_tokens);
            let mapped = u32::try_from(pending.len() - before)
                .expect("a span mapped down to more than u32::MAX tokens");
            groups.push((HirFileId::from(file_id), mapped));
            // we have found a mapping for the token if any token was pushed
            (mapped != 0).then_some(())
        };
```

3. The stack's start (lines 1262 to 1274). Before:

```rust
        let mut stack: Vec<(_, SmallVec<[_; 2]>)> = vec![];
        let include = ...;
        match include {
            Some(include) => {
                // include! inputs are always from real files, so they only need to be handled once upfront
                self.with_ctx(|ctx| process_expansion_for_token(ctx, &mut stack, include))?;
            }
            None => {
                stack.push((file_id, smallvec![(token, span.ctx)]));
            }
        }
```

After (the `include` binding unchanged):

```rust
        match include {
            Some(include) => {
                // include! inputs are always from real files, so they only need to be handled once upfront
                self.with_ctx(|ctx| process_expansion_for_token(ctx, groups, pending, include))?;
            }
            None => {
                pending.push((token, span.ctx));
                groups.push((file_id, 1));
            }
        }
```

4. `filter_duplicates` (line 1280): `|tokens: &mut SmallVec<_>, range: TextRange|` becomes
   `|tokens: &mut Vec<(SyntaxToken, SyntaxContext)>, range: TextRange|`, body unchanged.

5. The loop head (lines 1284 to 1289). Before:

```rust
        while let Some((expansion, ref mut tokens)) = stack.pop() {
            // Reverse the tokens so we prefer first tokens (to accommodate for popping from the
            // back)
            // alternatively we could pop from the front but that would shift the content on every pop
            tokens.reverse();
            while let Some((token, ctx)) = tokens.pop() {
```

After:

```rust
        while let Some((expansion, len)) = groups.pop() {
            current.clear();
            current.extend(pending.drain(pending.len() - len as usize..));
            let tokens = &mut *current;
            // Reverse the tokens so we prefer first tokens (to accommodate for popping from the
            // back)
            // alternatively we could pop from the front but that would shift the content on every pop
            tokens.reverse();
            while let Some((token, ctx)) = tokens.pop() {
```

6. Every `process_expansion_for_token(ctx, &mut stack, X)` in the loop (lines 1343, 1368,
   1371, 1418, 1422, 1452 to 1456, 1505) becomes `process_expansion_for_token(ctx, groups,
   pending, X)`. The closures that call it capture `groups` and `pending` by `&mut` the way they
   captured `&mut stack`.

7. The derive branch (lines 1351 to 1377). Before:

```rust
                        let res = self.with_ctx(|ctx| {
                            let (derives, adt) = token
                                .parent_ancestors()
                                .filter_map(ast::Adt::cast)
                                .find_map(|adt| {
                                    Some((
                                        ctx.derive_macro_calls(InFile::new(expansion, &adt))?
                                            .map(|(a, b, c)| (a, b, c.to_owned()))
                                            .collect::<SmallVec<[_; 2]>>(),
                                        adt,
                                    ))
                                })?;
                            for (_, derive_attr, derives) in derives {
                                // as there may be multiple derives registering the same helper
                                // name, we gotta make sure to call this for all of them!
                                // FIXME: We need to call `f` for all of them as well though!
                                process_expansion_for_token(ctx, &mut stack, derive_attr);
                                for derive in derives.into_iter().flatten() {
                                    let Either::Left(derive) = derive else { continue };
                                    process_expansion_for_token(ctx, &mut stack, derive);
                                }
                            }
                            // remove all tokens that are within the derives expansion
                            filter_duplicates(tokens, adt.syntax().text_range());
                            Some(())
                        });
```

After (each derive attribute's call, then its derives' calls, in the order they were walked):

```rust
                        let res = self.with_ctx(|ctx| {
                            DescendCalls::with(|calls| {
                                let adt = token.parent_ancestors().filter_map(ast::Adt::cast).find(
                                    |adt| {
                                        let Some(found) =
                                            ctx.derive_macro_calls(InFile::new(expansion, adt))
                                        else {
                                            return false;
                                        };
                                        for (_, derive_attr, derives) in found {
                                            calls.push(derive_attr);
                                            calls.extend(
                                                derives.iter().filter_map(|&it| it?.left()),
                                            );
                                        }
                                        true
                                    },
                                )?;
                                // as there may be multiple derives registering the same helper
                                // name, we gotta make sure to call this for all of them!
                                // FIXME: We need to call `f` for all of them as well though!
                                for &call in calls.iter() {
                                    process_expansion_for_token(ctx, groups, pending, call);
                                }
                                // remove all tokens that are within the derives expansion
                                filter_duplicates(tokens, adt.syntax().text_range());
                                Some(())
                            })
                        });
```

8. Imports in `semantics.rs`: add `use crate::scratch::{DescendCalls, DescendFirsts,
   DescendGroups, DescendTokens, LintAttrs};`. `smallvec!` stays used by the public functions
   that return `SmallVec`.

The callback `f` may itself descend (an IDE callback that asks `Semantics` again): it gets its own
three buffers from the shelf (R1), so the walk above is never aliased.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> db-expand-names (2026-10-29) adds `db` to `derive_helpers_in_scope` and `diagnostics_of`
> (`v2/requests/db-expand.md`, "db-expand-names: frontend-bodies R1's callers outside hir-def").
> Their call in this walk (the derive-helper branch, `semantics.rs` 1545 after R3) is in the part
> R3 leaves unchanged. The two edits are independent: their argument goes on top of R3 as applied.

> applied by db-mem-def (21:40): R3 as written: `descend_into_macros_impl` lends `DescendGroups` and two `DescendTokens`, the walk is `descend_into_macros_walk`, the derive branch collects into `DescendCalls`.

### R3b. The other descend helpers

- `descend_token_into_include_expansion` (line 979): the `SmallVec<[_; 2]>` it builds only to
  `pop` the last token. Before:

```rust
        let Some(InMacroFile { file_id, value: mut mapped_tokens }) = self.with_ctx(|ctx| {
            Some(
                ctx.cache
                    .get_or_insert_expansion(ctx.db, include)
                    .map_range_down(span)?
                    .map(SmallVec::<[_; 2]>::from_iter),
            )
        }) else {
            return tok.into();
        };
        // We should only get one result at most
        mapped_tokens.pop().map_or_else(|| tok.into(), |(tok, _)| InFile::new(file_id.into(), tok))
```

After:

```rust
        let Some(InMacroFile { file_id, value: last }) = self.with_ctx(|ctx| {
            Some(
                ctx.cache
                    .get_or_insert_expansion(ctx.db, include)
                    .map_range_down(span)?
                    .map(Iterator::last),
            )
        }) else {
            return tok.into();
        };
        // We should only get one result at most
        last.map_or_else(|| tok.into(), |(tok, _)| InFile::new(file_id.into(), tok))
```

- `descend_node_into_attributes` (line 1005), the `else` branch (lines 1037 to 1065): the
  first token's descendants go to a shelf. Before:

```rust
            let mut scratch: SmallVec<[_; 1]> = smallvec![];
            self.descend_into_macros_all(
                InFile::new(file.file_id, first),
                false,
                &mut |token, _ctx| scratch.push(token),
            );

            let mut scratch = scratch.into_iter();
            self.descend_into_macros_all(
```

After (the second walk and its closure unchanged, inside the lend):

```rust
            DescendFirsts::with(|firsts| {
                self.descend_into_macros_all(
                    InFile::new(file.file_id, first),
                    false,
                    &mut |token, _ctx| firsts.push(token),
                );

                let mut scratch = firsts.drain(..);
                self.descend_into_macros_all(
                    ...
                );
            });
```

- `descend_into_macros`, `_no_opaque`, `_exact`, `_exact_with_file` (lines 1113 to 1200) keep
  their `SmallVec<[_; 1]>` results: one descendant is inline, which is the common case, and the
  result is the caller's. `descend_into_macros_cb` and `descend_into_macros_breakable` are the
  visitor forms already; R11 asks the callers that only iterate to use them.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R3b as written (`Iterator::last`; `DescendFirsts` in `descend_node_into_attributes`).

## R4. `kmerge_by` of at most two iterators, without the heap (`semantics.rs`)

`Itertools::kmerge_by` collects its iterators into a heap `Vec` on every call. The three calls
here merge what `token_at_offset` yields, which is at most two tokens. A two-way merge with
kmerge's exact order replaces it. kmerge keeps the iterator that gave the last item first on a
tie (its `sift_down` swaps only when the other head is strictly less), and takes each iterator's
first item when it is made; this does the same.

Add at the end of `semantics.rs`:

```rust
/// `Itertools::kmerge_by` of at most two iterators (what `token_at_offset` yields), without
/// its heap: the same order, the iterator that gave the last item staying first on a tie.
struct KMerge2<I: Iterator, F> {
    first: Option<(I::Item, I)>,
    second: Option<(I::Item, I)>,
    less: F,
}

fn kmerge2<I: Iterator, F: FnMut(&I::Item, &I::Item) -> bool>(
    mut iters: impl Iterator<Item = I>,
    mut less: F,
) -> KMerge2<I, F> {
    let mut head = |it: Option<I>| it.and_then(|mut it| Some((it.next()?, it)));
    let mut first = head(iters.next());
    let mut second = head(iters.next());
    debug_assert!(iters.next().is_none(), "`kmerge2` of more than two iterators");
    if first.is_none() {
        mem::swap(&mut first, &mut second);
    }
    if let (Some(a), Some(b)) = (&first, &second)
        && less(&b.0, &a.0)
    {
        mem::swap(&mut first, &mut second);
    }
    KMerge2 { first, second, less }
}

impl<I: Iterator, F: FnMut(&I::Item, &I::Item) -> bool> Iterator for KMerge2<I, F> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        let (head, tail) = self.first.as_mut()?;
        let Some(next) = tail.next() else {
            return mem::replace(&mut self.first, self.second.take()).map(|(item, _)| item);
        };
        let item = mem::replace(head, next);
        if let (Some(a), Some(b)) = (&self.first, &self.second)
            && (self.less)(&b.0, &a.0)
        {
            mem::swap(&mut self.first, &mut self.second);
        }
        Some(item)
    }
}
```

The three call sites, each `X.kmerge_by(pred)` becoming `kmerge2(X, pred)`:

- `find_namelike_at_offset_with_descend` (line 269). Before:

```rust
        node.token_at_offset(offset)
            .map(move |token| self.descend_into_macros_no_opaque(token, true))
            .map(|descendants| descendants.into_iter().filter_map(move |it| it.value.parent()))
            // re-order the tokens from token_at_offset by returning the ancestors with the smaller first nodes first
            // See algo::ancestors_at_offset, which uses the same approach
            .kmerge_by(|left, right| left.text_range().len().lt(&right.text_range().len()))
            .filter_map(ast::NameLike::cast)
```

After:

```rust
        let descended = node
            .token_at_offset(offset)
            .map(move |token| self.descend_into_macros_no_opaque(token, true))
            .map(|descendants| descendants.into_iter().filter_map(move |it| it.value.parent()));
        // re-order the tokens from token_at_offset by returning the ancestors with the smaller first nodes first
        // See algo::ancestors_at_offset, which uses the same approach
        kmerge2(descended, |left, right| left.text_range().len().lt(&right.text_range().len()))
            .filter_map(ast::NameLike::cast)
```

  This is in `impl Semantics`, where `kmerge2` (a free function of the module) is in scope.

- `descend_node_at_offset` (line 1532): `node.token_at_offset(offset).map(..).map(..).kmerge_by(|left, right| ..)`
  becomes `kmerge2(node.token_at_offset(offset).map(..).map(..), |left, right| ..)`, the two
  `map`s and the predicate unchanged.
- `ancestors_at_offset_with_macros` (line 1644). Before:

```rust
        node.token_at_offset(offset)
            .map(|token| self.token_ancestors_with_macros(token))
            .kmerge_by(|node1, node2| node1.text_range().len() < node2.text_range().len())
```

After:

```rust
        kmerge2(
            node.token_at_offset(offset).map(|token| self.token_ancestors_with_macros(token)),
            |node1, node2| node1.text_range().len() < node2.text_range().len(),
        )
```

`itertools::Itertools` stays imported if anything else in the file uses it (`rg 'Itertools|\.join\(|\.sorted|\.unique|\.dedup' crates/hir/src/semantics.rs`); else the import goes.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R4 as written, with `kmerge2` and `KMerge2` `pub(crate)` for R12's test; `head` is a plain `let` (it captures nothing mutable). `Itertools` stays (`.join`).

## R5. The rest of `semantics.rs`' per-call buffers

- `SemanticsScope::speculative_resolve` (line 2796): `let mut segments = vec![];` becomes
  `let mut segments = SmallVec::<[Name; 8]>::new();`. `ModPath::from_segments` takes
  `impl IntoIterator<Item = Name>` and gathers into its own inline buffer of 8
  (`crates/hir-expand/src/mod_path.rs:106`), so a path of up to 8 segments allocates nothing
  here.
- `Semantics::lint_attrs` (line 298): the `Vec` it fills per item goes to the `LintAttrs` shelf,
  and the iterator it returns hands the items out of the lent buffer, which returns to the shelf
  when the iterator is dropped. Before: `let mut result = Vec::new();` ... `result.into_iter()`.
  After: `let mut result = blaze_mem::scratch::lend::<LintAttrs>();` (the pushes unchanged:
  `LintAttrs` derefs to the vector), and the return is

```rust
        crate::scratch::LentDrain::new(result)
```

  with, in `scratch.rs`:

```rust
/// The items of a lent buffer, first to last or from the back, the buffer going back to its
/// shelf when this is dropped.
pub(crate) struct LentDrain<B: Shelved, T> {
    buffer: scratch::Lent<B>,
    /// The next item from the front; those before it are taken.
    front: usize,
    _item: std::marker::PhantomData<fn() -> T>,
}

impl<B: Shelved + DerefMut<Target = Vec<T>>, T> LentDrain<B, T> {
    pub(crate) fn new(buffer: scratch::Lent<B>) -> Self {
        LentDrain { buffer, front: 0, _item: std::marker::PhantomData }
    }
}

impl<B: Shelved + DerefMut<Target = Vec<T>>, T: Default> Iterator for LentDrain<B, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        let item = self.buffer.get_mut(self.front)?;
        self.front += 1;
        Some(std::mem::take(item))
    }
}

impl<B: Shelved + DerefMut<Target = Vec<T>>, T: Default> DoubleEndedIterator for LentDrain<B, T> {
    fn next_back(&mut self) -> Option<T> {
        if self.buffer.len() == self.front {
            return None;
        }
        self.buffer.pop()
    }
}
```

  `T: Default` needs `(LintAttr, SmolStr): Default`, so `LintAttr` (line 195) gains a default:
  `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]` and `#[default]` on `Allow`.
  The taken slots are left holding `(Allow, "")`, which nobody reads: the buffer is emptied when
  it goes back to the shelf. (`Lent` derefs to the buffer type, which derefs to the vector, so
  `get_mut`, `len` and `pop` resolve through both.)
- `rename_conflicts`, `fn_return_points`, `get_unsafe_ops*`, `derive_helpers_in_scope`,
  `derive_helper`, `resolve_derive_macro`, `expand_derive_macro`, `record_*_fields`,
  `as_format_args_parts` return their collection to the caller, so it is the caller's result,
  made once at size from an `ExactSizeIterator` where one is collected. They stay.
- `find_file`'s panic message (line 2373) formats only on the panic path.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R5: `speculative_resolve`'s `SmallVec<[Name; 8]>`, `lint_attrs` on the `LintAttrs` shelf returning `LentDrain`, `LintAttr: Default` (`#[default] Allow`).

## R6. `Module`, `Crate`, `Adt`, `Trait`, `Impl` accessors: iterator forms beside the `Vec` ones (`lib.rs`)

Each accessor below collects into a fresh `Vec` per call, and nearly every caller only iterates
it. Every table they read lives for `'db` (`ModuleIdLt::def_map` returns `&'db DefMap`,
`DefMap::module` `&'a ModuleData`, `ModuleData::scope` `&'a ItemScope`; `VariantFields::of`,
`enum_variants`, `trait_items`, `impl_items` return references into the database), so the
iterator form borrows the database and allocates nothing. An iterator is the visitor that keeps
`break`, `?` and `return` working in the caller, so it is used here instead of a `for_each_*`
closure.

New methods, each beside the one named (the `Vec` form stays, now `self.X_iter(db).collect()`):

```rust
impl Module {
    /// [`Module::declarations`], without the vector.
    pub fn declarations_iter(self, db: &Db) -> impl Iterator<Item = ModuleDef> + '_ {
        let scope = self.id.def_map(db).module(db, self.id).scope(db);
        scope
            .declarations()
            .map(ModuleDef::from)
            .chain(scope.unnamed_consts().map(|id| ModuleDef::Const(Const::from(id))))
    }

    /// [`Module::impl_defs`], without the vector.
    pub fn impl_defs_iter(self, db: &Db) -> impl Iterator<Item = Impl> + '_ {
        let scope = self.id.def_map(db).module(db, self.id).scope(db);
        scope.impls().map(Impl::from).chain(scope.builtin_derive_impls().map(Impl::from))
    }

    /// [`Module::legacy_macros`], without the vector.
    pub fn legacy_macros_iter(self, db: &Db) -> impl Iterator<Item = Macro> + '_ {
        let scope = self.id.def_map(db).module(db, self.id).scope(db);
        scope.legacy_macros().flat_map(|(_, it)| it).map(|&it| it.into())
    }
}

impl Struct {
    /// [`Struct::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        self.id.fields(db).fields().iter().map(move |(id, _)| Field { parent: self.into(), id })
    }
}

impl Union {
    /// [`Union::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        self.id.fields(db).fields().iter().map(move |(id, _)| Field { parent: self.into(), id })
    }
}

impl EnumVariant {
    /// [`EnumVariant::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        self.id.fields(db).fields().iter().map(move |(id, _)| Field { parent: self.into(), id })
    }
}

impl Variant {
    /// [`Variant::fields`], without the vector.
    pub fn fields_iter(self, db: &Db) -> impl ExactSizeIterator<Item = Field> + '_ {
        VariantId::from(self)
            .fields(db)
            .fields()
            .iter()
            .map(move |(id, _)| Field { parent: self, id })
    }
}

impl Enum {
    /// [`Enum::variants`], without the vector.
    pub fn variants_iter(self, db: &Db) -> impl ExactSizeIterator<Item = EnumVariant> + '_ {
        self.id.enum_variants(db).variants.values().map(|&(id, _)| EnumVariant { id })
    }
}

impl Trait {
    /// [`Trait::items`], without the vector.
    pub fn items_iter(self, db: &Db) -> impl ExactSizeIterator<Item = AssocItem> + '_ {
        self.id.trait_items(db).items.iter().map(|(_name, it)| (*it).into())
    }
}
```

Where each goes: `Module` at `lib.rs:769` (`declarations`), 779 (`legacy_macros`), 805
(`impl_defs`); `Struct::fields` 983; `Union::fields` 1059; `Enum::variants` 1094;
`EnumVariant::fields` 1191; `Variant::fields` 1352; `Trait::items` 2433. If the arena's `iter`
or `variants.values()` is not `ExactSizeIterator` in the tree (`la_arena::Arena::iter` is;
`FxIndexMap::values` is), the return type is `impl Iterator<..>` instead. `Field.parent` is a
`Variant` (`lib.rs:870`), so `Variant::fields_iter` sets it to `self` as `Variant::fields` does
through each arm.

`Impl::items` (line 4166) has two arms of different iterator types (an impl's own items, or a
builtin derive's methods); its iterator form is `Either`:

```rust
    /// [`Impl::items`], without the vector.
    pub fn items_iter(self, db: &Db) -> impl Iterator<Item = AssocItem> + '_ {
        match self.id {
            AnyImplId::ImplId(id) => {
                Either::Left(id.impl_items(db).items.iter().map(|&(_, it)| it.into()))
            }
            AnyImplId::BuiltinDeriveImplId(impl_) => Either::Right(
                /* the second arm's iterator chain of `items`, unchanged, without its `.collect()` */
            ),
        }
    }
```

(the applier copies the second arm's chain from `items` as it is in the tree; `either::Either`
is an `Iterator` when both sides are, and is imported at `lib.rs:56`).

`Module::children` (line 638) already returns an iterator but collects first. The tables live
for `'db`, so it stops collecting. Before:

```rust
    pub fn children(self, db: &Db) -> impl Iterator<Item = Module> {
        let def_map = self.id.def_map(db);
        let children = def_map
            .module(db, self.id)
            .children(db)
            .values()
            .map(|module_id| Module { id: *module_id })
            .collect::<Vec<_>>();
        children.into_iter()
    }
```

After (edition 2024: the `impl Iterator` captures `db`'s lifetime, so callers are unchanged):

```rust
    pub fn children(self, db: &Db) -> impl Iterator<Item = Module> {
        let def_map = self.id.def_map(db);
        def_map.module(db, self.id).children(db).values().map(|module_id| Module { id: *module_id })
    }
```

`Crate::modules` (line 323) and `Crate::dependencies` (line 281): the same, as
`modules_iter` and `dependencies_iter`:

```rust
    /// [`Crate::modules`], without the vector.
    pub fn modules_iter(self, db: &Db) -> impl Iterator<Item = Module> + '_ {
        crate_def_map(db, self.id).modules(db).map(|(id, _)| id.into())
    }
```

(`DefMap::modules<'a>(&'a self, ..)` at `crates/hir-def/src/nameres.rs:762`; `crate_def_map`
returns `&'db DefMap`.)

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:42): applied below (db-mem-def 21:40); in the tree at crates/hir/src/lib.rs (`declarations_iter`, `impl_defs_iter`, `fields_iter`, `variants_iter`, `items_iter`).

### Callers in `crates/hir/` (migrated here)

| file:line | before | after |
|---|---|---|
| `lib.rs:1149` `Enum::is_data_carrying` | `self.variants(db).iter().any(..)` | `self.variants_iter(db).any(..)` |
| `lib.rs:2438` `Trait::items_with_supertraits` | `.flat_map(\|tr\| tr.items(db))` | `.flat_map(\|tr\| tr.items_iter(db))` |
| `attrs.rs:589` | `def.fields(db).into_iter().find(..)` | `def.fields_iter(db).find(..)` |
| `term_search/tactics.rs:93` | `.flat_map(\|it\| it.items(db))` | `.flat_map(\|it\| it.items_iter(db))` |
| `term_search/tactics.rs:210` | `.variants(db).into_iter()` | `.variants_iter(db)` |
| `term_search/tactics.rs:215` | `.fields(db).into_iter()` | `.fields_iter(db)` |
| `term_search/tactics.rs:423`, `:623` | `imp.items(db).into_iter().map(..)` | `imp.items_iter(db).map(..)` |
| `tests/api.rs:25` | `.flat_map(\|krate\| krate.root_module(db).declarations(db))` | `.flat_map(\|krate\| krate.root_module(db).declarations_iter(db))` |
| `tests/api.rs:37` | `.flat_map(\|it\| it.items(db))` | `.flat_map(\|it\| it.items_iter(db))` |

`attrs.rs:589`: `def` there is a `Variant`; if it is another type with `fields`, use that type's
`fields_iter`. `term_search/tactics.rs:160` and `expr.rs:201`, `:243` index or zip the fields
against parameters (`fields.iter()` twice), so they keep the vector. `term_search/tactics.rs:542`
is `Type::fields`, not one of these.

Callers in other crates: R11.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R6, the iterator forms and hir's callers (`attrs.rs`, `term_search/tactics.rs`, `tests/api.rs`). `Module::children` returns its iterator without collecting.

## R7. `display.rs`

Display runs per hover, at the edge, but per call it builds three vectors.

- `visible_fields` (line 446) takes the `Vec<Field>` that `self.fields(f.db)` made and filters it
  into a second one. After, every caller (lines 373, 396, 439, 579) fills the `Fields` shelf from
  the iterator form and passes a slice. New helper, replacing `visible_fields`:

```rust
/// The fields of `fields` that `f` shows, in `shown`, and whether any was hidden.
fn visible_fields<'db>(
    fields: impl Iterator<Item = Field>,
    shown: &mut Vec<Field>,
    f: &mut HirFormatter<'_, 'db>,
) -> bool {
    if f.render_private_fields() {
        shown.extend(fields);
        return false;
    }
    let mut hidden_fields = false;
    shown.extend(fields.filter(|field| {
        let is_public = field.visibility(f.db) == Visibility::Public;
        hidden_fields |= !is_public;
        is_public
    }));
    hidden_fields
}
```

  Each caller, for example line 396. Before:

```rust
                    let (fields, hidden_fields) = visible_fields(self.fields(f.db), f);
                    write_fields(&fields, hidden_fields, has_where_clause, limit, false, f)?;
```

  After:

```rust
                    crate::scratch::Fields::with(|fields| {
                        let hidden_fields = visible_fields(self.fields_iter(f.db), fields, f);
                        write_fields(fields, hidden_fields, has_where_clause, limit, false, f)
                    })?;
```

  The same at 373, 439 and 579 (each with the arguments it passes `write_fields` now; `self`
  is the `Struct`, `Union` or `EnumVariant` there, each of which has `fields_iter` from R6).
  Take `let db = f.db;` (a `&'db Db`) before the lend and call `self.fields_iter(db)`, so the
  iterator borrows the database and not `f`, which `visible_fields` and `write_fields` take
  mutably.
- `Enum`'s `write_variants(&self.variants(f.db), ..)` (line 418): before
  `write_variants(&self.variants(f.db), has_where_clause, limit, f)?;`, after:

```rust
            crate::scratch::Variants::with(|variants| {
                variants.extend(self.variants_iter(f.db));
                write_variants(variants, has_where_clause, limit, f)
            })?;
```

- Line 84 `write_params_bounds(f, &Vec::from_iter(predicates))` and line 641's
  `.collect::<Vec<_>>()` of `TypeParam`'s predicates hold `Clause<'db>`s. They go to a shelf of
  `Vec<Clause<'static>>` through `recycle`. In `scratch.rs`:

```rust
shelves! {
    /// Clauses `display` writes, kept as `'static` and lent as `'db` through [`recycle`].
    Clauses, CLAUSES: hir_ide::next_solver::Clause<'static>;
}

/// Lends the thread's clause buffer, empty, as `Clause<'db>`s for `f`.
pub(crate) fn with_clauses<'db, R>(
    f: impl FnOnce(&mut Vec<hir_ide::next_solver::Clause<'db>>) -> R,
) -> R {
    Clauses::with(|kept| {
        let mut lent = recycle(std::mem::take(kept));
        let result = f(&mut lent);
        *kept = recycle(lent);
        result
    })
}
```

  (`Clause` is the element type `write_params_bounds` and
  `write_bounds_like_dyn_trait_with_prefix` take a slice of; use the path the tree names it by
  in `hir_ide::next_solver`.) Line 84 becomes

```rust
        crate::scratch::with_clauses(|clauses| {
            clauses.extend(predicates);
            write_params_bounds(f, clauses)
        })?;
```

  and in `TypeParam::hir_fmt` (line 625) the `let predicates = ... .collect::<Vec<_>>();` and
  the `match` that reads `&predicates` go inside `crate::scratch::with_clauses(|predicates| {
  predicates.extend(/* the same iterator chain, without .collect */); match param_data { .. } })`,
  each `&predicates` becoming `predicates`.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R7 as written (`visible_fields` into a lent `Fields`, `Variants` for enums, `with_clauses` for the derive bounds and `TypeParam::hir_fmt`).

## R8. `attrs.rs`, `has_source.rs`, `from_id.rs`

Read in full for per-call buffers: `has_source.rs` and `from_id.rs` make none (conversions and
`source` lookups that return the database's nodes). `attrs.rs` makes one, at line 589, migrated
in R6. Nothing else to change.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:40): nothing to change; `attrs.rs:589` is on `fields_iter` (R6).

## R9. `SymbolCollector` on the query's frame (`symbols.rs`), the one query-run code here

`SymbolCollector` runs only inside ide-db's tracked `module_symbols`, `library_symbols` and
`extern_prelude_symbols` (`crates/ide-db/src/symbol_index.rs:374`, `:403`, `:422`), so its
temporaries are the run's and go to the frame those queries open. Its tag is `Tag::Nameres`
(contract 3: it reads def maps and item scopes only; the index is name-level).

| query (`ide-db/src/symbol_index.rs`) | tag | memo |
|---|---|---|
| `module_symbols` | `Tag::Nameres` | `SymbolIndex` over a `Box<[FileSymbol]>`, made once at size by `finish` |
| `library_symbols` | `Tag::Nameres` | the same |
| `extern_prelude_symbols` | `Tag::Nameres` | the same |

Temporaries (all per run):

| `symbols.rs` line | what | made | before | after |
|---|---|---|---|---|
| 97 `symbols` | the symbols so far, deduplicated, in insertion order | per run; grows per symbol | `FxIndexSet<FileSymbol<'db>>` | `order: Vec<FileSymbol<'db>, &'f Frame>` plus `seen: hashbrown::HashTable<u32, &'f Frame>` of indices into `order` |
| 98 `work` | the module worklist | per run; per module | `Vec<SymbolCollectorWork>` | `Vec<SymbolCollectorWork, &'f Frame>` |
| 255 `import_child_source_cache` | a `use` item's child sources, per module | a new map per module | `FxHashMap::default()` | one `hashbrown::HashMap<UseId, _, FxBuildHasher, &'f Frame>` on the collector, `clear`ed per module |

The struct and constructor. Before (line 95):

```rust
pub struct SymbolCollector<'db> {
    db: &'db Db,
    symbols: FxIndexSet<FileSymbol<'db>>,
    work: Vec<SymbolCollectorWork>,
    current_container_name: Option<Symbol>,
    collect_pub_only: bool,
}
```

After:

```rust
pub struct SymbolCollector<'db, 'f> {
    db: &'db Db,
    /// The symbols so far, first found first.
    order: Vec<FileSymbol<'db>, &'f Frame>,
    /// `order`'s indices, by the symbol's hash: a symbol found again is not pushed again.
    seen: hashbrown::HashTable<u32, &'f Frame>,
    work: Vec<SymbolCollectorWork, &'f Frame>,
    /// A `use` item's child sources, for the module being collected.
    import_child_sources: ImportChildSources<'f>,
    current_container_name: Option<Symbol>,
    collect_pub_only: bool,
}

type ImportChildSources<'f> = hashbrown::HashMap<
    hir_def::UseId,
    InFile<la_arena::ArenaMap<hir_def::item_tree::UseTreeIdx, ast::UseTree>>,
    rustc_hash::FxBuildHasher,
    &'f Frame,
>;
```

(the map's value type is what `i.use_.child_source(db)` returns at line 270; take it from
`HasChildSource for UseId` in hir-def's `src.rs` when applying: the key is `ImportId::use_`'s
type and the value `InFile<ArenaMap<Idx, ast::UseTree>>` or what the tree names.)

`new` and `new_module` take the frame:

```rust
    pub fn new(db: &'a Db, frame: &'f Frame, collect_pub_only: bool) -> Self {
        SymbolCollector {
            db,
            order: Vec::new_in(frame),
            seen: hashbrown::HashTable::new_in(frame),
            work: Vec::new_in(frame),
            import_child_sources: hashbrown::HashMap::with_hasher_in(
                rustc_hash::FxBuildHasher,
                frame,
            ),
            current_container_name: None,
            collect_pub_only,
        }
    }

    pub fn new_module(
        db: &'a Db,
        frame: &'f Frame,
        module: Module,
        collect_pub_only: bool,
    ) -> Box<[FileSymbol<'a>]> {
        let mut symbol_collector = SymbolCollector::new(db, frame, collect_pub_only);
        symbol_collector.collect(module);
        symbol_collector.finish()
    }
```

with `impl<'a, 'f> SymbolCollector<'a, 'f>` and `use blaze_mem::Frame;`.

Every `self.symbols.insert(sym)` / `this.symbols.insert(sym)` (lines 157, 170, 292 and those in
`push_decl`, `push_module` and the trait and impl walks: `rg -n 'symbols.insert' crates/hir/src/symbols.rs`)
becomes `self.push_symbol(sym)` / `this.push_symbol(sym)`, with:

```rust
    /// Pushes `symbol` unless it was found before: `FxIndexSet::insert`'s order and dedup.
    fn push_symbol(&mut self, symbol: FileSymbol<'a>) {
        let hash = rustc_hash::FxBuildHasher.hash_one(&symbol);
        let order = &self.order;
        if self.seen.find(hash, |&at| order[at as usize] == symbol).is_some() {
            return;
        }
        let at = u32::try_from(self.order.len()).expect("more than u32::MAX symbols in a module");
        self.seen.insert_unique(hash, at, |&at| {
            rustc_hash::FxBuildHasher.hash_one(&self.order[at as usize])
        });
        self.order.push(symbol);
    }
```

(`use std::hash::BuildHasher as _;` for `hash_one`. The borrow of `self.order` inside the rehash
closure while `self.seen` is borrowed mutably is two fields, which the closure captures
separately in edition 2021 and later.) If any caller reads the inserted flag
(`rg -n 'symbols.insert\(' crates/hir/src/symbols.rs` and check for a use of the result),
`push_symbol` returns `bool` (true when pushed).

`finish`. Before: `self.symbols.into_iter().collect()`. After (one allocation at size: a
`Vec::drain` is `TrustedLen`):

```rust
    pub fn finish(mut self) -> Box<[FileSymbol<'a>]> {
        self.order.drain(..).collect()
    }
```

`collect_from_module` (line 255). Before:

```rust
        // Nested trees are very common, so a cache here will hit a lot.
        let import_child_source_cache = &mut FxHashMap::default();
```

After:

```rust
        // Nested trees are very common, so a cache here will hit a lot.
        self.import_child_sources.clear();
```

and the `push_import` closure (line 264), which captures the cache and gets `this: &mut Self`,
reads `this.import_child_sources.entry(i.use_).or_insert_with(|| i.use_.child_source(this.db))`
instead of `import_child_source_cache.entry(..)`. Since the closure no longer captures the map,
`let mut push_import` becomes `let push_import` if nothing else it captures is mutable.

What still allocates in a run, and why: `child_source` builds its `ArenaMap` (hir-def's
`HasChildSource for UseId`, mem-db-def-items' slice) and `Symbol::intern` of a container name
interns once per new name. The result box is the memo (owned, exact size, contract 4).

The callers pass the frame (R11, ide-db).

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> applied by db-mem-def (21:40): R9 as written (`SymbolCollector<'db, 'f>` on the frame; `new(db, frame, ..)`, `new_module(db, frame, ..)`). ide-db's callers already pass the frame.

## R10. `SourceAnalyzer::resolve_path` re-lowers the path into a whole store (`source_analyzer.rs:1285`, `:1492`)

Per call, a path that inference did not resolve (every type path, `use` path and item path, and
expression paths that are not associated items or variants) is lowered again:
`ExprCollector::new(db, module, file_id, LoweringMode::Ide)`, `lower_path` (twice, for the
parent path), then `collector.store.finish()`, which makes an owned `ExpressionStore` with its
arenas and side tables, only so that `resolve_hir_path_` can lower a type anchor
(`<T as Trait>::x`) through `TyLoweringContext`. The same in `resolve_hir_path_per_ns`
(line 1492).

The store is read only for a path's type anchor (`resolve_hir_path_`, line 1884:
`path.type_anchor()`, then `store?`) and by `resolve_hir_path_qualifier` for a use tree's
qualifier (line 2087). Two changes, the first in hir-def's `expr_store` (mem-format's files,
held by speed-hir-store; asked there, see below), the second here:

1. **Asked of the `expr_store` holder** (speed-hir-store and mem-format-hir report DONE, so
   sent to their parent `mem-format` by tmux on 2026-10-29): with
   mem-format-hir's pooled builder scratch (`v2/requests/mem-format-hir.md`, the first section),
   an `ExpressionStoreBuilder` dropped without `finish` must give its scratch back to the pool
   (a `Drop`, or `ExprCollector::discard(self)`), and `ExprCollector::new` plus `lower_path` of a
   path with no generic arguments must allocate nothing on warm scratch. Then a path lowered only
   to be resolved costs no allocation.

> applied by mem-format (21:40), part 1: `impl Drop for ExpressionStoreBuilder` gives an unfinished
> builder's scratch back to the pool. `finish` and `Drop` share `take_scratch`, and an empty set is
> not pooled, where it would shadow a grown one. The collector's eight stacks (`path_segments`,
> `path_generic_args`, `generic_args`, `assoc_bindings`, `fn_params`, `closure_params`,
> `label_ribs`, `unowned_bindings`) are now one `Stacks` taken from a per-thread pool and given
> back on drop. Before, they were fresh `Vec`s per collector, and a plain path's first segment
> allocated. Tests: `an_unfinished_builder_gives_its_scratch_back` (expr_store.rs) and
> `a_warm_plain_path_lookup_allocates_nothing` (lower/path/tests.rs: `ExprCollector::new` with
> `LoweringMode::Ide`, `lower_path("a::b::C")`, dropped, budget 0). `SEGMENT_LOWERING_MAP`'s
> test-only recording is opt-in (`RECORD_SEGMENTS`), so it doesn't count against the budget. If
> the salsa reads in `with_maps` (`local_def_map`, `cfg_options`, `span_map`) allocate on a
> memo hit, the test names them. Nothing built. Part 2 (`finish` only when the store is read)
> is crates/hir's to apply.
2. **Here**: `finish` only when the store is read. In `resolve_path` (lines 1285 to 1293).
   Before:

```rust
        let mut collector =
            ExprCollector::new(db, self.resolver.module(), self.file_id, LoweringMode::Ide);
        let hir_path =
            collector.lower_path(path.clone(), &mut ExprCollector::impl_trait_error_allocator)?;
        let parent_hir_path = path
            .parent_path()
            .and_then(|p| collector.lower_path(p, &mut ExprCollector::impl_trait_error_allocator));
        let (store, _) = collector.store.finish();
```

   After:

```rust
        let mut collector =
            ExprCollector::new(db, self.resolver.module(), self.file_id, LoweringMode::Ide);
        let hir_path =
            collector.lower_path(path.clone(), &mut ExprCollector::impl_trait_error_allocator)?;
        let parent_hir_path = path
            .parent_path()
            .and_then(|p| collector.lower_path(p, &mut ExprCollector::impl_trait_error_allocator));
        // The store is read only to lower a type anchor and a use tree's qualifier.
        let reads_store = hir_path.type_anchor().is_some()
            || parent_hir_path.as_ref().is_some_and(|it| it.type_anchor().is_some())
            || parent().and_then(ast::UseTree::cast).is_some();
        let store = if reads_store { Some(collector.store.finish().0) } else { None };
```

   and each later `&store` / `Some(&store)` in the function becomes `store.as_ref()` where an
   `Option<&ExpressionStore>` is taken (the calls to `resolve_hir_path_`), and
   `store.as_ref().expect("a use tree's qualifier lowers its store")` at
   `resolve_hir_path_qualifier`'s call (line 1300), which `reads_store` covers. If `hir_path`
   borrows the collector (it is `Path<'a>` in the tree, `crates/hir-def/src/expr_store/path.rs:86`),
   the applier keeps the paths' lifetimes as they compile today: what changes is only whether
   `finish` runs. `resolve_hir_path_per_ns` (line 1492) the same, with
   `reads_store = hir_path.type_anchor().is_some()`.

   Before applying, check every other read of `store` in `resolve_path` between line 1293 and
   1484 (`rg -n 'store' crates/hir/src/source_analyzer.rs`): each must be under one of the three
   conditions, or the condition grows to cover it.

The `Resolver` a `SourceAnalyzer` holds is built per `analyze` (`resolver_for_scope` and kin
push onto `Resolver::scopes`, a `Vec<Scope>`), and `SemanticsScope::can_use_trait_methods`
(`semantics.rs:2788`) builds `traits_in_scope`'s `FxHashSet` per call, and
`process_all_names` (`:2757`) builds `names_in_scope`'s map. Those are `hir-def/src/resolver.rs`,
mem-db-def-resolve's slice; asked there:

- **mem-db-def-resolve** answered in `v2/requests/mem-db-def-resolve.md`, "Requests to callers
  outside the slice", "To db and mem-db-def-hir". Apply its text as written there; it supersedes
  what this file asked:
  - `can_use_trait_methods` (2788) becomes `self.resolver.has_trait_in_scope(self.db, t.id)`.
  - `process_all_names` (2757) fills a per-thread `resolver::NamesInScope`, taken rather than
    borrowed, through `names_in_scope_into`.
  - `Resolver::scopes` as `SmallVec<[Scope<'db>; 8]>` is filed in `v2/requests/db-expand.md` §2
    (speed-hir-sol-nameres).

  Its **R9h** is also for this crate: `SourceToDefCtx::to_def` (`semantics/source_to_def.rs:482`)
  gains the `where keys::AstPtrPolicy<Ast, ID>: Policy<K = AstPtr<Ast>, V = ID>` bound and the
  `dyn_map::Policy` import, for the `DynMap` rewritten as a struct of typed maps. It touches a
  function that R2 leaves alone, so the two apply in either order. R2's `Carry` holds `DynMap`s
  by value and needs only `Default` on them, which R9 keeps.

  The span map's `ranges_with_span` vector (`map_range_down`'s source, hir-expand) is
  mem-format's: `v2/requests/mem-format*.md`.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).


> not applied by db-mem-def (21:46): part 2 waits on format. Since paths became columns, `resolve_path` and `resolve_hir_path_per_ns` read `hir_path` through `store.path_at(..)` after `finish`, so the store is read on every call, and hir can't read a path from an unfinished builder (`ExpressionStoreBuilder::path_at` is `pub(crate)`, `crates/hir-def/src/expr_store/path.rs:1304`). Filed in `v2/requests/db.md` ("mem-db-def: format, `ExpressionStoreBuilder::path_at` public"). Once it is `pub`, read both paths from the builder, compute `reads_store` from them, and `finish` only when it is set. Part 1 is mem-format's, applied. The resolver half (`can_use_trait_methods`, `process_all_names`) is applied, see mem-db-def-resolve.md.
> applied by db-audit-mem (22:42): part 2, now that `ExpressionStoreBuilder::path_at` is `pub` (crates/hir-def/src/expr_store/path.rs:1304): `resolve_path` and `resolve_hir_path_per_ns` (crates/hir/src/source_analyzer.rs) read the paths from the builder and `finish` only when `hir_path` has a type anchor (`Either::Left(store)` / `Either::Right(builder)`, paths through `path_at` from either). Differs: `reads_store` is the type anchor alone, because `resolve_hir_path_qualifier` and `resolve_hir_path_` read the store only there; so `resolve_hir_path_qualifier` takes `Option<&ExpressionStore>` (`store?` in its anchor arm, as `resolve_hir_path_` does) in place of an `expect` at the use-tree call. Its three calls in `source_analyzer/view.rs` are frontend-tree-move-hir-next1's, told (they need `Some(&store)` or the same lazy finish).

## R10b. `Crate::query_external_importables` without a set per search (`lib.rs:376`)

Per completion request, `import_map::search_dependencies` builds a fresh
`FxHashSet<(ItemInNs, Complete)>`, and the function hands its items out in that set's
iteration order (`ide-db/src/items_locator.rs:131` chains them into completion without sorting).
mem-db-def-items offers `search_dependencies_into(db, krate, query, &mut set)`
(`v2/requests/mem-db-def-items.md` I6.2). Filling a per-worker set and draining it would be
nondeterministic, though: a reused hashbrown table iterates in an order set by the capacity it
grew to on earlier requests, so the same search would list completions in a different order
depending on what the thread searched before.

So the items come out in the order the search finds them (dependencies in order, then the
map's runs in key order, the first occurrence of each), which is deterministic and does not
depend on the buffers' history. The set only dedups.

- **Asked of mem-db-def-items** (window `mem-db-def-items-next1`): beside
  `search_dependencies_into`, the visitor it is built from, with the loop of I6.2 unchanged and
  `found.insert(..)` becoming `f(..)`. **Taken** (2026-10-29): it is in
  `v2/requests/mem-db-def-items.md` I6.2, with `search_dependencies_into` built on it. Its order
  is the dependencies in order, then each one's runs in key order. Its test is
  `each_is_free_and_in_a_fixed_order` (I12.3). The body below needs it to have landed first.

```rust
/// Calls `f` on each importable name of `krate`'s dependencies that matches `query`, in the
/// order the search finds them; an item can come more than once.
pub fn search_dependencies_each(
    db: &Db,
    krate: Crate,
    query: &Query,
    mut f: impl FnMut(ItemInNs, Complete),
) {
    /* I6.2's body, with `found.insert((map.items[row], info.complete));` as
       `f(map.items[row], info.complete);` */
}

pub fn search_dependencies_into(
    db: &Db,
    krate: Crate,
    query: &Query,
    found: &mut FxHashSet<(ItemInNs, Complete)>,
) {
    search_dependencies_each(db, krate, query, |item, complete| {
        found.insert((item, complete));
    });
}
```

- **Here.** In `scratch.rs` (R1), one more shelf and a by-copy drain:

```rust
shelves! {
    /// `query_external_importables`' items, first found first.
    Importables, IMPORTABLES: (hir_def::item_scope::ItemInNs, hir_def::Complete);
}

thread_local! {
    /// The items `query_external_importables` has handed out, to skip one found again.
    static IMPORTABLES_SEEN: RefCell<rustc_hash::FxHashSet<(hir_def::item_scope::ItemInNs, hir_def::Complete)>> =
        RefCell::new(rustc_hash::FxHashSet::default());
}

/// Takes the thread's seen-set, empty: taken rather than borrowed, so a search inside a search
/// gets one of its own.
pub(crate) fn take_importables_seen()
-> rustc_hash::FxHashSet<(hir_def::item_scope::ItemInNs, hir_def::Complete)> {
    IMPORTABLES_SEEN.try_with(|it| std::mem::take(&mut *it.borrow_mut())).unwrap_or_default()
}

/// Gives the set back, emptied, keeping its capacity.
pub(crate) fn give_importables_seen(
    mut seen: rustc_hash::FxHashSet<(hir_def::item_scope::ItemInNs, hir_def::Complete)>,
) {
    seen.clear();
    let _ = IMPORTABLES_SEEN.try_with(|it| {
        let mut slot = it.borrow_mut();
        if slot.capacity() < seen.capacity() {
            *slot = seen;
        }
    });
}

/// The items of a lent buffer of `Copy` items, first to last, the buffer going back to its
/// shelf when this is dropped.
pub(crate) struct LentCopied<B: Shelved, T> {
    buffer: scratch::Lent<B>,
    front: usize,
    _item: std::marker::PhantomData<fn() -> T>,
}

impl<B: Shelved + Deref<Target = Vec<T>>, T: Copy> LentCopied<B, T> {
    pub(crate) fn new(buffer: scratch::Lent<B>) -> Self {
        LentCopied { buffer, front: 0, _item: std::marker::PhantomData }
    }
}

impl<B: Shelved + Deref<Target = Vec<T>>, T: Copy> Iterator for LentCopied<B, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        let item = *self.buffer.get(self.front)?;
        self.front += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.buffer.len() - self.front;
        (left, Some(left))
    }
}
```

(use the paths of `ItemInNs` and `Complete` that `lib.rs` already imports; `hir_def::Complete`
is `crates/hir-def/src/lib.rs:1713`, `ItemInNs` is `item_scope.rs:2128`, both `Copy`.)

`query_external_importables` (`lib.rs:376`). Before:

```rust
        let _p = tracing::info_span!("query_external_importables").entered();
        import_map::search_dependencies(db, self.into(), &query).into_iter().map(
            |(item, do_not_complete)| {
```

After (the `map` closure unchanged):

```rust
        let _p = tracing::info_span!("query_external_importables").entered();
        let mut found = blaze_mem::scratch::lend::<crate::scratch::Importables>();
        let mut seen = crate::scratch::take_importables_seen();
        import_map::search_dependencies_each(db, self.into(), &query, |item, complete| {
            if seen.insert((item, complete)) {
                found.push((item, complete));
            }
        });
        crate::scratch::give_importables_seen(seen);
        crate::scratch::LentCopied::new(found).map(
            |(item, do_not_complete)| {
```

The return type stays `impl Iterator<Item = (Either<ModuleDef, Macro>, Complete)>`: the lent
buffer is `'static` and goes back to the shelf of the thread that drops the iterator.

The order change is the one visible effect: completion's external candidates come in search
order instead of hash order. If an ide-db or ide-completion snapshot test lists them unsorted,
its expectation changes once, to an order that no longer depends on the hash table.

Budget test, added to `tests/budget.rs` (R12):

```rust
#[test]
fn a_warm_completion_search_allocates_nothing() {
    let db = TestDB::with_files(
        r#"
//- /main.rs crate:main deps:dep
fn main() {}
//- /dep.rs crate:dep
pub struct Needle;
pub fn needle_fn() {}
pub mod needles { pub struct Inner; }
"#,
    );
    crate::attach_db(&db, || {
        let main = Crate::all(&db)
            .into_iter()
            .find(|it| it.display_name(&db).is_some_and(|it| it.crate_name().as_str() == "main"))
            .expect("the crate `main`");
        let search = |query: hir_def::import_map::Query| {
            main.query_external_importables(&db, query).count()
        };
        let first = search(hir_def::import_map::Query::new("needle".to_owned()));
        assert!(first >= 2, "`Needle` and `needle_fn` are found");
        assert_eq!(search(hir_def::import_map::Query::new("needle".to_owned())), first);
        // `Query::new` lowercases into a `String`: the request's edge, made outside the budget.
        let query = hir_def::import_map::Query::new("needle".to_owned());
        let again = assert_at_most(0, "a warm completion search", || search(query));
        assert_eq!(again, first);
        // The same items in the same order on every search.
        let order = |db: &TestDB| {
            main.query_external_importables(db, hir_def::import_map::Query::new("needle".to_owned()))
                .map(|(item, _)| item)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&db), order(&db));
    });
}
```

(`import_map` is `hir_def::import_map`, as `lib.rs:196` imports it; `display_name` and
`crate_name` are `Crate`'s, `symbols.rs:142` uses them. The search must also be zero inside
hir-def's `search_runs`, which is mem-db-def-items' I6.2 budget.)

> forwarded to db (db-next2, `crates/hir/`) (requests, pass 19; told by tmux).


> not applied by db-mem-def (21:40): R10b waits on items' `search_dependencies_each` (`crates/hir-def/src/import_map.rs`), not in the tree.
> handed to db-audit-mem (22:38): `path_at` (`expr_store/path.rs:1304`) and `search_dependencies_each` (`import_map.rs:681`) are in, and db-audit-mem is applying R10 part 2 and R10b in `source_analyzer.rs`, hir `lib.rs`, `scratch.rs` and `tests/budget.rs`. db-mem-def is in none of them.
> applied by db-audit-mem (22:42): as written, `search_dependencies_each` being in (crates/hir-def/src/import_map.rs:681): crates/hir/src/scratch.rs gains the `Importables` shelf, `IMPORTABLES_SEEN` with `take_`/`give_importables_seen` (through an `ImportablesSeen` alias) and `LentCopied`; crates/hir/src/lib.rs `query_external_importables` fills them in search order; `a_warm_completion_search_allocates_nothing` in crates/hir/src/tests/budget.rs. db-mem-def's `// alloc:` lines in scratch.rs kept.

## R11. Callers outside `crates/hir/` (owners from `docs/v2/plan/reference/crates.md`)

- **ide-db** (coordinator; `crates/ide-db/` until it is removed at integration):
  `symbol_index.rs:374`, `:403`, `:422` make the collector inside `module_symbols`,
  `library_symbols`, `extern_prelude_symbols`. Each body opens the run's frame at its top and
  passes it: `blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| { ... SymbolCollector::new(db, frame, true) ... })`,
  and `SymbolCollector::new_module(db, frame, module, false)`. The tests at `:719` and `:746`
  run outside a query, so they open a task: `blaze_mem::frame::task(blaze_mem::Tag::Nameres,
  |frame| SymbolCollector::new_module(&db, frame, module_id, false))`. ide-db gains
  `blaze-mem.workspace = true`. The iterator forms of R6 for ide-db's iterate-only callers of
  `declarations`, `impl_defs` and `variants` (`rg -n '\.(declarations|impl_defs|variants)\(' crates/ide-db/src`).
- **diagnostics** (`crates/blaze-diagnostics/`, `crates/ide-diagnostics/`), **codegen**
  (`crates/blaze-engine/`, `crates/hir-ty-codegen/`, `crates/hir-ty-mir/`) and the coordinator's
  `blaze-doc`, `blaze-wasm`, `blaze-lints`, and **items** (`crates/blaze-items/`): each call of
  `Module::declarations`/`impl_defs`/`legacy_macros`, `Struct`/`Union`/`EnumVariant`/`Variant::fields`,
  `Enum::variants`, `Trait::items`, `Impl::items`, `Crate::modules` whose result is only
  iterated (a `for` loop, `.into_iter()` or `.iter()` then adapters, no index, no `len`, no second
  pass) moves to the `_iter` form. Found with
  `rg -n '\.(declarations|impl_defs|legacy_macros|variants|items|modules)\(|\.fields\(' crates/<crate>/src`,
  then keeping only the calls on `hir` types (hir-def's `VariantId::fields` and others return
  references already). On 2026-10-29 that `rg` over the hir-dependent crates finds 126 lines.

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).
> applied (the ide-db bullet; ide-db is requests'): `symbol_index.rs`' `library_symbols`, `module_symbols` and `extern_prelude_symbols` open the run's frame with `blaze_mem::frame::current` and pass it to `SymbolCollector::new`/`new_module`; the two tests open `frame::task(Tag::Nameres, ..)`; ide-db already depends on blaze-mem. Its iterate-only `declarations`/`impl_defs`/`variants` callers: none (the two `variants` calls match a slice). R9's `Tag::Nameres` on the three queries waits on salsa's `mem_tag` (mem-db-salsa M2; `vendor/salsa*` doesn't take it yet) (requests, pass 23).


> filed by db-mem-def (21:40): ide-db's `SymbolCollector` callers are in already; the iterate-only callers' move to the `_iter` forms is filed in `v2/requests/db.md` ("mem-db-def: iterator forms of hir's accessors") under the coordinator, diagnostics, codegen and items.

## R12. Budget tests (`crates/hir/src/tests/budget.rs`, and `mod budget;` in `tests.rs`)

In `crates/hir/src/tests.rs`, after `mod api;` (line 1): `mod budget;`. The counting allocator
is mem-db-def's H3 (`#[cfg(test)] #[global_allocator] static ALLOC:
blaze_alloc_count::Counting` in `src/lib.rs`). Counts are per thread; every measured call runs
on the test's thread (no `run_all` inside these).

The whole file:

```rust
//! Allocation budgets of `Semantics`' hit paths and the accessors' iterator forms: none once
//! the thread's buffers have grown and the database is warm.

use base_db::EditionedFileId;
use blaze_alloc_count::assert_at_most;
use syntax::{AstNode, SyntaxKind, SyntaxNodePtr, TextRange, ast, view};
use test_fixture::WithFixture;

use super::db::TestDB;
use crate::{Adt, Crate, ModuleDef, Semantics, Variant};

const FIXTURE: &str = r#"
//- /main.rs crate:main
macro_rules! id { ($($t:tt)*) => { $($t)* }; }
mod inner {
    pub struct Point { pub x: u32, pub y: u32 }
    pub enum Shape { Dot(Point), Line(Point, Point), Empty }
    pub trait Area { fn area(&self) -> u32; }
    impl Area for Point { fn area(&self) -> u32 { 0 } }
}
use inner::{Point, Shape};
fn origin() -> Point { Point { x: 0, y: 0 } }
fn main() {
    id! { let p = origin(); }
    let _s = Shape::Dot(p);
}
"#;

/// What the tests ask `Semantics` about, found once through the file's views (outside every
/// measured closure) and kept as ranges and pointers, which outlive the views.
struct Targets {
    origin: TextRange,
    p_in_macro: TextRange,
    dot_path: SyntaxNodePtr,
    point_ty: SyntaxNodePtr,
    strukt: SyntaxNodePtr,
    field: SyntaxNodePtr,
    module: SyntaxNodePtr,
}

/// Whether `path`'s last segment is the identifier `text`.
fn ends_in(path: view::Path<'_>, text: &str) -> bool {
    path.segment()
        .and_then(|it| it.name_ref())
        .and_then(|it| it.ident_token())
        .is_some_and(|it| it.text() == text)
}

fn targets(db: &TestDB, file_id: EditionedFileId) -> Targets {
    use view::AstNode as _;
    let parse = file_id.parse(db);
    let root = parse.root();
    let ident = |at: view::Node<'_>, text: &str| {
        at.descendants_with_tokens()
            .filter_map(|it| it.into_token())
            .find(|it| it.kind() == SyntaxKind::IDENT && it.text() == text)
            .unwrap_or_else(|| panic!("no identifier `{text}` in the fixture"))
            .text_range()
    };
    let first = |what: &str, found: Option<view::Node<'_>>| {
        view::ptr(found.unwrap_or_else(|| panic!("no {what} in the fixture")))
    };
    let token_tree = root
        .descendants()
        .find_map(view::MacroCall::cast)
        .and_then(|call| call.token_tree())
        .expect("the call `id!`");
    let strukt = root.descendants().find_map(view::Struct::cast).expect("`struct Point`");
    Targets {
        origin: ident(root, "origin"),
        p_in_macro: ident(token_tree.syntax(), "p"),
        dot_path: first(
            "path `Shape::Dot`",
            root.descendants()
                .filter_map(view::Path::cast)
                .find(|it| ends_in(*it, "Dot") && it.qualifier().is_some())
                .map(|it| it.syntax()),
        ),
        point_ty: first(
            "return type `Point`",
            root.descendants()
                .filter_map(view::PathType::cast)
                .find_map(|it| it.path())
                .map(|it| it.syntax()),
        ),
        strukt: view::ptr(strukt.syntax()),
        field: first(
            "field `x`",
            strukt.syntax().descendants().find(|it| view::RecordField::can_cast(it.kind())),
        ),
        module: first(
            "`mod inner`",
            root.descendants().find(|it| view::Module::can_cast(it.kind())),
        ),
    }
}

// `Semantics`' API still takes handles, so each test turns a target into one at the call and
// nowhere else: `token_at` and `node_at` go when frontend-tree moves `Semantics` to views.

fn token_at(file: &ast::SourceFile, range: TextRange) -> syntax::SyntaxToken {
    file.syntax().covering_element(range).into_token().expect("a token's range covers a token")
}

fn node_at<N: AstNode>(file: &ast::SourceFile, ptr: SyntaxNodePtr) -> N {
    N::cast(ptr.to_node(file.syntax())).expect("a pointer's kind is the node's")
}

/// Runs `f` once to warm the database, the thread's shelves and the carried `Semantics`
/// tables, then again under a budget of no allocation, each time with a fresh `Semantics` (a
/// request), as an IDE does.
fn warm_then_free<R>(db: &TestDB, what: &str, mut f: impl FnMut(&Semantics<'_>) -> R) {
    {
        let sema = Semantics::new(db);
        drop(f(&sema));
    }
    {
        let sema = Semantics::new(db);
        drop(f(&sema));
    }
    let sema = assert_at_most(0, &format!("{what}: making a `Semantics`"), || Semantics::new(db));
    let result = assert_at_most(0, what, || f(&sema));
    drop(result);
    assert_at_most(0, &format!("{what}: dropping the `Semantics`"), || drop(sema));
}

#[test]
fn descend_into_macros_on_a_warm_file_allocates_nothing() {
    let (db, file_id) = TestDB::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let at = targets(&db, file_id);
        warm_then_free(&db, "descend_into_macros", |sema| {
            let file = sema.parse(file_id);
            let origin = sema.descend_into_macros(token_at(&file, at.origin));
            assert!(!origin.is_empty());
            let in_macro = token_at(&file, at.p_in_macro);
            let mut found = 0u32;
            sema.descend_into_macros_cb(in_macro, |_, _| found += 1);
            assert_eq!(found, 1, "`p` maps to one token of the expansion");
        });
    });
}

#[test]
fn resolve_path_through_semantics_on_a_warm_file_allocates_nothing() {
    let (db, file_id) = TestDB::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let at = targets(&db, file_id);
        warm_then_free(&db, "resolve_path", |sema| {
            let file = sema.parse(file_id);
            let path: ast::Path = node_at(&file, at.dot_path);
            assert!(sema.resolve_path(&path).is_some(), "`Shape::Dot` resolves");
            let ty_path: ast::Path = node_at(&file, at.point_ty);
            assert!(sema.resolve_path(&ty_path).is_some(), "`Point` resolves");
        });
    });
}

#[test]
fn to_def_on_a_cached_node_allocates_nothing() {
    let (db, file_id) = TestDB::with_single_file(FIXTURE);
    crate::attach_db(&db, || {
        let at = targets(&db, file_id);
        warm_then_free(&db, "to_def", |sema| {
            let file = sema.parse(file_id);
            let strukt: ast::Struct = node_at(&file, at.strukt);
            let def = sema.to_struct_def(&strukt).expect("`Point` has a def");
            let field: ast::RecordField = node_at(&file, at.field);
            assert!(sema.to_def(&field).is_some(), "`x` has a def");
            let module: ast::Module = node_at(&file, at.module);
            assert!(sema.to_module_def(&module).is_some(), "`inner` has a def");
            def
        });
    });
}

#[test]
fn the_accessors_iterator_forms_allocate_nothing() {
    let db = TestDB::with_files(FIXTURE);
    crate::attach_db(&db, || {
        let krate = Crate::all(&db).into_iter().next().expect("a crate");
        let root = krate.root_module(&db);
        let inner = root.children(&db).next().expect("`mod inner`");
        let point = inner
            .declarations(&db)
            .into_iter()
            .find_map(|it| match it {
                ModuleDef::Adt(Adt::Struct(it)) => Some(it),
                _ => None,
            })
            .expect("`Point`");
        let shape = inner
            .declarations(&db)
            .into_iter()
            .find_map(|it| match it {
                ModuleDef::Adt(Adt::Enum(it)) => Some(it),
                _ => None,
            })
            .expect("`Shape`");
        let area = inner
            .declarations(&db)
            .into_iter()
            .find_map(|it| match it {
                ModuleDef::Trait(it) => Some(it),
                _ => None,
            })
            .expect("`Area`");
        // Warm: every table read below is computed once here.
        let warm = |db: &TestDB| {
            (
                root.children(db).count(),
                inner.declarations_iter(db).count(),
                inner.impl_defs_iter(db).count(),
                inner.legacy_macros_iter(db).count(),
                point.fields_iter(db).count(),
                Variant::Struct(point).fields_iter(db).count(),
                shape.variants_iter(db).map(|v| v.fields_iter(db).count()).sum::<usize>(),
                area.items_iter(db).count(),
                inner.impl_defs_iter(db).map(|it| it.items_iter(db).count()).sum::<usize>(),
                krate.modules_iter(db).count(),
            )
        };
        let expected = warm(&db);
        let (children, decls, impls, _legacy, fields, variant_fields, shape_fields, items, impl_items, modules) =
            expected;
        assert_eq!((children, decls, impls), (1, 3, 1), "`inner` declares Point, Shape, Area");
        assert_eq!((fields, variant_fields, shape_fields), (2, 2, 3));
        assert_eq!((items, impl_items, modules), (1, 1, 2));
        let again = assert_at_most(0, "the accessors' iterator forms", || warm(&db));
        assert_eq!(again, expected);
        // The `Vec` forms are the same items, in the same order.
        assert_eq!(inner.declarations_iter(&db).collect::<Vec<_>>(), inner.declarations(&db));
        assert_eq!(point.fields_iter(&db).collect::<Vec<_>>(), point.fields(&db));
        assert_eq!(shape.variants_iter(&db).collect::<Vec<_>>(), shape.variants(&db));
        assert_eq!(area.items_iter(&db).collect::<Vec<_>>(), area.items(&db));
    });
}

#[test]
fn kmerge2_is_kmerge_by() {
    use itertools::Itertools as _;
    let cases: [(&[u32], &[u32]); 6] =
        [(&[], &[]), (&[1, 3], &[]), (&[], &[2]), (&[1, 3, 5], &[2, 3, 4]), (&[2, 2], &[2, 2]), (&[5], &[1, 2, 9])];
    for (left, right) in cases {
        // Tag each item with its side, so a tie's order is checked too.
        let tagged = |side: u8, items: &'static [u32]| items.iter().map(move |&it| (it, side));
        let less = |a: &(u32, u8), b: &(u32, u8)| a.0 < b.0;
        let ours: Vec<_> =
            crate::semantics::kmerge2([tagged(0, left), tagged(1, right)].into_iter(), less).collect();
        let theirs: Vec<_> = [tagged(0, left), tagged(1, right)].into_iter().kmerge_by(less).collect();
        assert_eq!(ours, theirs, "{left:?} and {right:?}");
    }
}

#[test]
fn recycle_keeps_the_allocation() {
    let mut kept: Vec<&'static str> = Vec::with_capacity(16);
    kept.push("x");
    let ptr = kept.as_ptr().cast::<u8>();
    let local = String::from("y");
    let mut lent: Vec<&str> = assert_at_most(0, "recycle", || crate::scratch::recycle(kept));
    assert_eq!(lent.as_ptr().cast::<u8>(), ptr, "the same allocation");
    assert!(lent.capacity() >= 16 && lent.is_empty());
    lent.push(&local);
    let back: Vec<&'static str> = assert_at_most(0, "recycle back", || crate::scratch::recycle(lent));
    assert_eq!(back.as_ptr().cast::<u8>(), ptr);
}
```

For `kmerge2_is_kmerge_by`, `kmerge2` is `pub(crate)` in `semantics.rs` (change `fn kmerge2`
in R4 to `pub(crate) fn kmerge2`, and `struct KMerge2` to `pub(crate) struct KMerge2`) and
`semantics` is reachable as `crate::semantics` (it is `mod semantics;` in `lib.rs`, so
`crate::semantics::kmerge2` resolves within the crate). In `kmerge2_is_kmerge_by` the slices
are `&'static` literals, so `tagged`'s `'static` bound holds.

What each holds:

- `descend_into_macros`: R1, R2 (carried expansion storage, macro call cache, dyn maps), R3. A
  fresh `Semantics` per request, as an IDE makes them.
- `resolve_path`: R2, R10 (both parts) and mem-db-def-resolve's `Resolver::scopes`. Until those
  are applied this test fails with the count, which names what is left.
- `to_def`: R2 (the carried `DynMap`s; `find_container`'s walk allocates nothing on the flat
  tree, `crates/syntax-tree/src/cursor.rs`).
- the accessors: R6.

`warm_then_free` runs the body twice before measuring: the first run computes the database's
memos and grows the shelves; the second fills the carried tables at their final size, since the
first `Semantics` of a thread finds no carry (a `Semantics` made at another revision, or the
first on a thread, rebuilds them: that is a miss, not held to 0).

> forwarded to db (`crates/hir/`) (from requests, pass 17; told by tmux).

> applied by db-mem-def (21:40): R12, `crates/hir/src/tests/budget.rs` as written and `mod budget;` after `mod api;`. `resolve_path_through_semantics_on_a_warm_file_allocates_nothing` holds R10 too, which is not applied yet.
