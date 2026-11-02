# Requests to `fastcompile-ident-share` (`crates/hir-ty-share/`)

## dedup: one type identity for the code generator and the share key

From `dedup-scan-ident` (lane `dedup`), 2026-10-29. Notes:
`docs/v2/plan/lanes/dedup/scan-ident.md`. The paired request to lane `codegen` is
`v2/requests/codegen.md`, `## dedup: one type identity (hashes.rs onto hir-ty-share)`. Apply
this one first: codegen's edit calls what this one makes public.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### Why

`crates/hir-ty-share/src/identity.rs` (`Spell`, `adt`, `variants`) is a copy of
`crates/hir-ty-codegen/v2/hashes.rs` (`Spell`, `adt_content`, `variants`): the same walk, tag
for tag, over the same 25 `TyKind`s. The scan found seven runs (hashes.rs 357-389, 446-521,
536-565, 570-590, 602-622 against identity.rs 65-99, 174-250, 266-295, 300-320, 328-348). The
two copies differ in these ways:

| | `hir-ty-codegen` `Spell` | `hir-ty-share` `Spell` | effect on which types hash equal |
|---|---|---|---|
| buffer | one `Vec<u8>` per spelling, then `ContentId::of` | streamed into one `Hasher` | none: BLAKE3 gives the same bytes either way |
| string | `len` u64 then bytes | `Hasher::part`, the same bytes | none |
| `FnPtr` ABI and safety | `format!("{:?} {:?}")` as one string | each `Debug` text ended by `0xff` | none: both are injective |
| domains | `TY_DOMAIN`, `CONST_DOMAIN` apart | one `IDENTITY_DOMAIN` for types, constants and argument lists | none today (the tags keep them apart), but three grammars share one domain |
| error | `String` | `Unshared` | none |
| new `rustc_type_ir` | not ported (`Alias(alias)`, `ConstKind::Unevaluated`) | ported | none |
| **`Coroutine`** | its key, then **its compiled code**: `coroutine_code_in` or `by_move_code_in` (the code hash, its bindings' names, its positions, and for a by-move body the closure's captures) | its key, then `blaze_items::coroutine_code` only | **share misses inputs** |
| **`CoroutineWitness`** | `coroutine_code_in` | `coroutine_code` | **share misses inputs** |

Within one revision the two give the same partition, since one coroutine key is one body. Across
an edit they do not. If you rename a binding inside an `async` block, the code generator's
identity of its type changes and share's does not.

The code generator's choice is deliberate. Its `Hashes::ty` is used for three things: the
instance's argument key (`args_of`), the value of a `TypeId` (`fill_type_ids` writes its first
16 bytes into a constant), and the type of a debug-info variable (`side::Variable::ty`). Its
debug info names a coroutine's saved locals by their bindings (`debuginfo/types.rs`
`coroutine`).

Share's coarser spelling is therefore a cache key that misses an input. Once request 1 in
`v2/requests/fastcompile.md` puts the share key in `Described::args`, these instances keep their
key across such an edit:

- `tid::<C>` for a coroutine type `C`;
- any instance holding a local of type `C`.

Their cached templates then hold a stale `TypeId` and stale debug-info type ids. That breaks the
"cache keys must miss no input" rule in AGENTS.md: a stale `TypeId` makes `TypeId::of::<C>()`
compare unequal to one computed in a freshly built template.

The fix is **one spelling, the finer one, used by both**. Its home is `hir-ty-share`, because:

- it is a front-end salsa crate (AGENTS.md rule 8);
- codegen depends on it anyway (fastcompile request 1);
- its copy is the newer one (`&Db`, the new `rustc_type_ir`, streaming).

`blaze-items` would also fit but is not free: `mem-backend-items` is moving it onto `blaze-mem`.
The coroutine's code is needed for the finer spelling, so codegen's body-code functions
(`with_spans`, `names`, `table_of`, `item_code`, `closure_code_in`, `coroutine_code_in`,
`by_move_code_in`) move here too, **verbatim** (same domains, same bytes). The code generator's
`Hashes::code` values do not change.

The shared spelling changes the bytes of both old `Hashes::ty` and share's identity, so it gets
new domain names. Nothing persists v2 caches yet.

Also in this request: `fact.rs`'s `Cx::is_plain_data` is a copy of
`hir-ty-codegen/mod.rs` `FunctionCx::is_plain_data` (the scan's run, `fact.rs` 111-128 against
`mod.rs` ~11232-11270). Fastcompile request 1 asks to keep them "in step". Instead, one
`pub fn is_plain_data` here, and codegen calls it.

One known dependency: the moved `with_spans` and `table_of` use `MirBody::inlined` and
`MirSpan::Inlined`, which are not in `hir-ty` on disk yet (they come with `codegen-inline`'s
request to hir-ty). They move unchanged, so they compile once that lands, exactly as
`hashes.rs` does today.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 1. `crates/hir-ty-share/src/code.rs` (new file)

