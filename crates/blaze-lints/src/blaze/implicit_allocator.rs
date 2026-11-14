//! `implicit_allocator`: a value whose allocator is left to its default, `Global` (AGENTS.md
//! rule 11, bump-allocate: first, don't allocate where a borrow, a fixed buffer or the
//! caller's buffer does; where an allocation is needed, name its allocator, `Global` included).
//!
//! No comment keeps a finding: the global heap is written where it is used
//! (`Vec::new_in(Global)`), or the item says why with `#[expect(blaze::implicit_allocator,
//! reason = "..")]`, which the ratchet counts. Each call finding's note leads with the form
//! that allocates nothing where the site has one (a borrow for `to_owned`, `clone` and
//! `to_vec`; `blaze_mem::format_into!` for `format!` and `to_string`; an array for a
//! `vec![..]` of elements; the iterator itself for `collect()`), then the allocator to name.
//!
//! A machine-applicable fix rewrites a constructor to its `_in` form with `Global`
//! ([`explicit_fix`]): it keeps the meaning, as `Vec<T>` is `Vec<T, Global>`, so no
//! signature changes; not allocating, or a frame, is the better change, made by hand.
//!
//! A dependency's type with no allocator parameter is judged by its fields, walked
//! transitively and cached per type (`CString`, `BufWriter`, `Command`, channels, `Regex`):
//! one that holds a global allocation is a finding as `String` is. A `Cow` of `str`, a slice,
//! `Path`, `OsStr` or `CStr` is one too: its `Owned` allocates. A copying method (`clone`,
//! `to_owned`, `to_string`, `to_vec`) makes its value at the call though its receiver holds
//! the type; a clone of an `Arc` or `Rc` shares and makes none.
//!
//! A type is implicit when it is, or holds, an ADT whose allocator parameter has `Global` as
//! its argument (`Vec<String>` is `Vec<String, Global>`), the parameter found by what it is
//! (bounded by `core::alloc::Allocator`, or defaulting to `Global`), never by its name; or one
//! of std's types that allocate from the global heap and take no allocator at all: `String`,
//! `PathBuf`, `OsString`, and std's `HashMap` and `HashSet`. `blaze_hash`'s maps and sets
//! (`blaze_hash::Map`, under `HashMap<K, V, A>` and the rest) have an allocator parameter, so
//! they are judged by its argument like `Vec`. papaya's shared tables (`blaze_hash::SharedMap`
//! and the rest) take no allocator and are not on that list: a table shared between threads is
//! long-lived (a daemon's, a view's), so the global heap is its lifetime's allocator.
//!
//! Three places are checked, each reported once, at the outermost spot:
//! - a written type (a signature, a field, an alias, a static or const, an impl header, a
//!   bound, a parameter's default, a closure's parameter, a cast), at its outermost type node:
//!   `Vec<String>` is one finding, not two. `Self` is the impl header's finding;
//! - a call whose value or generic arguments hold an implicit type that none of its inputs
//!   (receiver and arguments) holds: the call chose the global allocator (`Vec::new()`,
//!   `vec![]`, `collect()`, `Box::new(..)`, `String::from(..)`, a turbofish). A call that hands
//!   one on (`opt.unwrap()`, `v.clone()`, `mem::take(..)`) is not one: the value is found where
//!   it was made. A call of a workspace function whose own return type holds one is found at
//!   that signature, not at each caller;
//! - a `let`: with a written type, when that type is implicit; without one, when its binding
//!   holds what a call in its initializer made. A finding inside another (the call in a
//!   `let`'s initializer) is dropped for the outer one.
//!
//! A type behind a reference, pointer, function or `dyn` counts in a written type only: a
//! call's value of `&Vec<u8>` borrows a value made elsewhere.
//!
//! No machine-applicable fix: `blaze_mem::frame::current` lends the frame to a closure, so
//! `Vec::new()` has no rewrite in place that is sound at every call.

use std::{cell::RefCell, ops::ControlFlow};

use base_db::Db;
use blaze_hash::HashMap;
use hir::HirDisplay as _;
use hir_def::{AdtId, HasModule as _, hir::Expr};
use hir_ty::next_solver::{
    AliasTyKind, DbInterner, OpaqueTyIdWrapper, Ty, TyKind, ensure_sufficient_stack,
};
use rustc_type_ir::{
    TypeSuperVisitable as _, TypeVisitable as _, TypeVisitor, inherent::IntoKind as _,
};
use std::alloc::Global;
use syntax::{SyntaxKind as K, TextRange, WalkEvent, view as ast};

use ast::{AstNode as _, HasName as _};

use super::{Known, Lint, exempt};
use crate::check::Cx;

/// The rule every message ends with.
const RULE: &str = "don't allocate where a borrow, a fixed buffer or the caller's buffer does; \
                    where an allocation is needed, every value names its allocator: `Vec::new_in(frame)` from \
                    `blaze_mem::frame::current`, the request/revision/image allocator, or \
                    `Vec::new_in(Global)` for the global heap";

