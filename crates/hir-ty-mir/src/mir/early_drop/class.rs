//! Whether a value of a type may be dropped early (docs/v2/plan/design/early-drop.md, section
//! 1): [`drop_class`], decided on the type alone, without reading a body, so it is the same in
//! every crate and asking it while a body is built asks for no other body.
//!
//! - A type with no `Drop` impl of its own is as its parts are.
//! - A toolchain type with a `Drop` impl is early when it is one of [`OWNING`], its elements
//!   are early or have nothing to drop, and its allocator is `Global`; one of [`SHARED`] is
//!   scoped (its drop writes a count other values read); any other is scoped.
//! - Any other type with a `Drop` impl is scoped unless it says `#[blaze::drop(early)]`, which
//!   `hir_ty_pure::drop_effects` checks against the impl's body.
//! - `#[blaze::drop(scoped)]` makes the type, and whatever holds one, scoped.
//! - A type parameter, an alias the solver can't see through, `dyn`, a coroutine: scoped.

use base_db::{CrateOrigin, Db, LangCrateOrigin};
use blaze_mem::Frame;
use hir_def::{
    AdtId, AttrDefId, HasModule,
    attrs::AttrFlags,
    signatures::{EnumSignature, StructSignature, UnionSignature},
};
use hir_ty::{
    FrameSet,
    db::HirDatabase,
    drop::{DropGlue, destructor, has_drop_glue},
    frame_set,
    next_solver::{GenericArgs, ParamEnv, Ty, TyKind, infer::InferCtxt},
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};

use crate::mir::{dispatch::variants, normalize_or_keep};

/// When a value of a type is dropped under `#![blaze::early_drop]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropClass {
    /// Dropping it runs nothing.
    None,
    /// After its last use.
    Early,
    /// Where its scope ends, as in Rust.
    Scoped(Scoped),
}

/// Why a type keeps its scope's drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scoped {
    /// The type says `#[blaze::drop(scoped)]`.
    Marked(AdtId),
    /// The type has a `Drop` impl and is not marked `#[blaze::drop(early)]`.
    OwnDrop(AdtId),
    /// A toolchain type whose drop other values can see: `Rc`, `Arc`, their `Weak`s.
    Shared(AdtId),
    /// A toolchain type with a `Drop` impl that [`OWNING`] does not list.
    NotListed(AdtId),
    /// A toolchain container with an allocator other than `Global`.
    Allocator(AdtId),
    /// Only an instance decides: a type parameter, an alias, `dyn`, a coroutine.
    Open,
}

impl Scoped {
    /// Why, said of a value of the type: "its type `Guard` has a `Drop` impl ..".
    pub fn said(self, db: &Db) -> String {
        let name = |adt| adt_name(db, adt);
        // alloc: a diagnostic's text, on the error path
        match self {
            Scoped::Marked(adt) => format!("`{}` is `#[blaze::drop(scoped)]`", name(adt)),
            Scoped::OwnDrop(adt) => format!(
                "`{}` has a `Drop` impl and is not `#[blaze::drop(early)]`",
                name(adt)
            ),
            Scoped::Shared(adt) => format!(
                "`{}` is shared: its drop changes a count that other values read",
                name(adt)
            ),
            Scoped::NotListed(adt) => format!(
                "`{}` is a toolchain type whose destructor is not known to only free memory",
                name(adt)
            ),
            Scoped::Allocator(adt) => {
                format!("`{}` frees through an allocator other than `Global`", name(adt))
            }
            Scoped::Open => {
                "its type is a type parameter, an opaque type, `dyn` or a coroutine".to_owned()
            }
        }
    }
}

/// A toolchain type whose drop glue frees what it owns and drops its elements: its name, its
/// crate, and how many of its type arguments are elements (the rest is its allocator).
struct Owning {
    krate: LangCrateOrigin,
    name: &'static str,
    elements: usize,
}

const fn owning(krate: LangCrateOrigin, name: &'static str, elements: usize) -> Owning {
    Owning { krate, name, elements }
}