```rust
//! The code of a body as the code generator compiles it: its item's code hash
//! ([`blaze_items`], each callee by its interface hash) with what a template built from it
//! holds that the code hash leaves out: the names of its bindings, which its debug info tells,
//! and the positions its MIR names, by span table and index ([`table_of`]). A template that
//! names an expression by its id is never reused once that id is another expression's.
//!
//! The code generator's `Hashes::code` is this, and a coroutine's identity
//! ([`crate::identity`]) holds it: its layout, its drop glue and the debug info of a value of
//! it read its body's locals and their names.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, closure_code, code_hash, coroutine_code, interface_hash, key};
use hir_def::{DefWithBodyId, expr_store::ExpressionStore};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{MirBody, MirSpan},
    next_solver::TyKind,
};
use rustc_type_ir::inherent::IntoKind;

use crate::{Unshared, identity::unhashable};

pub const CODE_DOMAIN: &str = "blaze v2 codegen code 1";
pub const DISCRIMINANT_DOMAIN: &str = "blaze v2 codegen discriminant 1";
pub const NAMES_DOMAIN: &str = "blaze v2 codegen binding names 1";

/// The tables of a body's spans, by the kind of span: [`MirSpan`]'s variants.
pub const EXPRS: usize = 0;
pub const EXPR_ENDS: usize = 1;
pub const PATS: usize = 2;
pub const BINDINGS: usize = 3;
pub const SELF_PARAM: usize = 4;
pub const UNKNOWN: usize = 5;
pub const TABLES: usize = 6;

/// What a body's code is beside its code hash, in the preimage of [`CODE_DOMAIN`].
const ITEM: u8 = 0;
const CLOSURE: u8 = 1;
const COROUTINE: u8 = 2;
const BY_MOVE: u8 = 3;

/// Which table of its owner's a span is in, and its index there; for a span of code inlined
/// into the owner's body, which is of another's, its index in `Inlined::spans`.
pub fn table_of(span: MirSpan) -> Result<(usize, u32), u32> {
    Ok(match span {
        MirSpan::ExprId(it) => (EXPRS, it.into_raw().into_u32()),
        MirSpan::ExprEnd(it) => (EXPR_ENDS, it.into_raw().into_u32()),
        MirSpan::PatId(it) => (PATS, it.into_raw().into_u32()),
        MirSpan::BindingId(it) => (BINDINGS, it.into_raw().into_u32()),
        MirSpan::SelfParam => (SELF_PARAM, 0),
        MirSpan::Unknown => (UNKNOWN, 0),
        MirSpan::Inlined(at) => return Err(at),
    })
}

/// The code hash beside what a template built from `body` holds of it that the code hash
/// leaves out: the names of its bindings and the positions it names, by table and index, in
/// the order of its blocks.
fn with_spans(db: &Db, kind: u8, parts: &[ContentId], body: &MirBody<'_>) -> ContentId {
    // alloc: one buffer per body, this query's miss, memoized after
    let mut out = Vec::with_capacity(1 + 32 * (parts.len() + 1) + 5 * 8 * body.basic_blocks.len());
    out.push(kind);
    for part in parts {
        out.extend_from_slice(&part.0);
    }
    out.extend_from_slice(&names(db, body).0);
    let push = |out: &mut Vec<u8>, span: Option<MirSpan>| match span.map(table_of) {
        Some(Ok((table, index))) => {
            out.push(table as u8);
            out.extend_from_slice(&index.to_le_bytes());
        }
        Some(Err(at)) => {
            out.push(0xfe);
            out.extend_from_slice(&at.to_le_bytes());
        }
        None => out.push(0xff),
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            push(&mut out, Some(statement.span));
            statement.kind.for_each_operand(&mut |it| push(&mut out, it.span));
        }
        if let Some(terminator) = &block.terminator {
            push(&mut out, Some(terminator.span));
            terminator.kind.for_each_operand(&mut |it| push(&mut out, it.span));
        }
    }
    // The spans of inlined code, each of the body of its scope's callee, and where each
    // scope was inlined at.
    let inlined = &body.inlined;
    out.extend_from_slice(&(inlined.spans.len() as u32).to_le_bytes());
    for span in &inlined.spans {
        out.extend_from_slice(&span.scope.to_le_bytes());
        push(&mut out, Some(span.span));
    }
    out.extend_from_slice(&(inlined.scopes.len() as u32).to_le_bytes());
    for scope in &inlined.scopes {
        out.extend_from_slice(&scope.callee.to_le_bytes());
        push(&mut out, Some(scope.call_site));
    }
    ContentId::of(CODE_DOMAIN, &out)
}

/// The name of each of `body`'s bindings, in the order of their locals, each framed by its
/// local and its length.
fn names(db: &Db, body: &MirBody<'_>) -> ContentId {
    let store = ExpressionStore::of(db, body.owner.expression_store_owner(db));
    let mut out = Hasher::new(NAMES_DOMAIN);
    for (local, binding) in body.local_to_binding_map().iter() {
        let name = store[*binding].name.as_str();
        out.update(&local.into_raw().into_u32().to_le_bytes());
        out.update(&(name.len() as u32).to_le_bytes());
        out.update(name.as_bytes());
    }
    out.finish()
}

fn no_mir(what: impl std::fmt::Debug, error: impl std::fmt::Debug) -> Unshared {
    Unshared::NoMir(format!("the MIR of {what:?} failed to lower: {error:?}")) // alloc: error path
}

/// The code of a function, constant or static: its item's code hash. An enum variant's
/// discriminant is a body of its enum's interface.
#[salsa::tracked(returns(ref))]
pub fn of_item(db: &Db, def: DefWithBodyId) -> Result<ContentId, Unshared> {
    let item = ItemId::of_body(db, def);
    let code = match def {
        DefWithBodyId::VariantId(variant) => {
            let mut h = Hasher::new(DISCRIMINANT_DOMAIN);
            h.id(&key::variant_key(db, variant).0)
                .id(&interface_hash(db, item).map_err(unhashable(db))?.0);
            h.finish()
        }
        DefWithBodyId::FunctionId(_) | DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) => {
            match code_hash(db, item).map_err(unhashable(db))? {
                Some(code) => code.0,
                None => {
                    return Err(Unshared::NoMir(format!(
                        "blaze-items: {:?} `{}` has no body to compile",
                        item.kind(),
                        key::path(db, item)
                    ))); // alloc: error path
                }
            }
        }
    };
    let body = db.mir_body(InferBodyId::DefWithBodyId(def)).map_err(|it| no_mir(def, it))?;
    Ok(with_spans(db, ITEM, &[code], body))
}

#[salsa::tracked(returns(ref))]
pub fn of_closure<'db>(db: &'db Db, closure: InternedClosureId<'db>) -> Result<ContentId, Unshared> {
    let code = closure_code(db, closure).map_err(unhashable(db))?;
    let body = db.mir_body_for_closure(closure).map_err(|it| no_mir(closure, it))?;
    Ok(with_spans(db, CLOSURE, &[code.0], body))
}

#[salsa::tracked(returns(ref))]
pub fn of_coroutine<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unshared> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let body = db.mir_body_for_coroutine(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(db, COROUTINE, &[code.0], body))
}

/// The future an `async` closure gives when called by value: the coroutine's body lowered
/// against the closure's captures, so its code is the coroutine's with what the closure
/// captures and how.
#[salsa::tracked(returns(ref))]
pub fn of_coroutine_by_move<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, Unshared> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let loc = coroutine.loc(db);
    let closure = ExpressionStore::closure_for_coroutine(loc.expr);
    let closure = match InferenceResult::of(db, loc.owner).expr_ty(closure).kind() {
        TyKind::CoroutineClosure(id, _) => id.0.as_closure(db),
        other => {
            return Err(Unshared::NoMir(format!(
                "the coroutine {coroutine:?}, run by move, is of no `async` closure but of {other:?}"
            ))); // alloc: error path
        }
    };
    let captures =
        blaze_items::value(db, &blaze_items::captures(db, closure)).map_err(unhashable(db))?;
    let body =
        db.mir_body_for_coroutine_by_move(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(db, BY_MOVE, &[code.0, captures], body))
}
```

`names` now streams into the `Hasher` instead of a `Vec`. That is the same bytes under the same
domain (BLAKE3 is independent of how the input is split), with no allocation.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 2. `crates/hir-ty-share/src/identity.rs` (replace the whole file)