/// std's allocator, its trait, and the types that allocate from the global heap with no
/// parameter naming an allocator; with each ADT's allocator parameter as it is asked for.
pub(super) struct Allocators {
    global: Option<AdtId>,
    allocator: Option<hir::Trait>,
    string: Option<AdtId>,
    path_buf: Option<AdtId>,
    os_string: Option<AdtId>,
    map: Option<AdtId>,
    set: Option<AdtId>,
    /// `Cow`, whose `Owned` is a `String`, `Vec`, `PathBuf`, `OsString` or `CString` when what
    /// it borrows is unsized, and the unsized types of std whose owned form allocates.
    cow: Option<AdtId>,
    path: Option<AdtId>,
    os_str: Option<AdtId>,
    c_str: Option<AdtId>,
    /// `Arc` and `Rc`: a clone of one shares, it does not copy.
    shared: [Option<AdtId>; 2],
    /// The traits whose methods copy what the receiver holds into a new allocation.
    copying: [Option<hir::Trait>; 3],
    /// Each ADT's allocator parameter: its place among the ADT's type parameters and its name.
    params: RefCell<HashMap<AdtId, Option<(usize, hir::Name)>, Global>>,
    /// Each dependency ADT with no allocator parameter, by whether a field of it holds a
    /// global allocation: the field and the type found in it, walked once per ADT. An ADT
    /// being walked reads as holding none, which ends a walk around a recursive type.
    held: RefCell<HashMap<AdtId, Option<(hir::Name, AdtId)>, Global>>,
}

/// A type of std's that takes no allocator.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Heap {
    String,
    PathBuf,
    OsString,
    Map,
}

/// Why a type is implicit.
#[derive(Clone)]
enum Implicit {
    /// The allocator parameter `param` of `adt` has `Global` as its argument.
    Default { adt: AdtId, param: hir::Name },
    /// `adt` takes no allocator: it always allocates from the global heap.
    Heap { adt: AdtId, heap: Heap },
    /// `adt`, a dependency's type with no allocator parameter, holds in its field `field` an
    /// `inner` that allocates from the global heap (`BufWriter`'s buffer, `Command`'s
    /// arguments, `CString`'s bytes).
    Holds { adt: AdtId, field: hir::Name, inner: AdtId },
    /// A `Cow` of an unsized type, whose `Owned` is `owned`.
    Cow { adt: AdtId, owned: &'static str },
}

impl Implicit {
    /// The ADT the finding is of.
    fn adt(&self) -> AdtId {
        match *self {
            Implicit::Default { adt, .. }
            | Implicit::Heap { adt, .. }
            | Implicit::Holds { adt, .. }
            | Implicit::Cow { adt, .. } => adt,
        }
    }
}

impl Allocators {
    pub(super) fn new(cx: &Cx<'_, '_>) -> Allocators {
        let adt = |path: &[&str]| match cx.std_def(path) {
            Some(hir::ModuleDef::Adt(it)) => Some(AdtId::from(it)),
            _ => None,
        };
        Allocators {
            global: adt(&["alloc", "alloc", "Global"]).or_else(|| adt(&["std", "alloc", "Global"])),
            allocator: cx
                .std_trait(&["core", "alloc", "Allocator"])
                .or_else(|| cx.std_trait(&["std", "alloc", "Allocator"])),
            string: adt(&["alloc", "string", "String"])
                .or_else(|| adt(&["std", "string", "String"])),
            path_buf: adt(&["std", "path", "PathBuf"]),
            os_string: adt(&["std", "ffi", "OsString"]),
            map: adt(&["std", "collections", "HashMap"]),
            set: adt(&["std", "collections", "HashSet"]),
            cow: adt(&["alloc", "borrow", "Cow"]).or_else(|| adt(&["std", "borrow", "Cow"])),
            path: adt(&["std", "path", "Path"]),
            os_str: adt(&["std", "ffi", "OsStr"]),
            c_str: adt(&["core", "ffi", "CStr"]).or_else(|| adt(&["std", "ffi", "CStr"])),
            shared: [
                adt(&["alloc", "sync", "Arc"]).or_else(|| adt(&["std", "sync", "Arc"])),
                adt(&["alloc", "rc", "Rc"]).or_else(|| adt(&["std", "rc", "Rc"])),
            ],
            copying: [
                cx.std_trait(&["core", "clone", "Clone"]),
                cx.std_trait(&["alloc", "borrow", "ToOwned"])
                    .or_else(|| cx.std_trait(&["std", "borrow", "ToOwned"])),
                cx.std_trait(&["alloc", "string", "ToString"])
                    .or_else(|| cx.std_trait(&["std", "string", "ToString"])),
            ],
            params: RefCell::new(HashMap::new_in(Global)),
            held: RefCell::new(HashMap::new_in(Global)),
        }
    }

    /// Whether a call of `f` copies what its receiver holds into a new allocation: `clone`,
    /// `to_owned` and `to_string` of std's `Clone`, `ToOwned` and `ToString` (the trait's or an
    /// impl's), and `<[T]>::to_vec`. Its value is made there, though the receiver holds its
    /// type.
    fn copies(&self, db: &Db, f: hir::Function) -> bool {
        let name = f.name(db);
        let name = name.as_str();
        if !matches!(name, "clone" | "to_owned" | "to_string" | "to_vec") {
            return false;
        }
        if name == "to_vec" {
            let krate = f.module(db).krate(db);
            return super::facts::crate_is(db, krate, "alloc")
                || super::facts::crate_is(db, krate, "std");
        }
        hir::AsAssocItem::as_assoc_item(f, db)
            .and_then(|it| it.container_or_implemented_trait(db))
            .is_some_and(|it| self.copying.contains(&Some(it)))
    }

    /// Whether `ty` is an `Arc` or an `Rc`, whose clone shares what it points to.
    fn is_shared(&self, ty: Ty<'_>) -> bool {
        ty.as_adt().is_some_and(|(adt, _)| self.shared.contains(&Some(adt)))
    }