/// The toolchain's owning containers. `HashMap` and `HashSet` count their hasher as an
/// element: it is a field, dropped as one.
const OWNING: &[Owning] = &[
    owning(LangCrateOrigin::Alloc, "Box", 1),
    owning(LangCrateOrigin::Alloc, "Vec", 1),
    owning(LangCrateOrigin::Alloc, "VecDeque", 1),
    owning(LangCrateOrigin::Alloc, "String", 0),
    owning(LangCrateOrigin::Alloc, "BTreeMap", 2),
    owning(LangCrateOrigin::Alloc, "BTreeSet", 1),
    owning(LangCrateOrigin::Alloc, "BinaryHeap", 1),
    owning(LangCrateOrigin::Alloc, "LinkedList", 1),
    owning(LangCrateOrigin::Alloc, "CString", 0),
    owning(LangCrateOrigin::Std, "HashMap", 3),
    owning(LangCrateOrigin::Std, "HashSet", 2),
];

/// The toolchain's shared-ownership types, by name, in `alloc`.
const SHARED: &[&str] = &["Rc", "Arc", "Weak", "UniqueRc", "UniqueArc"];

/// The name of `adt`, as written where it is declared.
pub fn adt_name(db: &Db, adt: AdtId) -> &str {
    match adt {
        AdtId::StructId(it) => StructSignature::of(db, it).name.as_str(),
        AdtId::EnumId(it) => EnumSignature::of(db, it).name.as_str(),
        AdtId::UnionId(it) => UnionSignature::of(db, it).name.as_str(),
    }
}

/// The toolchain crate `adt` is declared in, if it is in one.
fn toolchain(db: &Db, adt: AdtId) -> Option<LangCrateOrigin> {
    match adt.krate(db).data(db).origin {
        CrateOrigin::Lang(lang) => Some(lang),
        _ => None,
    }
}

/// What `#[blaze::drop(..)]` on `adt` says: `Some(true)` early, `Some(false)` scoped. Both
/// words at once are scoped, the answer that changes nothing (`hir_ty_pure::drop_effects`
/// reports the pair).
pub fn drop_attr(db: &Db, adt: AdtId) -> Option<bool> {
    let flags = AttrFlags::query(db, AttrDefId::AdtId(adt));
    if flags.contains(AttrFlags::DROP_SCOPED) {
        Some(false)
    } else if flags.contains(AttrFlags::DROP_EARLY) {
        Some(true)
    } else {
        None
    }
}

/// When a value of `ty` is dropped, in `env`.
pub fn drop_class<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    ty: Ty<'db>,
    frame: &Frame,
) -> DropClass {
    let mut cx = Cx { infcx, env, visited: frame_set(frame) };
    cx.class(ty)
}

struct Cx<'a, 'f, 'db> {
    infcx: &'a InferCtxt<'db>,
    env: ParamEnv<'db>,
    /// The types on the way down: one met again is decided by its other parts.
    visited: FrameSet<'f, Ty<'db>>,
}

/// Scoped if either is, for the first reason found; else early if either is.
fn both(a: DropClass, b: DropClass) -> DropClass {
    match (a, b) {
        (DropClass::Scoped(why), _) | (_, DropClass::Scoped(why)) => DropClass::Scoped(why),
        (DropClass::Early, _) | (_, DropClass::Early) => DropClass::Early,
        (DropClass::None, DropClass::None) => DropClass::None,
    }
}

impl<'db> Cx<'_, '_, 'db> {
    /// One level per level of `ty`, each with stack to spare (rule 20).
    fn class(&mut self, ty: Ty<'db>) -> DropClass {
        stdx::ensure_sufficient_stack(|| self.level(ty))
    }

    fn level(&mut self, ty: Ty<'db>) -> DropClass {
        let ty = normalize_or_keep(self.infcx, self.env, ty);
        if let TyKind::Adt(def, _) = ty.kind()
            && drop_attr(self.infcx.interner.db, def.def_id()) == Some(false)
        {
            return DropClass::Scoped(Scoped::Marked(def.def_id()));
        }
        if has_drop_glue(self.infcx, ty, self.env) == DropGlue::None {
            return DropClass::None;
        }
        if !self.visited.insert(ty) {
            return DropClass::None;
        }
        match ty.kind() {
            TyKind::Adt(def, args) => self.adt(def.def_id(), args),
            TyKind::Tuple(fields) => {
                fields.iter().fold(DropClass::None, |all, it| both(all, self.class(it)))
            }
            TyKind::Array(element, _) | TyKind::Slice(element) => self.class(element),
            TyKind::Closure(_, args) => self.class(args.as_closure().tupled_upvars_ty()),
            _ => DropClass::Scoped(Scoped::Open),
        }
    }