```rust
//! The full identity of a type, constant or argument list, spelled by content: what tells two
//! types apart wherever a program or its code can (a `TypeId`, a `type_name`, a vtable, a
//! `dyn` call, debug info, a closure's or coroutine's body). This is the one spelling of a type
//! by content. The code generator's `Hashes::ty` and `Hashes::konst` (instance arguments,
//! `TypeId` values, the types of its debug info) are this, and so are the share key's identity
//! records, so a share key covers every type id a template holds.
//!
//! - An ADT by its key, its interface, its layout and the `Drop` impls its drop glue reads
//!   ([`adt`], a tracked query); its arguments the same way.
//! - A closure by its key and arguments; a coroutine by its key, its compiled code
//!   ([`crate::code`]: its code hash, its bindings' names, its positions) and its arguments.
//! - Regions are left out.
//!
//! Each spelling streams into one [`Hasher`]: nothing is allocated but a trait object's
//! bounds past four, which are sorted.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, Unhashable, interface_hash, item_key, key, layout_hash};
use either::Either;
use hir_def::{AdtId, CallableDefId, VariantId};
use hir_ty::{
    db::HirDatabase,
    mir::is_by_move_coroutine,
    next_solver::{Binder, Const, ConstKind, GenericArgs, Pattern, TermId, Ty, TyKind, ValTree},
};
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, InferConst,
    PatternKind, TermKind, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;

use crate::{Unshared, code};

/// The domain of a type's identity.
pub const TY_DOMAIN: &str = "blaze v2 identity type 1";
/// The domain of a constant's identity.
pub const CONST_DOMAIN: &str = "blaze v2 identity const 1";
/// The domain of an argument list's identity.
pub const ARGS_DOMAIN: &str = "blaze v2 identity args 1";
/// The domain of one bound of a trait object, hashed alone so the bounds can be sorted.
pub const BOUND_DOMAIN: &str = "blaze v2 identity bound 1";
/// The domain of an ADT's identity: its key, interface, layout and the drop impls it reaches.
pub const ADT_DOMAIN: &str = "blaze v2 identity adt 1";

/// The identity of `ty`. Call it with the database attached (`hir_ty::attach_db`).
pub fn ty<'db>(db: &'db Db, ty: Ty<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db, TY_DOMAIN);
    spell.ty(ty)?;
    Ok(spell.out.finish())
}

/// The identity of `args`: each type and constant, and a lifetime as a placeholder.
pub fn args<'db>(db: &'db Db, args: GenericArgs<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db, ARGS_DOMAIN);
    spell.args(args)?;
    Ok(spell.out.finish())
}

/// The identity of `konst`: its value and its type's identity.
pub fn konst<'db>(db: &'db Db, konst: Const<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db, CONST_DOMAIN);
    spell.konst(konst)?;
    Ok(spell.out.finish())
}

pub(crate) fn unhashable(db: &Db) -> impl Fn(Unhashable) -> Unshared + '_ {
    move |it| Unshared::Unhashable(it.describe(db))
}

fn no_content(what: String) -> Unshared {
    Unshared::NoContent(what)
}

/// An ADT as its identity says it: its key, its interface (fields, variants, `repr`), its
/// layout, and the `Drop` impls of it and of every ADT its fields' types name, transitively,
/// which drop glue reads.
#[salsa::tracked(returns(ref))]
pub fn adt(db: &Db, adt: AdtId) -> Result<ContentId, Unshared> {
    let item = ItemId::Adt(adt);
    let mut h = Hasher::new(ADT_DOMAIN);
    h.id(&item_key(db, item).0)
        .id(&interface_hash(db, item).map_err(unhashable(db))?.0)
        .id(&layout_hash(db, adt).map_err(unhashable(db))?.0);
    let mut seen: FxHashSet<AdtId> = FxHashSet::default();
    seen.insert(adt);
    // alloc: the ADTs the fields reach, once per ADT: this query's miss, memoized after
    let mut next = vec![adt];
    // alloc: as `next`; sorted, because the walk's order is not the ADT's
    let mut dropping: Vec<(ItemKey, ContentId)> = Vec::new();
    while let Some(at) = next.pop() {
        if let Some(imp) = hir_ty::drop::destructor(db, at) {
            let interface = interface_hash(db, ItemId::Impl(imp)).map_err(unhashable(db))?;
            dropping.push((item_key(db, ItemId::Adt(at)), interface.0));
        }
        for variant in variants(db, at) {
            for field in db.field_types(variant).values() {
                for arg in field.ty().skip_binder().walk() {
                    if let Some(ty) = arg.ty()
                        && let TyKind::Adt(def, _) = ty.kind()
                        && seen.insert(def.def_id())
                    {
                        next.push(def.def_id());
                    }
                }
            }
        }
    }
    dropping.sort_unstable();
    h.update(&(dropping.len() as u64).to_le_bytes());
    for (key, interface) in &dropping {
        h.id(&key.0).id(interface);
    }
    Ok(h.finish())
}

/// The variants of `adt` in declaration order: one for a struct or union.
pub(crate) fn variants<'db>(db: &'db Db, adt: AdtId) -> impl Iterator<Item = VariantId> + 'db {
    match adt {
        AdtId::StructId(it) => Either::Left(std::iter::once(it.into())),
        AdtId::UnionId(it) => Either::Left(std::iter::once(it.into())),
        AdtId::EnumId(it) => Either::Right(
            it.enum_variants(db).variants.iter().map(|(_, (variant, _))| VariantId::from(*variant)),
        ),
    }
}

/// A type or constant spelled by content straight into its hash: nothing is buffered.
struct Spell<'db> {
    db: &'db Db,
    out: Hasher,
}

/// `fmt::Write` into a spelling: a `Debug` name hashed without a `String`.
struct Text<'a>(&'a mut Hasher);

impl std::fmt::Write for Text<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.update(text.as_bytes());
        Ok(())
    }
}

impl<'db> Spell<'db> {
    fn new(db: &'db Db, domain: &'static str) -> Spell<'db> {
        Spell { db, out: Hasher::new(domain) }
    }

    fn u8(&mut self, value: u8) {
        self.out.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.out.update(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.out.update(&(len as u64).to_le_bytes());
    }

    fn id(&mut self, id: &ContentId) {
        self.out.id(id);
    }

    fn str(&mut self, text: &str) {
        self.out.part(text.as_bytes());
    }

    /// `value`'s `Debug` text, ended by `0xff`, which no UTF-8 text holds.
    fn debug(&mut self, value: &impl std::fmt::Debug) {
        use std::fmt::Write as _;
        // `Text` never fails, so neither does the write.
        let _ = write!(Text(&mut self.out), "{value:?}");
        self.u8(0xff);
    }

    fn key(&mut self, key: ItemKey) {
        self.id(&key.0);
    }

    fn item(&mut self, item: ItemId) {
        self.key(item_key(self.db, item));
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<(), Unshared> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => self.u8(0),
            TyKind::Char => self.u8(1),
            TyKind::Int(it) => {
                self.u8(2);
                self.str(it.name_str());
            }
            TyKind::Uint(it) => {
                self.u8(3);
                self.str(it.name_str());
            }
            TyKind::Float(it) => {
                self.u8(4);
                self.str(it.name_str());
            }
            TyKind::Adt(def, args) => {
                self.u8(5);
                let content = adt(db, def.def_id()).as_ref().map_err(Clone::clone)?;
                self.id(content);
                self.args(args)?;
            }
            TyKind::Foreign(it) => {
                self.u8(6);
                self.item(ItemId::Alias(it.0));
            }
            TyKind::Str => self.u8(7),
            TyKind::Array(element, len) => {
                self.u8(8);
                self.ty(element)?;
                self.konst(len)?;
            }
            TyKind::Pat(base, pattern) => {
                self.u8(9);
                self.ty(base)?;
                self.pattern(pattern)?;
            }
            TyKind::Slice(element) => {
                self.u8(10);
                self.ty(element)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                self.u8(11);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::Ref(_, pointee, mutability) => {
                self.u8(12);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                self.u8(13);
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        self.u8(0);
                        self.item(ItemId::Function(it));
                    }
                    CallableDefId::StructId(it) => {
                        self.u8(1);
                        self.item(ItemId::Adt(it.into()));
                    }
                    CallableDefId::EnumVariantId(it) => {
                        self.u8(2);
                        self.key(key::variant_key(db, it));
                    }
                }
                self.args(args)?;
            }
            TyKind::FnPtr(tys, header) => {
                self.u8(14);
                self.len(tys.bound_vars().len());
                let tys = tys.skip_binder().inputs_and_output;
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
                self.debug(&header.abi());
                self.debug(&header.safety());
                self.u8(header.c_variadic() as u8);
            }
            TyKind::UnsafeBinder(inner) => {
                self.u8(15);
                let binder: Binder<'db, Ty<'db>> = inner.into();
                self.len(binder.bound_vars().len());
                self.ty(binder.skip_binder())?;
            }
            TyKind::Dynamic(predicates, _) => {
                self.u8(16);
                // The order the bounds are written in is not the type's.
                let mut each: SmallVec<[ContentId; 4]> = SmallVec::new();
                for predicate in predicates.iter() {
                    let mut inner = Spell::new(db, BOUND_DOMAIN);
                    inner.len(predicate.bound_vars().len());
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            inner.u8(0);
                            inner.item(ItemId::Trait(it.def_id.0));
                            inner.args(it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            inner.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => inner.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => inner.item(ItemId::Const(konst)),
                            }
                            inner.args(it.args)?;
                            match it.term.kind() {
                                TermKind::Ty(ty) => {
                                    inner.u8(0);
                                    inner.ty(ty)?;
                                }
                                TermKind::Const(konst) => {
                                    inner.u8(1);
                                    inner.konst(konst)?;
                                }
                            }
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            inner.u8(2);
                            inner.item(ItemId::Trait(it.0));
                        }
                    }
                    each.push(inner.out.finish());
                }
                each.sort_unstable();
                self.len(each.len());
                for it in &each {
                    self.id(it);
                }
            }
            TyKind::Closure(id, args) => {
                self.u8(17);
                self.key(key::closure_key(db, id.0));
                self.args(args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                self.u8(18);
                self.key(key::coroutine_closure_key(db, id.0));
                self.args(args)?;
            }
            // A coroutine's layout is of the locals its body keeps across its yields, which
            // its arguments do not say, and its debug info names them: its compiled code says
            // both.
            TyKind::Coroutine(id, args) => {
                self.u8(19);
                self.key(key::coroutine_key(db, id.0));
                let code = match is_by_move_coroutine(db, id.0, args.as_coroutine()) {
                    true => code::of_coroutine_by_move(db, id.0),
                    false => code::of_coroutine(db, id.0),
                };
                self.id(code.as_ref().map_err(Clone::clone)?);
                self.args(args)?;
            }
            TyKind::CoroutineWitness(id, args) => {
                self.u8(20);
                self.key(key::coroutine_key(db, id.0));
                self.id(code::of_coroutine(db, id.0).as_ref().map_err(Clone::clone)?);
                self.args(args)?;
            }
            TyKind::Never => self.u8(21),
            TyKind::Tuple(tys) => {
                self.u8(22);
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
            }
            TyKind::Param(param) => {
                self.u8(23);
                self.u32(param.index);
            }
            TyKind::Bound(index, bound) => {
                self.u8(24);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
                self.u8(match bound.kind {
                    BoundTyKind::Anon => 0,
                    BoundTyKind::Param(_) => 1,
                });
            }
            TyKind::Alias(_, alias) => {
                let what = match alias.kind {
                    AliasTyKind::Projection { .. } => "a projection",
                    AliasTyKind::Inherent { .. } => "an inherent associated type",
                    AliasTyKind::Opaque { .. } => "an opaque type",
                    AliasTyKind::Free { .. } => "a type alias",
                };
                return Err(no_content(format!("{what} left unnormalized in `{ty:?}`"))); // alloc: error path
            }
            TyKind::Placeholder(_) => {
                return Err(no_content(format!("a placeholder type `{ty:?}`"))); // alloc: error path
            }
            TyKind::Infer(_) => {
                return Err(no_content(format!("an inference variable `{ty:?}`"))); // alloc: error path
            }
            TyKind::Error(_) => return Err(no_content("a type with an error".to_owned())), // alloc: error path
        }
        Ok(())
    }

    fn bound_index(&mut self, index: BoundVarIndexKind) {
        match index {
            BoundVarIndexKind::Bound(depth) => {
                self.u8(0);
                self.u32(depth.as_u32());
            }
            BoundVarIndexKind::Canonical => self.u8(1),
        }
    }

    fn args(&mut self, args: GenericArgs<'db>) -> Result<(), Unshared> {
        self.len(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(_) => self.u8(0),
                GenericArgKind::Type(ty) => {
                    self.u8(1);
                    self.ty(ty)?;
                }
                GenericArgKind::Const(konst) => {
                    self.u8(2);
                    self.konst(konst)?;
                }
            }
        }
        Ok(())
    }

    fn pattern(&mut self, pattern: Pattern<'db>) -> Result<(), Unshared> {
        match pattern.kind() {
            PatternKind::Range { start, end } => {
                self.u8(0);
                self.konst(start)?;
                self.konst(end)?;
            }
            PatternKind::Or(patterns) => {
                self.u8(1);
                self.len(patterns.len());
                for it in patterns.iter() {
                    self.pattern(it)?;
                }
            }
            PatternKind::NotNull => self.u8(2),
        }
        Ok(())
    }

    /// A constant by its value and its type's identity.
    fn konst(&mut self, konst: Const<'db>) -> Result<(), Unshared> {
        match konst.kind() {
            ConstKind::Value(value) => {
                self.u8(0);
                self.ty(value.ty)?;
                self.valtree(value.value)?;
            }
            ConstKind::Param(param) => {
                self.u8(1);
                self.u32(param.index);
            }
            ConstKind::Bound(index, bound) => {
                self.u8(2);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
            }
            ConstKind::Alias(..) => {
                return Err(no_content(format!("the constant `{konst:?}` is not evaluated"))); // alloc: error path
            }
            ConstKind::Expr(_) => {
                return Err(no_content(format!("the constant expression `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Infer(InferConst::Var(_) | InferConst::Fresh(_)) => {
                return Err(no_content(format!("an inference variable `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Placeholder(_) => {
                return Err(no_content(format!("a placeholder constant `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Error(_) => {
                return Err(no_content("a constant with an error".to_owned())); // alloc: error path
            }
        }
        Ok(())
    }

    fn valtree(&mut self, value: ValTree<'db>) -> Result<(), Unshared> {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                self.u8(0);
                self.out.update(&scalar.size().bytes().to_le_bytes());
                self.out.update(&scalar.to_bits_unchecked().to_le_bytes());
            }
            ValTreeKind::Branch(consts) => {
                self.u8(1);
                self.len(consts.len());
                for it in consts.iter() {
                    self.konst(it)?;
                }
            }
        }
        Ok(())
    }
}
```

Compared with today's file:

- the new domains (five constants here instead of `IDENTITY_DOMAIN` and `ADT_DOMAIN` in
  `lib.rs`);
- `ty`, `args`, `konst` and `adt` are `pub`, and `Spell` is private;
- the coroutine and witness arms take `code::*`;
- the docs.

Every other line is today's.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 3. `crates/hir-ty-share/src/lib.rs`

Before:

```rust
mod drop;
mod fact;
mod identity;
mod intrinsics;
mod layout;
mod walk;
```

After:

```rust
pub mod code;
mod drop;
mod fact;
pub mod identity;
mod intrinsics;
mod layout;
mod walk;
```

Before:

```rust
pub use intrinsics::{Class, classify};
```

After:

```rust
pub use fact::is_plain_data;
pub use intrinsics::{Class, classify};
```

Delete these four lines; the domains are now in `identity.rs` and `code.rs`:

```rust
/// The domain of a type's, constant's or arguments' full identity.
pub const IDENTITY_DOMAIN: &str = "blaze v2 share identity 1";
/// The domain of an ADT's identity: its key, interface, layout and the drop impls it reaches.
pub const ADT_DOMAIN: &str = "blaze v2 share adt 1";
```

In the crate doc, this sentence:

> and the full identity of everything else that tells types apart ([`identity`]): `TypeId`,
> `type_name`, vtables, `dyn` calls, closures.

becomes:

> and the full identity of everything else that tells types apart ([`identity`], the code
> generator's spelling of a type too): `TypeId`, `type_name`, vtables, `dyn` calls, closures,
> coroutines by their compiled code ([`code`]).
> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 4. `crates/hir-ty-share/src/fact.rs`: one `is_plain_data`

Imports. Before:

```rust
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue, is_freeze},
    layout::Layout,
    next_solver::{
        DbInterner, StoredTy, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::{StoredParamEnvAndCrate, implements_trait_unique, structurally_normalize_ty},
};
```

After (adds `ParamEnv`):

```rust
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    drop::{DropGlue, has_drop_glue, is_freeze},
    layout::Layout,
    next_solver::{
        DbInterner, ParamEnv, StoredTy, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt},
    },
    traits::{StoredParamEnvAndCrate, implements_trait_unique, structurally_normalize_ty},
};
```

Before:

```rust
/// How deep [`Cx::is_plain_data`] looks, as the code generator's `is_plain_data` does.
const PLAIN_DATA_DEPTH: u32 = 8;
```

After:

```rust
/// How deep [`is_plain_data`] looks into a type.
const PLAIN_DATA_DEPTH: u32 = 8;

