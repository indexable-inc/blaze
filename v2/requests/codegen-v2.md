
## dedyn: crates/hir-ty-codegen/v2 and crates/blaze-codegen: the `dyn` sites (from dedyn-codegen, 2026-10-29)

> applied by dedyn-codegen (2026-10-29): dedyn-v2 1 to 5 as written (`HashSource` with `v2/by_id.rs` and its test, fold `Names` with its test, the comments); hot_lint finds no `dyn` in hir-ty-codegen or blaze-codegen. Not built: check it builds and the tests pass.

From `dedyn-codegen` (child of lane `dedyn`, report `/tmp/blaze-lanes/v2-dedyn-codegen.md`). Every
`dyn` in these paths outside tests, by what it becomes. Anchored on code text: line numbers are as
of 2026-10-29 21:30 and move. Nothing changes behavior. hot_lint accepts a `// dyn:` on the line or
the line above.

> forwarded to codegen (codegen-v2-next1) (requests, pass 20; told by tmux).

### dedyn-v2 1: `blaze-codegen/src/driver.rs`: `&dyn Lookup` becomes `&impl Lookup`

Three implementations: `blaze_cas::Store`, `blaze-cargo`'s `Reading` (the one a build uses) and
the tests' `Store`. Every caller passes a concrete one (`&store` in `blaze-cargo`
`serve/unit/codegen.rs`, `&Store::default()` / `store: &Store` in `driver/tests.rs`). `Build<B>`
is generic over its backend already and production has one backend and one `Lookup`, so each
function gets one production instance; nothing is copied. Asked per template, summary, masked
shape and static read.

Replace each `store: &dyn Lookup,` (in `Out::read`, `Build::run`, `Build::folded`,
`Build::release`) with `store: &impl Lookup,`, and

```rust
    pub fn tier_up(&self, store: &dyn Lookup) -> Result<TieredUp, Vec<CodegenError>> {
    fn dev(&self, nodes: &Sorted<InstanceKey, FnNode<B>>, store: &dyn Lookup, out: &mut Out) {
    fn statics(&self, statics: &Sorted<StaticId, StaticNode<B>>, store: &dyn Lookup, out: &mut Out) {
```

with

```rust
    pub fn tier_up(&self, store: &impl Lookup) -> Result<TieredUp, Vec<CodegenError>> {
    fn dev(&self, nodes: &Sorted<InstanceKey, FnNode<B>>, store: &impl Lookup, out: &mut Out) {
    fn statics(&self, statics: &Sorted<StaticId, StaticNode<B>>, store: &impl Lookup, out: &mut Out) {
```

No caller changes (`out.read(store, ..)` passes the `&impl Lookup` on; nobody turbofishes `read`).

### dedyn-v2 2: `blaze-codegen/src/dedupe.rs`, `memo.rs`: the predicates are generics