    fn adt(&mut self, adt: AdtId, args: GenericArgs<'db>) -> DropClass {
        let db = self.infcx.interner.db;
        let lang = toolchain(db, adt);
        let name = adt_name(db, adt);
        if lang == Some(LangCrateOrigin::Alloc) && SHARED.contains(&name) {
            return DropClass::Scoped(Scoped::Shared(adt));
        }
        if destructor(db, adt).is_none() {
            // Its glue is its fields'. A box's pointee is no field of it, and a box always
            // frees, so it never comes here: it has drop glue of its own.
            let mut all = DropClass::None;
            for variant in variants(db, adt) {
                for (_, field) in db.field_types(variant).iter() {
                    let field = field.ty().instantiate(self.infcx.interner, args).skip_norm_wip();
                    all = both(all, self.class(field));
                    if matches!(all, DropClass::Scoped(_)) {
                        return all;
                    }
                }
            }
            return all;
        }
        if drop_attr(db, adt) == Some(true) {
            // The mark is of the impl; what the type is given to hold is each use's.
            return args.types().fold(DropClass::Early, |all, it| both(all, self.class(it)));
        }
        let Some(lang) = lang else { return DropClass::Scoped(Scoped::OwnDrop(adt)) };
        let Some(row) = OWNING.iter().find(|it| it.krate == lang && it.name == name) else {
            return DropClass::Scoped(Scoped::NotListed(adt));
        };
        let mut all = DropClass::Early;
        for (at, arg) in args.types().enumerate() {
            if at < row.elements {
                all = both(all, self.class(arg));
            } else if !is_global(db, arg) {
                return DropClass::Scoped(Scoped::Allocator(adt));
            }
            if matches!(all, DropClass::Scoped(_)) {
                return all;
            }
        }
        all
    }
}

/// Whether `ty` is alloc's `Global`, the one allocator whose frees the rule allows.
fn is_global(db: &Db, ty: Ty<'_>) -> bool {
    match ty.kind() {
        TyKind::Adt(def, _) => {
            let adt = def.def_id();
            toolchain(db, adt) == Some(LangCrateOrigin::Alloc) && adt_name(db, adt) == "Global"
        }
        _ => false,
    }
}

/// Whether a value of `ty` may be, or hold, a raw pointer the borrow checker does not follow:
/// `ty` is or names `*const`, `*mut` or `NonNull` in its arguments, or is an ADT outside the
/// toolchain with such a field, recursively. A toolchain type other than `NonNull` owns what
/// its pointers point to (`Vec`, `Box`, `String`), so its fields are not looked into: a
/// `v.clone()` is no pointer into `v`.
pub fn may_hold_raw<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    ty: Ty<'db>,
    frame: &Frame,
) -> bool {
    let mut visited = frame_set(frame);
    holds_raw(infcx, env, ty, &mut visited)
}

fn holds_raw<'db>(
    infcx: &InferCtxt<'db>,
    env: ParamEnv<'db>,
    ty: Ty<'db>,
    visited: &mut FrameSet<'_, Ty<'db>>,
) -> bool {
    let ty = normalize_or_keep(infcx, env, ty);
    if !visited.insert(ty) {
        return false;
    }
    let db = infcx.interner.db;
    stdx::ensure_sufficient_stack(|| match ty.kind() {
        TyKind::RawPtr(..) => true,
        TyKind::Ref(_, inner, _) => holds_raw(infcx, env, inner, visited),
        TyKind::Tuple(fields) => fields.iter().any(|it| holds_raw(infcx, env, it, visited)),
        TyKind::Array(element, _) | TyKind::Slice(element) => {
            holds_raw(infcx, env, element, visited)
        }
        TyKind::Closure(_, args) => {
            holds_raw(infcx, env, args.as_closure().tupled_upvars_ty(), visited)
        }
        TyKind::Adt(def, args) => {
            let adt = def.def_id();
            if infcx.interner.lang_items().NonNull.is_some_and(|it| AdtId::StructId(it) == adt)
                || args.types().any(|it| holds_raw(infcx, env, it, visited))
            {
                return true;
            }
            if toolchain(db, adt).is_some() {
                return false;
            }
            variants(db, adt).into_iter().any(|variant| {
                db.field_types(variant).iter().any(|(_, field)| {
                    let field = field.ty().instantiate(infcx.interner, args).skip_norm_wip();
                    holds_raw(infcx, env, field, visited)
                })
            })
        }
        _ => false,
    })
}