    /// What a `Cow<B>` owns when `B` is unsized: `String` for `str`, `Vec` for a slice,
    /// `PathBuf`, `OsString` or `CString` for `Path`, `OsStr` or `CStr`; none for a sized `B`,
    /// whose `Owned` is `B` itself and is judged as `B` is.
    fn cow_owned(&self, ty: Ty<'_>) -> Option<&'static str> {
        let (adt, args) = ty.as_adt()?;
        if Some(adt) != self.cow {
            return None;
        }
        let borrowed = args.types().next()?;
        match borrowed.kind() {
            TyKind::Str => Some("String"),
            TyKind::Slice(_) => Some("Vec"),
            _ => {
                let (it, _) = borrowed.as_adt()?;
                if Some(it) == self.path {
                    Some("PathBuf")
                } else if Some(it) == self.os_str {
                    Some("OsString")
                } else if Some(it) == self.c_str {
                    Some("CString")
                } else {
                    None
                }
            }
        }
    }

    /// The field of `adt`, a dependency's type with no allocator parameter, that holds a
    /// global allocation, and the type found there, by its declared field types walked as
    /// owned values, transitively through the dependencies' types they hold. A type of the
    /// workspace is not walked: its own fields are written types, found where they are
    /// written. papaya's shared tables are not walked either: a table shared between threads
    /// lives as long as what shares it, the global heap's lifetime (the module's doc).
    fn holds(&self, db: &Db, adt: AdtId) -> Option<(hir::Name, AdtId)> {
        let known = self.held.borrow().get(&adt).cloned(); // clone: an interned name
        if let Some(known) = known {
            return known;
        }
        let it = hir::Adt::from(adt);
        let krate = it.module(db).krate(db);
        if krate.origin(db).is_local() || super::facts::crate_is(db, krate, "papaya") {
            self.held.borrow_mut().insert(adt, None);
            return None;
        }
        // Being walked: a field that holds `adt` again reads as holding nothing.
        self.held.borrow_mut().insert(adt, None);
        // alloc: one ADT's fields, walked once and dropped
        let fields: Vec<hir::Field> = match it {
            hir::Adt::Struct(it) => it.fields(db),
            hir::Adt::Union(it) => it.fields(db),
            hir::Adt::Enum(it) => {
                it.variants(db).into_iter().flat_map(|variant| variant.fields(db)).collect()
            }
        };
        let found = fields.into_iter().find_map(|field| {
            let ty = field.ty(db).to_next_solver();
            let (_, inner) = collect(db, self, ty, Walk::Owned).into_iter().next()?;
            Some((field.name(db), inner.adt()))
        });
        self.held.borrow_mut().insert(adt, found.clone()); // clone: an interned name
        found
    }

    /// `core::alloc::Allocator`, where the crate's graph has it.
    pub(super) fn allocator_trait(&self) -> Option<hir::Trait> {
        self.allocator
    }

    fn heap(&self, adt: AdtId) -> Option<Heap> {
        let is = |it: Option<AdtId>| it == Some(adt);
        if is(self.string) {
            Some(Heap::String)
        } else if is(self.path_buf) {
            Some(Heap::PathBuf)
        } else if is(self.os_string) {
            Some(Heap::OsString)
        } else if is(self.map) || is(self.set) {
            Some(Heap::Map)
        } else {
            None
        }
    }

    /// The allocator parameter of `adt`, if it has one: a type parameter bounded by std's
    /// `Allocator` or defaulting to `Global`, as its place among the type parameters.
    pub(super) fn allocator_param(&self, db: &Db, adt: AdtId) -> Option<(usize, hir::Name)> {
        if let Some(known) = self.params.borrow().get(&adt) {
            return known.clone(); // clone: an interned name
        }
        let params = hir::GenericDef::from(hir::Adt::from(adt)).type_or_const_params(db);
        let found = params.into_iter().filter_map(|it| it.as_type_param(db)).enumerate().find_map(
            |(at, param)| {
                let to_global = param
                    .default(db)
                    .and_then(|it| it.as_adt())
                    .is_some_and(|it| Some(AdtId::from(it)) == self.global);
                let bounded = self
                    .allocator
                    .is_some_and(|allocator| param.trait_bounds(db).contains(&allocator));
                (to_global || bounded).then(|| (at, param.name(db)))
            },
        );
        self.params.borrow_mut().insert(adt, found.clone()); // clone: an interned name
        found
    }

    /// Why `ty` itself (not what it holds) is implicit, if it is. An allocator parameter is
    /// looked for first, so a type of std's that gains one is judged by its argument.
    fn implicit(&self, db: &Db, ty: Ty<'_>) -> Option<Implicit> {
        let (adt, args) = ty.as_adt()?;
        if let Some(owned) = self.cow_owned(ty) {
            return Some(Implicit::Cow { adt, owned });
        }
        let Some((at, param)) = self.allocator_param(db, adt) else {
            if let Some(heap) = self.heap(adt) {
                return Some(Implicit::Heap { adt, heap });
            }
            let (field, inner) = self.holds(db, adt)?;
            return Some(Implicit::Holds { adt, field, inner });
        };
        let global = self.global?;
        let arg = args.types().nth(at)?;
        arg.as_adt().is_some_and(|(it, _)| it == global).then_some(Implicit::Default { adt, param })
    }
}

/// How far a type is looked into.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// Everywhere, behind references, pointers, functions and `dyn` too, and an
    /// `impl Trait`'s bounds: what a written type names.
    Written,
    /// As [`Walk::Written`], for a finding at a written type: an ADT whose allocator parameter
    /// is `Global` is not one there. `Vec<T>` is `Vec<T, Global>`, one type, so a signature
    /// names no allocation; the allocation is found where it is made, at the constructor, which
    /// names `Global` (`Vec::new_in(Global)`). A type that takes no allocator (`String`, a
    /// dependency's type holding one) has no explicit form, and is found where written.
    Declared,
    /// What the value owns: not behind a reference, pointer, function, closure or `dyn`, nor
    /// in an unnormalized projection or opaque type.
    Owned,
}

struct Collect<'a, 'db> {
    db: &'db Db,
    allocators: &'a Allocators,
    walk: Walk,
    /// The types whose `impl Trait` bounds were walked, each walked once: a bound names the
    /// type it bounds (`impl Iterator`'s `Self: Iterator`), so without this a walk of the
    /// bounds walks them again, forever, as rustc's opaque-type walks keep a set of the
    /// opaque types they have seen.
    expanded: Vec<Expanded<'db>>,
    found: Vec<(Ty<'db>, Implicit)>,
}

/// What a walk of `impl Trait` bounds is keyed by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expanded<'db> {
    /// An opaque type (`-> impl Trait`, an RPITIT), by its definition, not its arguments: its
    /// bounds can name it again with other arguments (a recursive `impl Trait` through an
    /// associated type), and its arguments are walked as the type's own parts anyway.
    Opaque(OpaqueTyIdWrapper<'db>),
    /// An argument-position `impl Trait` parameter, or a coroutine: by the type itself.
    Ty(Ty<'db>),
}

impl<'db> Expanded<'db> {
    fn of(ty: Ty<'db>) -> Expanded<'db> {
        match ty.kind() {
            TyKind::Alias(_, alias) => match alias.kind {
                AliasTyKind::Opaque { def_id } => Expanded::Opaque(def_id),
                _ => Expanded::Ty(ty),
            },
            _ => Expanded::Ty(ty),
        }
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for Collect<'_, 'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        // A walk goes as deep as the type is nested: on a fiber's stack, grow it rather than
        // run into its guard page, which takes the whole daemon down, not one job.
        ensure_sufficient_stack(|| self.visit_ty_here(ty));
    }
}

impl<'db> Collect<'_, 'db> {
    fn visit_ty_here(&mut self, ty: Ty<'db>) {
        if let Some(it) = self.allocators.implicit(self.db, ty)
            && !(self.walk == Walk::Declared && matches!(it, Implicit::Default { .. }))
        {
            self.found.push((ty, it));
        }
        let behind = matches!(
            ty.kind(),
            TyKind::Ref(..)
                | TyKind::RawPtr(..)
                | TyKind::FnDef(..)
                | TyKind::FnPtr(..)
                | TyKind::Closure(..)
                | TyKind::CoroutineClosure(..)
                | TyKind::Coroutine(..)
                | TyKind::CoroutineWitness(..)
                | TyKind::Dynamic(..)
                | TyKind::Alias(..)
        );
        match self.walk {
            Walk::Owned if behind => {}
            Walk::Owned => ty.super_visit_with(self),
            Walk::Written | Walk::Declared => {
                let key = Expanded::of(ty);
                if !self.expanded.contains(&key)
                    && let Some(bounds) = ty.impl_trait_bounds(self.db)
                {
                    self.expanded.push(key);
                    bounds.visit_with(self);
                }
                ty.super_visit_with(self);
            }
        }
    }
}

/// The implicit types in `ty`, outermost first.
fn collect<'db>(
    db: &'db Db,
    allocators: &Allocators,
    ty: Ty<'db>,
    walk: Walk,
) -> Vec<(Ty<'db>, Implicit)> {
    // alloc: a lint's walk of one type, dropped when it returns
    let mut it = Collect { db, allocators, walk, expanded: Vec::new(), found: Vec::new() };
    ty.visit_with(&mut it);
    it.found
}

/// Whether `ty` is or holds `part`, anywhere in it.
fn holds<'db>(ty: Ty<'db>, part: Ty<'db>) -> bool {
    struct Holds<'x>(Ty<'x>);
    impl<'x> TypeVisitor<DbInterner<'x>> for Holds<'x> {
        type Result = ControlFlow<()>;