/// Whether `ty` is made of scalars, pointers and references only, through arrays, tuples and
/// the fields of structs and enums: what the code generator reads to say a reference is the
/// only way to its memory (its `reference_pointee`), and what a pointee's summary records.
/// Only plain data is looked at: whether a type with a coroutine, a closure or a trait object
/// in it is `Unpin` takes more than its fields to know, and a wrong yes is a miscompile.
pub fn is_plain_data<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
) -> bool {
    plain_data(db, interner, infcx, param_env, ty, 0)
}

fn plain_data<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
    depth: u32,
) -> bool {
    if depth > PLAIN_DATA_DEPTH {
        return false;
    }
    match structurally_normalize_ty(infcx, ty, param_env).kind() {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Never
        | TyKind::RawPtr(..)
        | TyKind::Ref(..)
        | TyKind::FnPtr(..)
        | TyKind::FnDef(..) => true,
        TyKind::Array(element, _) | TyKind::Pat(element, _) => {
            plain_data(db, interner, infcx, param_env, element, depth + 1)
        }
        TyKind::Tuple(fields) => {
            fields.iter().all(|it| plain_data(db, interner, infcx, param_env, it, depth + 1))
        }
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if let hir_def::AdtId::UnionId(_) = adt {
                return false;
            }
            identity::variants(db, adt).all(|variant| {
                db.field_types(variant).iter().all(|(_, field)| {
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    plain_data(db, interner, infcx, param_env, ty, depth + 1)
                })
            })
        }
        _ => false,
    }
}
```

Delete the method `Cx::is_plain_data` (its doc comment, from `/// The code generator's
`is_plain_data`: made of scalars` down to its closing brace before `fn is_unpin`).

In `Cx::pointee`, before:

```rust
        let plain = element.is_none_or(|it| self.is_plain_data(it, 0));
```

After:

```rust
        let plain = element
            .is_none_or(|it| is_plain_data(self.db, self.interner, &self.infcx, self.env.param_env, it));