`dedupe` has one production caller (`driver.rs`, `dedupe(&mut out.functions, &mergeable)`), and
`Memo::retain` only its test so far (the daemon's collection will be one); `&F` is itself an `Fn`, so the callers
(`&mergeable`, `&|it| *it != key(9)`, `&|shape| ..`) are unchanged. `mergeable` is asked once per
function per round.

```rust
    mergeable: &dyn Fn(&InstanceKey) -> bool,
```
becomes
```rust
    mergeable: impl Fn(&InstanceKey) -> bool,
```
and
```rust
    pub fn retain(&mut self, kept: &dyn Fn(&ContentId) -> bool) {
```
becomes
```rust
    pub fn retain(&mut self, kept: impl Fn(&ContentId) -> bool) {
```

(`imports.rs`'s `closed: &dyn Fn(&InstanceKey) -> bool` is in the test module `model`: left.)

### dedyn-v2 3: `v2`: `Hashes` is held as an enum, `HashSource`

`Hashes` has one production implementation, `ItemHashes`, and one test implementation, `ById`
(written twice, in `v2/tests.rs` and `v2/refs/tests.rs`). `HirFrontend` holds an enum of the two,
and every `&dyn Hashes` becomes `&HashSource`: without `cfg(test)` it has one variant and the
`match` is no branch at all. A generic `H: Hashes` was the other choice; it would put a type
parameter on `HirFrontend` and, through `Prepare::Frontend`, on `Backends`. `Hashes` is asked
per type, constant, span and item of every function described and prepared.

**`v2.rs`**: beside `#[cfg(test)] mod tests;` add

```rust
#[cfg(test)]
pub(crate) mod by_id;
```

and after the trait `Hashes`:

```rust
/// The [`Hashes`] a build keys its code by: the items lane's, or a test's.
pub enum HashSource {
    Items(ItemHashes),
    #[cfg(test)]
    ById(by_id::ById),
}

impl Hashes for HashSource {
    fn item(&self, db: &Db, item: ItemId) -> Result<ItemKey, String> {
        match self {
            HashSource::Items(it) => it.item(db, item),
            #[cfg(test)]
            HashSource::ById(it) => it.item(db, item),
        }
    }

    fn code(&self, db: &Db, body: Body) -> Result<CodeHash, String> {
        match self {
            HashSource::Items(it) => it.code(db, body),
            #[cfg(test)]
            HashSource::ById(it) => it.code(db, body),
        }
    }

    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        match self {
            HashSource::Items(it) => it.ty(db, ty),
            #[cfg(test)]
            HashSource::ById(it) => it.ty(db, ty),
        }
    }

    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        match self {
            HashSource::Items(it) => it.konst(db, konst),
            #[cfg(test)]
            HashSource::ById(it) => it.konst(db, konst),
        }
    }

    fn span(&self, db: &Db, owner: DefWithBodyId, span: MirSpan) -> Result<SpanRef, String> {
        match self {
            HashSource::Items(it) => it.span(db, owner, span),
            #[cfg(test)]
            HashSource::ById(it) => it.span(db, owner, span),
        }
    }

    fn declaration(&self, db: &Db, func: FunctionId) -> Result<SpanRef, String> {
        match self {
            HashSource::Items(it) => it.declaration(db, func),
            #[cfg(test)]
            HashSource::ById(it) => it.declaration(db, func),
        }
    }

    fn file_name(&self, db: &Db, file: base_db::FileId) -> Result<String, String> {
        match self {
            HashSource::Items(it) => it.file_name(db, file),
            #[cfg(test)]
            HashSource::ById(it) => it.file_name(db, file),
        }
    }
}
```

**New file `v2/by_id.rs`** (the two tests' `ById`, once):

```rust
//! The tests' [`Hashes`]: content IDs of the database's own ids.

use base_db::Db;
use blaze_format::SpanRef;
use blaze_ids::ContentId;
use blaze_items::{CodeHash, ItemId, ItemKey};
use hir_def::{DefWithBodyId, FunctionId};
use hir_ty::next_solver::{Const, Ty};
use hir_ty_mir::mir::MirSpan;

use super::{Body, Hashes};

/// Content IDs of the database's own ids: stable within one database, which is all a test
/// compares.
pub(crate) struct ById;

fn by_id(domain: &'static str, what: impl std::fmt::Debug) -> ContentId {
    ContentId::of(domain, format!("{what:?}").as_bytes())
}

impl Hashes for ById {
    fn item(&self, _: &Db, item: ItemId) -> Result<ItemKey, String> {
        Ok(ItemKey(by_id("test item", item)))
    }

    fn code(&self, _: &Db, body: Body) -> Result<CodeHash, String> {
        Ok(CodeHash(by_id("test code", body)))
    }

    fn ty<'db>(&self, _: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        Ok(by_id("test type", ty))
    }

    fn konst<'db>(&self, _: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        Ok(by_id("test const", konst))
    }

    fn span(&self, _: &Db, owner: DefWithBodyId, span: MirSpan) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test span", (owner, span)), index: 0 })
    }

    fn declaration(&self, _: &Db, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: by_id("test declaration", func), index: 0 })
    }

    fn file_name(&self, _: &Db, file: base_db::FileId) -> Result<String, String> {
        Ok(format!("{file:?}"))
    }
}
```

**`v2/frontend.rs`**: `use super::{Body, BuildTime, Hashes, ..}` gains `HashSource`
(`use super::{Body, BuildTime, HashSource, Hashes, HirInstance, HirStatic, Kind, build::Build, refs, undescribed};`);

```rust
    pub(crate) hashes: Box<dyn Hashes>,
```
becomes
```rust
    pub(crate) hashes: HashSource,
```
```rust
    pub fn new(databases: Databases<'db>, hashes: Box<dyn Hashes>, build_time: BuildTime) -> Self {
```
becomes
```rust
    pub fn new(databases: Databases<'db>, hashes: HashSource, build_time: BuildTime) -> Self {
```
both `let hashes = &*self.hashes;` become `let hashes = &self.hashes;`, and `hashes: &dyn Hashes`
in `inlined`, `code_of`, `args_of` becomes `hashes: &HashSource`.

**`v2/backend.rs`**: add `HashSource` to `use super::{..}`; `hashes: &dyn Hashes,` (in `prepared`,
`written_spans`, `inlined_scopes`, `named`) becomes `hashes: &HashSource,`;
`self.prepared(db, homes, &*frontend.hashes, unit, ..)` becomes
`self.prepared(db, homes, &frontend.hashes, unit, ..)`, and `let hashes = &*frontend.hashes;`
(in `compile_static`) becomes `let hashes = &frontend.hashes;`.

**`v2/places.rs`** (`place_requests`, `fill_type_ids`) and **`v2/statics.rs`** (`value`,
`compile`): add `HashSource` to `use super::{..}` and `hashes: &dyn Hashes,` becomes
`hashes: &HashSource,`. Each file keeps `Hashes` imported: its methods are the trait's.

**`v2/tests.rs`**: delete the doc comment, `struct ById;`, `fn by_id` and `impl Hashes for ById`
(the block from `/// Content IDs of the database's own ids` to the end of the impl); add
`HashSource, by_id::ById` to `use super::{..}` and drop `Body`, `Hashes` from it; drop
`use blaze_format::SpanRef;`, `ItemId` from `use blaze_items::{..}` (keep `CodeHash, ItemKey`), and
`next_solver::{Const, Ty}` from `use hir_ty::{..}` (keep `db::HirDatabase`). Then each
`Box::new(ById)` (three, in `held_as` and two tests) becomes `HashSource::ById(ById)`, and each
`&ById` (`place_requests(db, homes, &ById, ..)` five times, `super::frontend::inlined(db, &ById, ..)`
twice) becomes `&HashSource::ById(ById)`.

**`v2/refs/tests.rs`**: the same deletion; add `HashSource, by_id::ById` to the `v2::{..}` import
and drop `Body`, `Hashes` from it; drop `use blaze_format::SpanRef;`, the whole
`use blaze_items::{CodeHash, ItemId, ItemKey};`, `DefWithBodyId` from `use hir_def::{..}`,
`next_solver::{Const, Ty}` from `use hir_ty::{..}`, and `use hir_ty_mir::mir::MirSpan;`. Then
`Box::new(ById)` (in `frontend`) becomes `HashSource::ById(ById)` and both
`place_requests(db, homes, &ById, ..)` become `place_requests(db, homes, &HashSource::ById(ById), ..)`.

**`v2/hashes/tests.rs`**: `use crate::v2::{Body, Hashes};` becomes
`use crate::v2::{Body, HashSource, Hashes, by_id::ById};`, and add the test that each variant
answers as what it holds:

```rust
#[test]
fn a_hash_source_says_what_the_hashes_it_holds_say() {
    let (db, _) = fixture("sized", CALLS);
    let (items, by_id) = (HashSource::Items(hashes()), HashSource::ById(ById));
    for name in ["callee", "caller"] {
        let func = function(&db, name);
        let (item, body) = (ItemId::Function(func), Body::Item(func.into()));
        assert_eq!(items.item(&db, item), hashes().item(&db, item));
        assert_eq!(items.code(&db, body), hashes().code(&db, body));
        assert_eq!(items.declaration(&db, func), hashes().declaration(&db, func));
        assert_eq!(by_id.item(&db, item), ById.item(&db, item));
        assert_eq!(by_id.code(&db, body), ById.code(&db, body));
        assert_eq!(by_id.declaration(&db, func), ById.declaration(&db, func));
    }
}
```

**`blaze-cargo/src/serve/unit/codegen.rs`** is lane cargo's: the one call there,
`HirFrontend::new(Databases::new(reader.db()), Box::new(ItemHashes::new(Box::new(names))), build_time(job))`
becomes
`HirFrontend::new(Databases::new(reader.db()), HashSource::Items(ItemHashes::new(Box::new(names))), build_time(job))`
with `HashSource` added to its `use hir_ty_codegen::v2::{..}`. dedyn-codegen asks it in
`v2/requests/cargo.md` under "## dedyn:".

### dedyn-v2 4: `v2/fold.rs`: a function's names are an enum, `Names`

In production `names` is always `|it| name(function, refs, it)` (`masked`, and one closure per
member in `merge`); the tests use the plain `fn names`. A generic would copy the ~400-line IR walk
(`canon`, `Canon`) once per closure type, two in production; an enum keeps one copy, and `merge`
no longer collects a `Vec` of closures. Asked per user-named function and global value of every
function masked.

After `pub(crate) fn name(..)`, add

```rust
/// What the names of a prepared function's IR are: its requests' places among the references
/// ([`name`]), or a test's fixed rule.
#[derive(Clone, Copy)]
pub(crate) enum Names<'a> {
    Of(&'a Function, &'a Refs),
    #[cfg(test)]
    Plain(fn(ir::UserExternalName) -> Option<Name>),
}

impl Names<'_> {
    pub(crate) fn of(self, it: ir::UserExternalName) -> Option<Name> {
        match self {
            Names::Of(function, refs) => name(function, refs, it),
            #[cfg(test)]
            Names::Plain(names) => names(it),
        }
    }
}
```

Then:
- `masked`: `masked_ir(function.ir()?, &|it| name(function, refs, it))` becomes
  `masked_ir(function.ir()?, Names::Of(function, refs))`.
- `masked_ir`, `canon`: `names: &dyn Fn(ir::UserExternalName) -> Option<Name>,` becomes
  `names: Names<'_>,`.
- `fn walk(func: &ir::Function, names: &dyn Fn(ir::UserExternalName) -> Option<Name>) -> Option<Walk> {`
  becomes `fn walk(func: &ir::Function, names: Names<'_>) -> Option<Walk> {`.
- `struct Canon<'a>`: `names: &'a dyn Fn(ir::UserExternalName) -> Option<Name>,` becomes
  `names: Names<'a>,`.
- `Canon::name`: `let constant = match (self.names)(self.func.params.user_named_funcs()[*user].clone())? {`
  becomes `let constant = match self.names.of(self.func.params.user_named_funcs()[*user].clone())? {`.
- `pub(crate) type Member<'a> =\n    (&'a ir::Function, &'a dyn Fn(ir::UserExternalName) -> Option<Name>, &'a Refs);`
  becomes `pub(crate) type Member<'a> = (&'a ir::Function, Names<'a>, &'a Refs);` (`build`'s
  `walk(func, *names)` is unchanged).
- `merge`: replace

```rust
    let names: Vec<_> = members
        .iter()
        .map(|&(_, _, prepared, refs)| move |it: ir::UserExternalName| name(prepared, refs, it))
        .collect();
    let mut sources: Vec<Member<'_>> = Vec::with_capacity(members.len());
    for (at, (&(_, _, prepared, refs), names)) in members.iter().zip(&names).enumerate() {
        let Some(func) = prepared.ir() else {
            return Err(fail(format!("member {at} is assembly, which has no IR to fold")));
        };
        sources.push((func, names as &dyn Fn(ir::UserExternalName) -> Option<Name>, refs));
    }
```

  with

```rust
    let mut sources: Vec<Member<'_>> = Vec::with_capacity(members.len());
    for (at, &(_, _, prepared, refs)) in members.iter().enumerate() {
        let Some(func) = prepared.ir() else {
            return Err(fail(format!("member {at} is assembly, which has no IR to fold")));
        };
        sources.push((func, Names::Of(prepared, refs), refs));
    }
```

Tests (`mod tests` of `fold.rs`): after `fn names(..)` add

```rust
    const PLAIN: Names<'static> = Names::Plain(names);

    #[test]
    fn plain_names_are_what_their_rule_says() {
        for (namespace, index) in [(0, 3), (1, 5), (2, 0)] {
            let name = ir::UserExternalName::new(namespace, index);
            assert_eq!(PLAIN.of(name.clone()), names(name));
        }
    }
```

and replace `&names` with `PLAIN` in every `masked_ir(.., &names)` (~1078, 1092, 1112, 1113, 1118,
1665, 1712, 1742, 1744, 1750, 1751, 1752, 1772, 1839, 1884) and in the `Member` arrays
(`[(&a, &names, &refs_a[..]), (&b, &names, &refs_b[..])]` and the two like it, ~1781, 1849, 1894).
Leave `text::masked(.., &names)` and its `&dyn Fn` (the test model of `mod text`) as they are.

### dedyn-v2 5: kept, with a `// dyn:` line above (indented as the line)

- `v2/backend.rs` `fn isa(unit: &Unit) -> Result<&'static dyn TargetIsa, CodegenError> {`,
  `v2/convert.rs` `isa: &dyn TargetIsa,`, `v2/unwind.rs` `isa: &dyn TargetIsa,` and
  `fn common(isa: &dyn TargetIsa) -> Result<&'static Common, String> {`:
  `// dyn: cranelift's API takes `&dyn TargetIsa`; its backends are private, one made per process`
- `v2/hashes.rs` `names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>,` and
  `pub fn new(names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>) -> ItemHashes {`:
  `// dyn: blaze-cargo's naming of files, set once per build; asked per `file!()` and position, each allocating its name`