        fn visit_ty(&mut self, ty: Ty<'x>) -> ControlFlow<()> {
            if ty == self.0 {
                return ControlFlow::Break(());
            }
            ty.super_visit_with(self)
        }
    }
    ty.visit_with(&mut Holds(part)).is_break()
}

/// Whether `ty` is or holds the ADT `adt` (`Global`, `&Global`), anywhere in it.
fn holds_adt(ty: Ty<'_>, adt: AdtId) -> bool {
    struct HoldsAdt(AdtId);
    impl<'x> TypeVisitor<DbInterner<'x>> for HoldsAdt {
        type Result = ControlFlow<()>;

        fn visit_ty(&mut self, ty: Ty<'x>) -> ControlFlow<()> {
            if ty.as_adt().is_some_and(|(it, _)| it == self.0) {
                return ControlFlow::Break(());
            }
            ty.super_visit_with(self)
        }
    }
    ty.visit_with(&mut HoldsAdt(adt)).is_break()
}

/// The finding of the written type `ty`, reported where it is written.
pub(super) fn check_type<'t>(cx: &mut Cx<'_, 't>, ty: ast::Type<'t>) {
    let node = ty.syntax();
    if cx.expansion().is_some()
        || !cx.is_written(node)
        || is_trait_path(node)
        || cx.source_of(node) == "Self"
    {
        return;
    }
    let Some(at) = place(node) else { return };
    let Some(range) = cx.range(node) else { return };
    if exempt::exempt(cx.text(), range, Lint::ImplicitAllocator) {
        return;
    }
    let Some(resolved) = cx.resolve_type(ty) else { return };
    let db = cx.db();
    let allocators = Allocators::new(cx);
    let found = collect(db, &allocators, resolved.to_next_solver(), Walk::Declared);
    let Some((_, what)) = found.first() else { return };
    let item = describe(cx, at);
    let message = message(db, what, &item, "is", cx.snippet(range));
    let note = note(what);
    let span = cx.span(range);
    let diagnostic = cx.blaze(Lint::ImplicitAllocator, node, span, message);
    diagnostic.note(note);
    diagnostic
        .help(Lint::ImplicitAllocator.help())
        .help(format!("for further information visit {}", Lint::ImplicitAllocator.docs_url()));
}