```

The one difference between the two old copies is that `structurally_normalize_ty`
(hir-ty `traits.rs`) also replaces inference variables with errors, which codegen's
`normalize` does not. That changes no answer: an inference variable and an error both reach
`_ => false`. The differential test below checks this.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 5. `crates/hir-ty-share/Cargo.toml`

Before:

```toml
[dev-dependencies]
test-fixture.workspace = true
```

After:

```toml
[dev-dependencies]
proptest.workspace = true
test-fixture.workspace = true
```

`proptest` is already a root workspace dependency (root `Cargo.toml` line 229).

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 6. `crates/hir-ty-share/src/tests/mod.rs`

Before:

```rust
mod db;
```

After:

```rust
mod db;
mod identity;
```

The new module uses `declarations`, `alias` and `function` of `tests/mod.rs` through `super::`;
they stay where they are. That they are copies of codegen's test helpers is `dedup-scan`'s
test-database request, not this one.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### 7. `crates/hir-ty-share/src/tests/identity.rs` (new file): the differential test

```rust
//! The one spelling of a type by content ([`crate::identity`]) against the two it replaced,
//! kept here as the spec: the code generator's `v2/hashes.rs` `Spell` and this crate's own
//! (the [`Old`] spellings). Within one revision all three tell the same types, constants and
//! argument lists apart and fail on the same ones. Across an edit, the identity follows the
//! code generator's, which a coroutine's binding names are in, and this crate's old one missed
//! them. `is_plain_data` is checked against the code generator's copy the same way.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, ItemKey, coroutine_code, interface_hash, item_key, key, layout_hash};
use hir_def::{AdtId, CallableDefId, ModuleDefId};
use hir_ty::{
    InferBodyId, ParamEnvAndCrate, TyDefId,
    consteval::usize_const,
    db::HirDatabase,
    mir::is_by_move_coroutine,
    next_solver::{
        Binder, Const, ConstKind, DbInterner, GenericArg, GenericArgs, ParamEnv, Pattern, TermId,
        Ty, TyKind, TypingMode, ValTree,
        infer::{DbInternerInferExt, InferCtxt},
    },
};
use proptest::prelude::*;
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, InferConst,
    PatternKind, TermKind, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};
use test_fixture::WithFixture;

use super::{TestDB, alias, declarations, function};
use crate::{code, identity, is_plain_data};

const MINICORE: &str = "sized, copy, clone, drop, fn, send, future, unsize, coerce_unsized";

/// Every kind of type the spelling has an arm for that a fixture can name: by type aliases,
/// and by the locals of the functions' MIR (closures, coroutines, parameters).
const CORPUS: &str = r#"
pub struct P(pub u32);
pub struct Q(pub u32);
pub struct W<T>(pub T);
pub enum E { A(u8), B }
pub struct D(pub u32);
impl Drop for D {
    fn drop(&mut self) {}
}
pub struct HasD { pub d: D }
pub trait Show { fn show(&self) -> u32; }
impl Show for P { fn show(&self) -> u32 { self.0 } }

pub type T0 = bool;
pub type T1 = char;
pub type T2 = u8;
pub type T3 = u32;
pub type T4 = i32;
pub type T5 = f64;
pub type T6 = P;
pub type T7 = Q;
pub type T8 = W<P>;
pub type T9 = W<Q>;
pub type T10 = W<W<P>>;
pub type T11 = E;
pub type T12 = D;
pub type T13 = HasD;
pub type T14 = &'static P;
pub type T15 = &'static mut P;
pub type T16 = *const P;
pub type T17 = *mut P;
pub type T18 = [u8; 2];
pub type T19 = [u8; 3];
pub type T20 = &'static [u8];
pub type T21 = &'static str;
pub type T22 = (u8, P);
pub type T23 = (P, u8);
pub type T24 = ();
pub type T25 = fn(u32) -> u32;
pub type T26 = unsafe fn(u32) -> u32;
pub type T27 = extern "C" fn(u32) -> u32;
pub type T28 = for<'a> fn(&'a u32) -> &'a u32;
pub type T29 = &'static dyn Show;
pub type T30 = &'static (dyn Show + Send);
pub type T31 = &'static (dyn Send + Show);
pub type T32 = W<u8>;
pub type Same8 = W<P>;
pub type Same14 = &'static P;

pub fn closures() -> u32 {
    let add = |x: u32| x + 1;
    let also = |x: u32| x + 1;
    add(1) + also(2)
}
pub fn generic<T: Copy>(x: T) -> (T, T) {
    let pair = (x, x);
    pair
}
pub fn futures() {
    let one = async { let one = 1u32; one };
    let two = async { let two = 2u32; two };
    let _ = (one, two);
}
"#;

const ASYNC: &str = r#"
pub fn make() {
    let fut = async { let one = 1u32; one };
    let _ = fut;
}
"#;

/// The spelling a copy used before there was one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Old {
    /// `hir-ty-codegen`'s `v2/hashes.rs` `Spell` and `adt_content`.
    Codegen,
    /// `hir-ty-share`'s `identity.rs` before it became the one.
    Share,
}

impl Old {
    fn ty_domain(self) -> &'static str {
        match self {
            Old::Codegen => "blaze v2 codegen type 1",
            Old::Share => "blaze v2 share identity 1",
        }
    }

    fn const_domain(self) -> &'static str {
        match self {
            Old::Codegen => "blaze v2 codegen const 1",
            Old::Share => "blaze v2 share identity 1",
        }
    }

    fn adt_domain(self) -> &'static str {
        match self {
            Old::Codegen => "blaze v2 codegen adt 1",
            Old::Share => "blaze v2 share adt 1",
        }
    }
}

fn old_adt(db: &Db, old: Old, adt: AdtId) -> Result<ContentId, String> {
    let describe = |it: blaze_items::Unhashable| it.describe(db);
    let item = ItemId::Adt(adt);
    let mut h = Hasher::new(old.adt_domain());
    h.id(&item_key(db, item).0)
        .id(&interface_hash(db, item).map_err(describe)?.0)
        .id(&layout_hash(db, adt).map_err(describe)?.0);
    let mut seen: FxHashSet<AdtId> = FxHashSet::default();
    seen.insert(adt);
    let mut next = vec![adt];
    let mut dropping: Vec<(ItemKey, ContentId)> = Vec::new();
    while let Some(at) = next.pop() {
        if let Some(imp) = hir_ty::drop::destructor(db, at) {
            let interface = interface_hash(db, ItemId::Impl(imp)).map_err(describe)?;
            dropping.push((item_key(db, ItemId::Adt(at)), interface.0));
        }
        for variant in crate::identity::variants(db, at) {
            for field in db.field_types(variant).values() {
                for arg in field.ty().skip_binder().walk() {
                    if let Some(ty) = arg.ty()
                        && let TyKind::Adt(def, _) = ty.kind()
                        && seen.insert(def.def_id())
                    {
                        next.push(def.def_id());
                    }
                }
            }
        }
    }
    dropping.sort_unstable();
    h.update(&(dropping.len() as u64).to_le_bytes());
    for (key, interface) in &dropping {
        h.id(&key.0).id(interface);
    }
    Ok(h.finish())
}

/// Both old spellings, into one buffer: they differ only where `old` is matched on.
struct OldSpell<'db> {
    db: &'db Db,
    old: Old,
    out: Vec<u8>,
}