/// The findings of the body walked: its calls and its `let`s, each once, the outermost kept.
pub(super) fn check_body<'db, 't>(
    cx: &Cx<'db, 't>,
    k: &Known<'db>,
    analysis: &hir::BodyAnalysis<'db>,
    push: &mut dyn FnMut(Lint, TextRange, String, Option<String>),
    fixes: &mut Vec<(TextRange, Vec<(TextRange, String)>)>,
) {
    let db = k.db;
    let allocators = &k.implicit;
    let body = analysis.body();
    let infer = analysis.infer();
    let owner = owner_path(cx, cx.root());
    // alloc: one body's findings, before they are reported
    let mut calls: Vec<(TextRange, Ty<'db>, Implicit, String, String)> = Vec::new();
    for (id, expr) in body.exprs() {
        let (receiver, args) = match expr {
            Expr::MethodCall { receiver, args, .. } => (Some(receiver), args),
            Expr::Call { args, .. } => (None, args),
            _ => continue,
        };
        let Some((callee, callee_args)) = super::alloc::written_callee(infer, body, id) else {
            continue;
        };
        let function = hir::Function::from(callee);
        if callee.krate(db).data(db).origin.is_local()
            && !collect(db, allocators, function.ret_type(db).to_next_solver(), Walk::Written)
                .is_empty()
        {
            continue;
        }
        let value = infer.expr_ty(id);
        // A copying method (`clone`, `to_owned`, `to_string`, `to_vec`) makes its value here
        // though its receiver holds the type; a clone of an `Arc` or `Rc` shares and makes none.
        let copies = allocators.copies(db, function) && !allocators.is_shared(value);
        let mut made = collect(db, allocators, value, Walk::Owned);
        for arg in callee_args.types() {
            made.extend(collect(db, allocators, arg, Walk::Owned));
        }
        let inputs = receiver.into_iter().chain(args.iter().copied()).map(|it| infer.expr_ty(it));
        // `Vec::new_in(Global)`, `Box::new_in(x, Global)`: the call was handed `Global`, so the
        // global heap is named where it is used.
        if let Some(global) = allocators.global
            && args.iter().any(|&arg| holds_adt(infer.expr_ty(arg), global))
        {
            continue;
        }
        let Some((ty, what)) = made
            .into_iter()
            .find(|(ty, _)| copies || !inputs.clone().any(|input| holds(input, *ty)))
        else {
            continue;
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        let fixed = match explicit_fix(cx, analysis, allocators, callee, &what, range) {
            Some(fix) => {
                fixes.push((range, fix));
                true
            }
            None => false,
        };
        let called = super::macro_name(cx.snippet(range));
        let item = match called {
            Some(called) => format!("this `{called}!` in `{owner}`"),
            None => format!("this call to `{}` in `{owner}`", function.name(db).as_str()),
        };
        let shown = ty.display(db, k.target).to_string();
        let message = message(db, &what, &item, "makes", &shown);
        let lead = no_allocation(called, function.name(db).as_str(), cx.snippet(range));
        let note = site_note(lead, &what, fixed);
        calls.push((range, ty, what, message, note));
    }

    // alloc: one body's findings, before they are reported
    let mut found: Vec<(TextRange, String, String)> = Vec::new();
    let root = cx.root();
    let mut events = root.preorder();
    while let Some(event) = events.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if node != root && matches!(node.kind(), K::FN | K::CONST | K::STATIC) {
            events.skip_subtree();
            continue;
        }
        let Some(stmt) = ast::LetStmt::cast(node) else { continue };
        let (Some(pat), Some(range)) = (stmt.pat(), cx.range(node)) else { continue };
        let item = format!("the local `{}` of `{owner}`", cx.source_of(pat.syntax()));
        if let Some(ty) = stmt.ty() {
            let Some(resolved) = cx.resolve_type(ty) else { continue };
            let written = collect(db, allocators, resolved.to_next_solver(), Walk::Declared);
            let Some((_, what)) = written.first() else { continue };
            let shown = match cx.range(ty.syntax()) {
                Some(at) => cx.snippet(at).to_owned(),
                None => cx.source_of(ty.syntax()).into_owned(),
            };
            found.push((range, message(db, what, &item, "is", &shown), note(what)));
            continue;
        }
        let Some(init) = stmt.initializer().and_then(|it| cx.range(it.syntax())) else { continue };
        let Some(local) = cx.pat_ty(pat).map(|it| it.to_next_solver()) else { continue };
        let made = calls.iter().find(|(at, ty, ..)| init.contains_range(*at) && holds(local, *ty));
        let Some((_, _, what, _, note)) = made else { continue };
        let shown = local.display(db, k.target).to_string();
        found.push((range, message(db, what, &item, "is", &shown), note.clone()));
    }
    found.extend(calls.into_iter().map(|(range, _, _, message, note)| (range, message, note)));

    // alloc: the ranges of one body's findings
    let ranges: Vec<TextRange> = found.iter().map(|it| it.0).collect();
    for (range, message, note) in found {
        if ranges.iter().any(|it| *it != range && it.contains_range(range)) {
            continue;
        }
        push(Lint::ImplicitAllocator, range, message, Some(note));
    }
}

/// The machine-applicable rewrite of the call at `range` to the explicit form that names
/// `Global`, where one with the same meaning exists: `T::new()` to `T::new_in(Global)` and
/// `T::with_capacity(n)` to `T::with_capacity_in(n, Global)` for an ADT whose allocator
/// parameter defaults to `Global` and which has the `_in` constructor in an inherent impl
/// (`Vec`, `VecDeque`, `Box::new(x)` to `Box::new_in(x, Global)`, blaze_hash's maps,
/// blaze-mem's `String`, `PathBuf` and `OsString`), and an empty `vec![]` to
/// `Vec::new_in(Global)`. `Vec<T>` is `Vec<T, Global>`, so no signature changes with it.
///
/// `Global` is written as it resolves at the call: by its name when that name is std's
/// `Global` there, by its path from the module when the name is something else, and by its
/// name with a `use` of that path added before the first statement of the function the call
/// is in when the name resolves to nothing. Two fixes in one function that each add the `use` overlap, so
/// `--fix` makes one a pass and the next pass finds the name in scope.
///
/// None (the finding stands, with no fix) for a crate whose root does not turn on
/// `allocator_api` (the `_in` constructors are unstable and the attribute is in another file),
/// a call a macro wrote, a `vec![..]` with elements (std has no `vec!` that takes an
/// allocator), `String::new()`, `format!`, `to_string`, `to_owned`, `collect()` and the other
/// calls of a type with no allocator parameter, whose explicit form is another type.
fn explicit_fix<'t>(
    cx: &Cx<'_, 't>,
    analysis: &hir::BodyAnalysis<'_>,
    allocators: &Allocators,
    callee: hir_def::FunctionId,
    what: &Implicit,
    range: TextRange,
) -> Option<Vec<(TextRange, String)>> {
    let function = hir::Function::from(callee);
    let Implicit::Default { adt, .. } = *what else { return None };
    let db = cx.db();
    let global = allocators.global?;
    if !cx.features().is_enabled(&intern::Symbol::intern("allocator_api")) {
        return None;
    }
    let snippet = cx.snippet(range);
    // The call as written, with `{G}` where the allocator goes.
    let call = match super::macro_name(snippet) {
        Some("vec") => {
            let inside = snippet.strip_prefix("vec!")?.trim();
            if !matches!(inside, "[]" | "()" | "{}") || adt_name(db, adt) != "Vec" {
                return None;
            }
            "Vec::new_in({G})".to_owned()
        }
        Some(_) => return None,
        None => {
            if super::facts::impl_adt(db, callee) != Some(hir::Adt::from(adt)) {
                return None;
            }
            let name = function.name(db);
            let name = name.as_str();
            if !matches!(name, "new" | "with_capacity") {
                return None;
            }
            let open = snippet.find('(')?;
            let (path, rest) = snippet.split_at(open);
            if !path.ends_with(&format!("::{name}")) || !rest.ends_with(')') {
                return None;
            }
            let explicit = format!("{name}_in");
            if !has_inherent(db, adt, &explicit) {
                return None;
            }
            let args = rest[1..rest.len() - 1].trim();
            let args = match args.is_empty() {
                true => "{G}".to_owned(),
                false => format!("{args}, {{G}}"),
            };
            format!("{}{explicit}({args})", &path[..path.len() - name.len()])
        }
    };
    let root = cx.root();
    if !root.text_range().contains_range(range) {
        return None;
    }
    let at = match root.covering_element(range) {
        syntax::NodeOrToken::Node(node) => node,
        syntax::NodeOrToken::Token(token) => token.parent()?,
    };
    let module = analysis.def().module(db);
    let config = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let item = hir::ItemInNs::Types(hir::ModuleDef::Adt(hir::Adt::from(global)));
    let path = || Some(module.find_path(db, item, config)?.display(db, cx.edition()).to_string());
    let mut parts = Vec::new();
    let named = match cx.resolve_name(at, "Global") {
        Some(hir::PathResolution::Def(hir::ModuleDef::Adt(it))) if AdtId::from(it) == global => {
            "Global".to_owned()
        }
        Some(_) => path()?,
        None => {
            parts.push(import(cx, &path()?)?);
            "Global".to_owned()
        }
    };
    parts.push((range, call.replace("{G}", &named)));
    Some(parts)
}

/// The form of the call that allocates nothing, where one exists for it: the first choice of
/// a finding's note (priority one is no allocation; naming the allocator is the second).
/// `called` is the macro the call is, if it is one; `name` the function called.
fn no_allocation(called: Option<&str>, name: &str, snippet: &str) -> Option<&'static str> {
    match (called, name) {
        (Some("format"), _) | (None, "to_string") => Some(
            "write into the caller's buffer with `blaze_mem::format_into!(&mut buf, ..)` or `write!`, or hand \
             on the `fmt::Arguments`/`Display` value itself",
        ),
        (Some("vec"), _) => {
            let empty = snippet.strip_prefix("vec!").map(str::trim);
            match empty {
                Some("[]" | "()" | "{}") => None,
                _ => Some("use an array (`[a, b]`) or the caller's slice where the length is fixed"),
            }
        }
        (None, "to_owned" | "clone" | "to_vec") => {
            Some("borrow it (`&str`, `&[T]`, `&T`) where the copy is only read")
        }
        (None, "collect") => {
            Some("pass the iterator on to what consumes it instead of collecting it")
        }
        _ => None,
    }
}

/// A call finding's note: the form that does not allocate first, where there is one; then the
/// allocator to name; then, where the finding has a fix, that the fix keeps the meaning and
/// the better change is by hand.
fn site_note(lead: Option<&str>, what: &Implicit, fixed: bool) -> String {
    let named = note(what);
    let mut out = match lead {
        Some(lead) => format!("first, do not allocate: {lead}; where an allocation is needed, {named}"),
        None => named,
    };
    if fixed {
        out.push_str(
            "; the fix writes the explicit `Global` form, which keeps the meaning: not allocating, or \
             allocating in the task's frame, is the better change and is made by hand",
        );
    }
    out
}