impl<'db> OldSpell<'db> {
    fn new(db: &'db Db, old: Old) -> OldSpell<'db> {
        OldSpell { db, old, out: Vec::new() }
    }

    fn ty_of(db: &'db Db, old: Old, ty: Ty<'db>) -> Result<ContentId, String> {
        let mut spell = OldSpell::new(db, old);
        spell.ty(ty)?;
        Ok(ContentId::of(old.ty_domain(), &spell.out))
    }

    fn konst_of(db: &'db Db, old: Old, konst: Const<'db>) -> Result<ContentId, String> {
        let mut spell = OldSpell::new(db, old);
        spell.konst(konst)?;
        Ok(ContentId::of(old.const_domain(), &spell.out))
    }

    /// The old share spelling of an argument list (the code generator had none of its own
    /// beyond `args_of` over its `Hashes`).
    fn args_of(db: &'db Db, args: GenericArgs<'db>) -> Result<ContentId, String> {
        let mut spell = OldSpell::new(db, Old::Share);
        spell.args(args)?;
        Ok(ContentId::of(Old::Share.ty_domain(), &spell.out))
    }

    fn describe(&self) -> impl Fn(blaze_items::Unhashable) -> String + 'db {
        let db = self.db;
        move |it| it.describe(db)
    }

    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.out.extend_from_slice(&(len as u64).to_le_bytes());
    }

    fn id(&mut self, id: &ContentId) {
        self.out.extend_from_slice(&id.0);
    }

    fn str(&mut self, text: &str) {
        self.len(text.len());
        self.out.extend_from_slice(text.as_bytes());
    }

    fn item(&mut self, item: ItemId) {
        let key = item_key(self.db, item);
        self.id(&key.0);
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<(), String> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => self.u8(0),
            TyKind::Char => self.u8(1),
            TyKind::Int(it) => {
                self.u8(2);
                self.str(it.name_str());
            }
            TyKind::Uint(it) => {
                self.u8(3);
                self.str(it.name_str());
            }
            TyKind::Float(it) => {
                self.u8(4);
                self.str(it.name_str());
            }
            TyKind::Adt(def, args) => {
                self.u8(5);
                let content = old_adt(db, self.old, def.def_id())?;
                self.id(&content);
                self.args(args)?;
            }
            TyKind::Foreign(it) => {
                self.u8(6);
                self.item(ItemId::Alias(it.0));
            }
            TyKind::Str => self.u8(7),
            TyKind::Array(element, len) => {
                self.u8(8);
                self.ty(element)?;
                self.konst(len)?;
            }
            TyKind::Pat(base, pattern) => {
                self.u8(9);
                self.ty(base)?;
                self.pattern(pattern)?;
            }
            TyKind::Slice(element) => {
                self.u8(10);
                self.ty(element)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                self.u8(11);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::Ref(_, pointee, mutability) => {
                self.u8(12);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                self.u8(13);
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        self.u8(0);
                        self.item(ItemId::Function(it));
                    }
                    CallableDefId::StructId(it) => {
                        self.u8(1);
                        self.item(ItemId::Adt(it.into()));
                    }
                    CallableDefId::EnumVariantId(it) => {
                        self.u8(2);
                        self.id(&key::variant_key(db, it).0);
                    }
                }
                self.args(args)?;
            }
            TyKind::FnPtr(tys, header) => {
                self.u8(14);
                self.len(tys.bound_vars().len());
                let tys = tys.skip_binder().inputs_and_output;
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
                match self.old {
                    Old::Codegen => self.str(&format!("{:?} {:?}", header.abi(), header.safety())),
                    Old::Share => {
                        self.out.extend_from_slice(format!("{:?}", header.abi()).as_bytes());
                        self.u8(0xff);
                        self.out.extend_from_slice(format!("{:?}", header.safety()).as_bytes());
                        self.u8(0xff);
                    }
                }
                self.u8(header.c_variadic() as u8);
            }
            TyKind::UnsafeBinder(inner) => {
                self.u8(15);
                let binder: Binder<'db, Ty<'db>> = inner.into();
                self.len(binder.bound_vars().len());
                self.ty(binder.skip_binder())?;
            }
            TyKind::Dynamic(predicates, _) => {
                self.u8(16);
                let mut each = Vec::new();
                for predicate in predicates.iter() {
                    let mut inner = OldSpell::new(db, self.old);
                    inner.len(predicate.bound_vars().len());
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            inner.u8(0);
                            inner.item(ItemId::Trait(it.def_id.0));
                            inner.args(it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            inner.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => inner.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => inner.item(ItemId::Const(konst)),
                            }
                            inner.args(it.args)?;
                            match it.term.kind() {
                                TermKind::Ty(ty) => {
                                    inner.u8(0);
                                    inner.ty(ty)?;
                                }
                                TermKind::Const(konst) => {
                                    inner.u8(1);
                                    inner.konst(konst)?;
                                }
                            }
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            inner.u8(2);
                            inner.item(ItemId::Trait(it.0));
                        }
                    }
                    each.push(ContentId::of(self.old.ty_domain(), &inner.out));
                }
                each.sort_unstable();
                self.len(each.len());
                for it in &each {
                    self.id(it);
                }
            }
            TyKind::Closure(id, args) => {
                self.u8(17);
                self.id(&key::closure_key(db, id.0).0);
                self.args(args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                self.u8(18);
                self.id(&key::coroutine_closure_key(db, id.0).0);
                self.args(args)?;
            }
            TyKind::Coroutine(id, args) => {
                self.u8(19);
                self.id(&key::coroutine_key(db, id.0).0);
                let code = match self.old {
                    Old::Codegen => {
                        let code = match is_by_move_coroutine(db, id.0, args.as_coroutine()) {
                            true => code::of_coroutine_by_move(db, id.0),
                            false => code::of_coroutine(db, id.0),
                        };
                        *code.as_ref().map_err(ToString::to_string)?
                    }
                    Old::Share => coroutine_code(db, id.0).map_err(self.describe())?.0,
                };
                self.id(&code);
                self.args(args)?;
            }
            TyKind::CoroutineWitness(id, args) => {
                self.u8(20);
                self.id(&key::coroutine_key(db, id.0).0);
                let code = match self.old {
                    Old::Codegen => *code::of_coroutine(db, id.0).as_ref().map_err(ToString::to_string)?,
                    Old::Share => coroutine_code(db, id.0).map_err(self.describe())?.0,
                };
                self.id(&code);
                self.args(args)?;
            }
            TyKind::Never => self.u8(21),
            TyKind::Tuple(tys) => {
                self.u8(22);
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
            }
            TyKind::Param(param) => {
                self.u8(23);
                self.u32(param.index);
            }
            TyKind::Bound(index, bound) => {
                self.u8(24);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
                self.u8(match bound.kind {
                    BoundTyKind::Anon => 0,
                    BoundTyKind::Param(_) => 1,
                });
            }
            TyKind::Alias(_, alias) => {
                let what = match alias.kind {
                    AliasTyKind::Projection { .. } => "a projection",
                    AliasTyKind::Inherent { .. } => "an inherent associated type",
                    AliasTyKind::Opaque { .. } => "an opaque type",
                    AliasTyKind::Free { .. } => "a type alias",
                };
                return Err(format!("{what} left unnormalized in `{ty:?}`"));
            }
            TyKind::Placeholder(_) => return Err(format!("a placeholder type `{ty:?}`")),
            TyKind::Infer(_) => return Err(format!("an inference variable `{ty:?}`")),
            TyKind::Error(_) => return Err("a type with an error".to_owned()),
        }
        Ok(())
    }

    fn bound_index(&mut self, index: BoundVarIndexKind) {
        match index {
            BoundVarIndexKind::Bound(depth) => {
                self.u8(0);
                self.u32(depth.as_u32());
            }
            BoundVarIndexKind::Canonical => self.u8(1),
        }
    }

    fn args(&mut self, args: GenericArgs<'db>) -> Result<(), String> {
        self.len(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(_) => self.u8(0),
                GenericArgKind::Type(ty) => {
                    self.u8(1);
                    self.ty(ty)?;
                }
                GenericArgKind::Const(konst) => {
                    self.u8(2);
                    self.konst(konst)?;
                }
            }
        }
        Ok(())
    }

    fn pattern(&mut self, pattern: Pattern<'db>) -> Result<(), String> {
        match pattern.kind() {
            PatternKind::Range { start, end } => {
                self.u8(0);
                self.konst(start)?;
                self.konst(end)?;
            }
            PatternKind::Or(patterns) => {
                self.u8(1);
                self.len(patterns.len());
                for it in patterns.iter() {
                    self.pattern(it)?;
                }
            }
            PatternKind::NotNull => self.u8(2),
        }
        Ok(())
    }

    fn konst(&mut self, konst: Const<'db>) -> Result<(), String> {
        match konst.kind() {
            ConstKind::Value(value) => {
                self.u8(0);
                self.ty(value.ty)?;
                self.valtree(value.value)?;
            }
            ConstKind::Param(param) => {
                self.u8(1);
                self.u32(param.index);
            }
            ConstKind::Bound(index, bound) => {
                self.u8(2);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
            }
            ConstKind::Alias(..) => return Err(format!("the constant `{konst:?}` is not evaluated")),
            ConstKind::Expr(_) => return Err(format!("the constant expression `{konst:?}`")),
            ConstKind::Infer(InferConst::Var(_) | InferConst::Fresh(_)) => {
                return Err(format!("an inference variable `{konst:?}`"));
            }
            ConstKind::Placeholder(_) => return Err(format!("a placeholder constant `{konst:?}`")),
            ConstKind::Error(_) => return Err("a constant with an error".to_owned()),
        }
        Ok(())
    }

    fn valtree(&mut self, value: ValTree<'db>) -> Result<(), String> {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                self.u8(0);
                self.out.extend_from_slice(&scalar.size().bytes().to_le_bytes());
                self.out.extend_from_slice(&scalar.to_bits_unchecked().to_le_bytes());
            }
            ValTreeKind::Branch(consts) => {
                self.u8(1);
                self.len(consts.len());
                for it in consts.iter() {
                    self.konst(it)?;
                }
            }
        }
        Ok(())
    }
}