/// Whether `adt` has an inherent associated function named `name`, in any impl of its crate's.
fn has_inherent(db: &Db, adt: AdtId, name: &str) -> bool {
    let it = hir::Adt::from(adt);
    hir::Impl::all_for_type(db, it.module(db).krate(db), it.ty(db))
        .into_iter()
        .filter(|it| it.trait_(db).is_none())
        .flat_map(|it| it.items(db))
        .any(|item| match item {
            hir::AssocItem::Function(f) => f.name(db).as_str() == name,
            _ => false,
        })
}

/// The insertion of `use <path>;` before the first statement of the body's function, at that
/// statement's indentation: a `use` in a function's block is in scope in all of it, nested
/// blocks and closures too. The body's query reads the function's own tree, whose ranges begin
/// at it, so the module's item list around the function is not a place a fix of the body can
/// write. `None` for a body that is not a function's block (a `const`'s or `static`'s) and for
/// a block whose first statement shares its line with the `{`.
fn import<'t>(cx: &Cx<'_, 't>, path: &str) -> Option<(TextRange, String)> {
    let list = ast::Fn::cast(cx.root())?.body()?.stmt_list()?;
    let first = match list.statements().next() {
        Some(stmt) => stmt.syntax(),
        None => list.tail_expr()?.syntax(),
    };
    let start = cx.range(first)?.start();
    let text = cx.text();
    let line = text[..usize::from(start)].rfind('\n').map_or(0, |it| it + 1);
    let indent = &text[line..usize::from(start)];
    if !indent.chars().all(|it| it == ' ' || it == '\t') {
        return None;
    }
    Some((TextRange::empty(start), format!("use {path};\n{indent}")))
}

/// Whether `node` is the path of a trait (in a bound, or an impl's trait), not a type.
fn is_trait_path(node: ast::Node<'_>) -> bool {
    if node.kind() != K::PATH_TYPE {
        return false;
    }
    let Some(parent) = node.parent() else { return false };
    match parent.kind() {
        K::TYPE_BOUND => true,
        K::IMPL => {
            ast::Impl::cast(parent).and_then(|it| it.trait_()).is_some_and(|it| it.syntax() == node)
        }
        _ => false,
    }
}

/// Where a written type is: the node it belongs to, and the parameter or return type it is in.
#[derive(Clone, Copy)]
struct Place<'t> {
    holder: ast::Node<'t>,
    via: Option<ast::Node<'t>>,
}

/// Where the written type `node` is, climbing past what a type is built of; `None` when it is
/// part of a larger type, a `let`'s type, or a turbofish, which the type around it, the `let`
/// and the call find.
fn place(node: ast::Node<'_>) -> Option<Place<'_>> {
    let mut in_args = false;
    let mut via = None;
    let mut at = node.parent();
    while let Some(parent) = at {
        let kind = parent.kind();
        if ast::Type::can_cast(kind) && !is_trait_path(parent) {
            return None;
        }
        match kind {
            K::GENERIC_ARG_LIST => in_args = true,
            K::PARAM | K::RET_TYPE => via = Some(parent),
            K::TYPE_ARG
            | K::ASSOC_TYPE_ARG
            | K::PATH_SEGMENT
            | K::PATH
            | K::PATH_TYPE
            | K::TYPE_BOUND
            | K::TYPE_BOUND_LIST
            | K::PARENTHESIZED_ARG_LIST
            | K::PARAM_LIST => {}
            K::LET_STMT => return None,
            _ if ast::Expr::can_cast(kind) => {
                return (!in_args).then_some(Place { holder: parent, via });
            }
            _ => return Some(Place { holder: parent, via }),
        }
        at = parent.parent();
    }
    None
}

/// The item a written type at `place` is of, by its path.
fn describe<'t>(cx: &Cx<'_, 't>, place: Place<'t>) -> String {
    let Place { holder, via } = place;
    let owner = owner_path(cx, holder);
    let returned = via.is_some_and(|it| it.kind() == K::RET_TYPE);
    match holder.kind() {
        K::FN if returned => format!("the return type of `{owner}`"),
        K::FN => match via.and_then(ast::Param::cast).and_then(|it| it.pat()) {
            Some(pat) => format!("the parameter `{}` of `{owner}`", cx.source_of(pat.syntax())),
            None => format!("a parameter of `{owner}`"),
        },
        K::CLOSURE_EXPR if returned => format!("a closure's return type in `{owner}`"),
        K::CLOSURE_EXPR => format!("a closure's parameter in `{owner}`"),
        K::RECORD_FIELD => format!("the field `{owner}`"),
        K::TUPLE_FIELD => {
            let index = holder.parent().map_or(0, |list| {
                list.children()
                    .take_while(|it| *it != holder)
                    .filter(|it| it.kind() == K::TUPLE_FIELD)
                    .count()
            });
            format!("the field `{owner}::{index}`")
        }
        K::TYPE_ALIAS => format!("the type alias `{owner}`"),
        K::CONST => format!("the const `{owner}`"),
        K::STATIC => format!("the static `{owner}`"),
        K::IMPL => format!("the impl header of `{owner}`"),
        K::TYPE_PARAM => format!("a type parameter of `{owner}`"),
        K::WHERE_PRED => format!("a `where` bound of `{owner}`"),
        _ => format!("a type written in `{owner}`"),
    }
}

/// The path of the items around `node`, itself included: `m::Index::names`, `Vec<u8>::push`
/// for a method of an impl.
fn owner_path<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> String {
    // alloc: a message's part
    let mut segments: Vec<String> = Vec::new();
    let mut at = Some(node);
    while let Some(node) = at {
        match node.kind() {
            K::FN
            | K::STRUCT
            | K::ENUM
            | K::UNION
            | K::TRAIT
            | K::MODULE
            | K::CONST
            | K::STATIC
            | K::TYPE_ALIAS
            | K::VARIANT
            | K::RECORD_FIELD => {
                if let Some(name) = ast::AnyHasName::cast(node).and_then(|it| it.name()) {
                    segments.push(name.text().to_owned());
                }
            }
            K::IMPL => {
                if let Some(ty) = ast::Impl::cast(node).and_then(|it| it.self_ty()) {
                    segments.push(cx.source_of(ty.syntax()).into_owned());
                }
            }
            _ => {}
        }
        at = node.parent();
    }
    if segments.is_empty() {
        return "the crate".to_owned();
    }
    segments.reverse();
    segments.join("::")
}

fn adt_name(db: &Db, adt: AdtId) -> String {
    hir::Adt::from(adt).name(db).as_str().to_owned()
}

/// The message of a finding: `item` (`the field `Index::names``) `verb`s `shown`, the type as
/// written or inferred, and why it is implicit.
fn message(db: &Db, what: &Implicit, item: &str, verb: &str, shown: &str) -> String {
    match what {
        Implicit::Default { adt, param } => format!(
            "{item} {verb} `{shown}`, whose `{}` leaves its allocator `{}` to the default `Global`; {RULE}",
            adt_name(db, *adt),
            param.as_str(),
        ),
        Implicit::Heap { adt, .. } => format!(
            "{item} {verb} `{shown}`, and `{}` takes no allocator, so it always allocates from the global heap; {RULE}",
            adt_name(db, *adt),
        ),
        Implicit::Holds { adt, field, inner } => format!(
            "{item} {verb} `{shown}`, and `{}` takes no allocator but holds a `{}` (its field `{}`), so it allocates from the global heap; {RULE}",
            adt_name(db, *adt),
            adt_name(db, *inner),
            field.as_str(),
        ),
        Implicit::Cow { owned, .. } => format!(
            "{item} {verb} `{shown}`, whose `Cow::Owned` is a `{owned}` on the global heap; {RULE}"
        ),
    }
}

/// What `blaze_mem` has in its place.
fn note(what: &Implicit) -> String {
    let note = match what {
        Implicit::Default { .. } => {
            "`blaze_mem::Frame` (a task's), `blaze_mem::ArenaLocal` (a request's) and `blaze_mem::Kept(tag)` \
             (an image value that outlives requests and is freed on its own) implement `Allocator`: \
             `Vec<T, &Frame>`, `Box<T, &Frame>`, `Arc<[u8], Kept>`, and `blaze_mem::frame::collect(frame, iter)` \
             for a `collect()`; where the global heap is the value's lifetime, name it: `Vec::new_in(Global)`, \
             `Box::new_in(x, Global)`"
        }
        Implicit::Heap { heap: Heap::String, .. } => {
            "use `blaze_mem::String<A>`, a growable string in the allocator you name: \
             `blaze_mem::String<&Frame>` from `String::new_in(frame)`, `blaze_mem::frame::string(frame, text)` \
             or `blaze_mem::format_in!(frame, ..)`; finished text that won't grow goes in the frame with \
             `Frame::alloc_str` (a request's `ArenaLocal::alloc_str`)"
        }
        Implicit::Heap { heap: Heap::PathBuf, .. } => {
            "use `blaze_mem::PathBuf<A>` (unix), a path in the allocator you name: \
             `blaze_mem::PathBuf<&Frame>` from `blaze_mem::frame::path(frame, path)`, with `push`, `pop` and \
             `join` as std's; or join in the worker's reused buffer with `blaze_mem::scratch::with_path`"
        }
        Implicit::Heap { heap: Heap::OsString, .. } => {
            "use `blaze_mem::OsString<A>` (unix), an `OsStr` in the allocator you name: \
             `blaze_mem::OsString<&Frame>` from `blaze_mem::frame::os_string(frame, text)`"
        }
        Implicit::Heap { heap: Heap::Map, .. } => {
            "std's `HashMap` and `HashSet` take no allocator: `blaze_hash::HashMap<K, V, &Frame>` \
             (`HashMap::new_in(frame)`, `with_capacity_in`, `from_iter_in`) names one"
        }
        Implicit::Holds { .. } => {
            "the type takes no allocator and allocates inside: use an equivalent that takes one \
             (`blaze_mem`'s text and path types, `Vec<u8, A>` for a byte buffer), or hold it where the global \
             heap is its lifetime and say so with `#[expect(blaze::implicit_allocator, reason = \"..\")]`"
        }
        Implicit::Cow { .. } => {
            "borrow (`&str`, `&[T]`) where nothing is owned, or own in the allocator you name \
             (`blaze_mem::String<&Frame>`, `Vec<T, &Frame>`)"
        }
    };
    note.to_owned()
}