/// The code generator's `FunctionCx::is_plain_data` before there was one, with its
/// `normalize`: the spec [`is_plain_data`] is held to.
fn old_is_plain_data<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &InferCtxt<'db>,
    param_env: ParamEnv<'db>,
    ty: Ty<'db>,
    depth: u32,
) -> bool {
    if depth > 8 {
        return false;
    }
    let ty = match ty.kind() {
        TyKind::Alias(..) => hir_ty::next_solver::obligation_ctxt::ObligationCtxt::new(infcx)
            .structurally_normalize_ty(
                &hir_ty::next_solver::infer::traits::ObligationCause::dummy(),
                param_env,
                ty,
            )
            .unwrap_or(ty),
        _ => ty,
    };
    match ty.kind() {
        TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Never
        | TyKind::RawPtr(..)
        | TyKind::Ref(..)
        | TyKind::FnPtr(..)
        | TyKind::FnDef(..) => true,
        TyKind::Array(element, _) | TyKind::Pat(element, _) => {
            old_is_plain_data(db, interner, infcx, param_env, element, depth + 1)
        }
        TyKind::Tuple(fields) => fields
            .iter()
            .all(|it| old_is_plain_data(db, interner, infcx, param_env, it, depth + 1)),
        TyKind::Adt(def, args) => {
            let variants: Vec<hir_def::VariantId> = match def.def_id() {
                AdtId::StructId(id) => vec![id.into()],
                AdtId::EnumId(id) => {
                    id.enum_variants(db).variants.iter().map(|(_, it)| it.0.into()).collect()
                }
                AdtId::UnionId(_) => return false,
            };
            variants.into_iter().all(|variant| {
                db.field_types(variant).iter().all(|(_, field)| {
                    let ty = field.ty().instantiate(interner, args).skip_norm_wip();
                    old_is_plain_data(db, interner, infcx, param_env, ty, depth + 1)
                })
            })
        }
        _ => false,
    }
}

fn fixture(text: &str) -> TestDB {
    TestDB::with_single_file(&format!("//- minicore: {MINICORE}\n{text}")).0
}

/// The types of the corpus: each type alias's, then the type of each local of each function.
fn corpus<'db>(db: &'db TestDB) -> Vec<Ty<'db>> {
    let mut tys = Vec::new();
    for def in declarations(db) {
        match def {
            ModuleDefId::TypeAliasId(it) => tys.push(db.ty(TyDefId::TypeAliasId(it)).skip_binder()),
            ModuleDefId::FunctionId(it) => {
                let body = db
                    .mir_body(InferBodyId::DefWithBodyId(it.into()))
                    .unwrap_or_else(|e| panic!("the MIR of {it:?}: {e:?}"));
                tys.extend(body.locals.iter().map(|(_, local)| local.ty.as_ref()));
            }
            _ => {}
        }
    }
    tys
}

/// Whether `a` and `b` fail on the same entries and tell the same pairs apart.
#[track_caller]
fn agree<A: PartialEq, B: PartialEq>(what: &str, names: &[String], a: &[Option<A>], b: &[Option<B>]) {
    assert_eq!(a.len(), b.len());
    for i in 0..a.len() {
        assert_eq!(a[i].is_some(), b[i].is_some(), "{what}: `{}` is spelled by one only", names[i]);
        for j in 0..i {
            if let (Some(ai), Some(aj), Some(bi), Some(bj)) = (&a[i], &a[j], &b[i], &b[j]) {
                assert_eq!(ai == aj, bi == bj, "{what}: `{}` against `{}`", names[i], names[j]);
            }
        }
    }
}

/// Every spelling of every type of `tys`: the one, then the code generator's and share's old.
fn spellings<'db>(db: &'db Db, tys: &[Ty<'db>]) -> [Vec<Option<ContentId>>; 3] {
    [
        tys.iter().map(|&ty| identity::ty(db, ty).ok()).collect(),
        tys.iter().map(|&ty| OldSpell::ty_of(db, Old::Codegen, ty).ok()).collect(),
        tys.iter().map(|&ty| OldSpell::ty_of(db, Old::Share, ty).ok()).collect(),
    ]
}

#[track_caller]
fn all_agree<'db>(db: &'db Db, tys: &[Ty<'db>]) {
    let names: Vec<String> = tys.iter().map(|ty| format!("{ty:?}")).collect();
    let [one, codegen, share] = spellings(db, tys);
    agree("the code generator's old spelling", &names, &one, &codegen);
    agree("share's old spelling", &names, &one, &share);
}

#[test]
fn the_identity_tells_apart_what_both_old_spellings_did() {
    let db = fixture(CORPUS);
    hir_ty::attach_db(&db, || {
        let tys = corpus(&db);
        all_agree(&db, &tys);
        let spelled = |name: &str| {
            identity::ty(&db, db.ty(TyDefId::TypeAliasId(alias(&db, name))).skip_binder())
                .unwrap_or_else(|why| panic!("`{name}`: {why}"))
        };
        assert_eq!(spelled("T8"), spelled("Same8"), "an alias is what it stands for");
        assert_eq!(spelled("T14"), spelled("Same14"));
        assert_eq!(spelled("T30"), spelled("T31"), "the order of a trait object's bounds");
        assert_ne!(spelled("T22"), spelled("T23"), "the order of a tuple's fields");
        assert_ne!(spelled("T25"), spelled("T26"), "a function pointer's safety");
        assert_ne!(spelled("T25"), spelled("T27"), "a function pointer's ABI");
        assert_ne!(spelled("T12"), spelled("T6"), "a drop impl");
    });
}

#[test]
fn constants_and_argument_lists_agree_with_the_old_spellings() {
    let db = fixture(CORPUS);
    hir_ty::attach_db(&db, || {
        let krate = db.test_crate();
        let konsts: Vec<Const<'_>> =
            [0, 1, 1, 2, u32::MAX as u128].map(|it| usize_const(&db, Some(it), krate)).to_vec();
        let names: Vec<String> = konsts.iter().map(|it| format!("{it:?}")).collect();
        let one: Vec<_> = konsts.iter().map(|&it| identity::konst(&db, it).ok()).collect();
        for old in [Old::Codegen, Old::Share] {
            let then: Vec<_> = konsts.iter().map(|&it| OldSpell::konst_of(&db, old, it).ok()).collect();
            agree(&format!("{old:?}'s constants"), &names, &one, &then);
        }

        let tys = corpus(&db);
        let lists: Vec<GenericArgs<'_>> = tys
            .iter()
            .flat_map(|&a| {
                [
                    GenericArgs::new_from_slice(&[GenericArg::from(a)]),
                    GenericArgs::new_from_slice(&[GenericArg::from(a), GenericArg::from(konsts[1])]),
                ]
            })
            .collect();
        let names: Vec<String> = lists.iter().map(|it| format!("{it:?}")).collect();
        let one: Vec<_> = lists.iter().map(|&it| identity::args(&db, it).ok()).collect();
        let then: Vec<_> = lists.iter().map(|&it| OldSpell::args_of(&db, it).ok()).collect();
        agree("share's argument lists", &names, &one, &then);
    });
}

/// The type of the one local of `func`'s MIR that is a coroutine.
fn coroutine_local<'db>(db: &'db TestDB, func: &str) -> Ty<'db> {
    let body = db
        .mir_body(InferBodyId::DefWithBodyId(function(db, func).into()))
        .unwrap_or_else(|e| panic!("the MIR of `{func}`: {e:?}"));
    let found: Vec<Ty<'db>> = body
        .locals
        .iter()
        .map(|(_, local)| local.ty.as_ref())
        .filter(|ty| matches!(ty.kind(), TyKind::Coroutine(..)))
        .collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} coroutine locals in `{func}`", found.len()),
    }
}

/// Where the old spellings differed on purpose, and the one follows the finer: across a rename
/// of a binding a coroutine keeps, which its debug info names, the code generator's old
/// spelling and the one change, and share's old one did not (the missed input).
#[test]
fn a_coroutines_identity_holds_its_binding_names() {
    let (mut db, file) = TestDB::with_single_file(&format!("//- minicore: {MINICORE}\n{ASYNC}"));
    let file = file.file_id(&db);
    let spelled = |db: &TestDB| {
        hir_ty::attach_db(db, || {
            let ty = coroutine_local(db, "make");
            (
                identity::ty(db, ty).expect("the one spelling"),
                OldSpell::ty_of(db, Old::Codegen, ty).expect("the code generator's"),
                OldSpell::ty_of(db, Old::Share, ty).expect("share's"),
            )
        })
    };
    let before = spelled(&db);
    assert_eq!(spelled(&db), before, "asked twice");

    db.set_file_text(file, &ASYNC.replace("one", "uno"));
    let after = spelled(&db);
    assert_ne!(after.0, before.0, "the one spelling holds the coroutine's binding names");
    assert_ne!(after.1, before.1, "as the code generator's did");
    assert_eq!(after.2, before.2, "share's old spelling missed them");
}

#[test]
fn is_plain_data_is_the_code_generators() {
    let db = fixture(CORPUS);
    hir_ty::attach_db(&db, || {
        let krate = db.test_crate();
        let interner = DbInterner::new_with(&db, krate);
        let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
        let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
        let mut plain = 0;
        for ty in corpus(&db) {
            let one = is_plain_data(&db, interner, &infcx, env.param_env, ty);
            let old = old_is_plain_data(&db, interner, &infcx, env.param_env, ty, 0);
            assert_eq!(one, old, "`{ty:?}`");
            plain += usize::from(one);
        }
        assert!(plain > 0, "the corpus has plain data");
    });
}

/// A type written in the fixture's language: scalars, `P` and `Q` (one `u32` each), and what
/// references, pointers, arrays, tuples, `W<_>` and function pointers make of them.
fn ty_text() -> impl Strategy<Value = String> {
    let leaf = prop::sample::select(vec!["u8", "u32", "i64", "bool", "char", "P", "Q", "()", "&'static str"])
        .prop_map(str::to_owned);
    leaf.prop_recursive(3, 24, 2, |inner| {
        prop_oneof![
            inner.clone().prop_map(|t| format!("&'static {t}")),
            inner.clone().prop_map(|t| format!("&'static mut {t}")),
            inner.clone().prop_map(|t| format!("*const {t}")),
            (inner.clone(), 0usize..3).prop_map(|(t, n)| format!("[{t}; {n}]")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a}, {b})")),
            inner.clone().prop_map(|t| format!("W<{t}>")),
            (inner.clone(), inner).prop_map(|(a, b)| format!("fn({a}) -> {b}")),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Random types, each written twice under two names, agree with both old spellings.
    #[test]
    fn random_types_agree_with_both_old_spellings(texts in prop::collection::vec(ty_text(), 1..10)) {
        let mut program = String::from("pub struct P(pub u32);\npub struct Q(pub u32);\npub struct W<T>(pub T);\n");
        for (at, text) in texts.iter().enumerate() {
            program.push_str(&format!("pub type A{at} = {text};\npub type B{at} = {text};\n"));
        }
        let db = fixture(&program);
        hir_ty::attach_db(&db, || {
            let tys: Vec<Ty<'_>> = (0..texts.len())
                .flat_map(|at| [format!("A{at}"), format!("B{at}")])
                .map(|name| db.ty(TyDefId::TypeAliasId(alias(&db, &name))).skip_binder())
                .collect();
            all_agree(&db, &tys);
            for pair in tys.chunks(2) {
                assert_eq!(identity::ty(&db, pair[0]).ok(), identity::ty(&db, pair[1]).ok());
            }
        });
    }
}
```

What the tests check:

- Over the fixed corpus (every `TyKind` arm a fixture can name, closures, coroutines, generic
  parameters, opaque types) and over random types, the one spelling tells the same pairs
  apart as each old spelling, and fails on the same types.
- Constants, and argument lists against share's old spelling, the same way.
- The one deliberate difference, across an edit: a coroutine's identity follows the code
  generator's, and share's old spelling is shown to have missed the binding names.
- `is_plain_data` equals the code generator's old copy on every corpus type.

`hir-ty-codegen/v2/hashes/tests.rs` keeps its tests unchanged, and they now run over this
spelling through `ItemHashes`. That is lane codegen's half of the request.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

### Done when

- `rg 'is_plain_data|struct Spell|fn with_spans|fn adt_content' crates/hir-ty-codegen` finds
  only the calls into `hir_ty_share` (after codegen's half).
- `rg 'IDENTITY_DOMAIN' crates/` finds nothing.

> forwarded to fastcompile (from requests, pass 16; told by tmux).

> applied by fastcompile-ident-share, 2026-10-29: steps 1 to 7 as written (`code.rs`,
> `identity.rs`, `lib.rs`, `fact.rs`'s one `is_plain_data`, `proptest`, `tests/identity.rs`).
> `rg IDENTITY_DOMAIN crates/` finds nothing. Codegen's half (`hashes.rs` onto `hir_ty_share::{identity, code, is_plain_data}`) can go now.


## dedup: walk.rs's `body_of` is `MirStage::compiled` (from dedup-sim-front, 2026-10-29)

After `v2/requests/db-ty.md` "## dedup: one driver under the 24 MIR queries (from
dedup-sim-front, 2026-10-29)". `body_of` and `is_runtime` copied codegen's
`compiled_mir_body{,_for_closure,_for_coroutine}` and `is_runtime`. Both now have one home in
hir-ty: `MirStage::compiled` and `MirOptions::is_runtime`. Codegen's copies go in
`v2/requests/codegen.md` under the same heading, so the walk and the code generator can't drift.

`crates/hir-ty-share/src/walk.rs`:
- `mir::{MirOptLevel, MirOptions},` (line 46) becomes `mir::{MirOptions, MirOwner, MirStage},`.
- Delete `is_runtime` (line ~204, with its doc comment).
- Replace the body of `body_of` (the `let body = match def { ... };` and the final
  `body.map_err(...)`) with:

```rust
    let owner = match def {
        ShareDef::Fn(func) => MirOwner::Body(InferBodyId::DefWithBodyId(func.into())),
        ShareDef::Closure(id) => MirOwner::Closure(InternedClosureId::from_id(id)),
        ShareDef::Coroutine(id) => MirOwner::Coroutine(InternedCoroutineId::from_id(id)),
        ShareDef::CoroutineByMove(id) => {
            MirOwner::CoroutineByMove(InternedCoroutineId::from_id(id))
        }
        ShareDef::DropGlue
        | ShareDef::CallOnceShim
        | ShareDef::VtableShim(_)
        | ShareDef::FnPointerShim
        | ShareDef::ReifyShim { .. } => return Err(Unshared::Shim("a shim, which has no generic body")),
    };
    db.mir(owner, MirStage::compiled(owner, options))
        .map_err(|error| Unshared::NoMir(format!("{error:?}"))) // alloc: error path
```

Same bodies for every `(def, options)`: functions and closures runtime iff
`level == Off && !ub_checks`, coroutines runtime iff `!ub_checks`, else optimized with
`options`. `crates/hir-ty-mir/tests/mir/stages.rs` (codegen request) checks `MirStage::compiled`
against that rule.

> forwarded to fastcompile-ident-share (fastcompile-ident-share-next2); told by tmux (requests, pass 28).

> waiting (fastcompile-ident-share, 2026-10-29 22:10): db-ty's "one driver under the 24 MIR
> queries" is not applied yet (no `MirStage`, `MirOwner` or `Db::mir` in `crates/`). walk.rs is
> unchanged; apply as written once it lands. Tell fastcompile-ident-share by tmux then.

> applied by unify-front-mir (queue 00305), checked by fastcompile-ident-share (2026-10-30
> 05:00, not built): `body_of` is `db.mir(owner, MirStage::compiled(owner, options))`, and
> `is_runtime` and the `MirOptLevel` import are gone from walk.rs. `HirDatabase::mir` returns
> `Result<&MirBody, MirLowerError>`, which matches.
