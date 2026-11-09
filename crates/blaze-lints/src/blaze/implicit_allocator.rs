//! `implicit_allocator`: a value whose allocator is left to its default, `Global` (AGENTS.md
//! rule 11, bump-allocate: every value names its allocator).
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
const RULE: &str = "every value names its allocator: `Vec::new_in(frame)` from \
                    `blaze_mem::frame::current`, or the request/revision/image allocator";

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
    /// Each ADT's allocator parameter: its place among the ADT's type parameters and its name.
    params: RefCell<HashMap<AdtId, Option<(usize, hir::Name)>, Global>>,
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
            params: RefCell::new(HashMap::new_in(Global)),
        }
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
    fn allocator_param(&self, db: &Db, adt: AdtId) -> Option<(usize, hir::Name)> {
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
        let Some((at, param)) = self.allocator_param(db, adt) else {
            return self.heap(adt).map(|heap| Implicit::Heap { adt, heap });
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
        if let Some(it) = self.allocators.implicit(self.db, ty) {
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
            Walk::Written => {
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
    let found = collect(db, &allocators, resolved.to_next_solver(), Walk::Written);
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
) {
    let db = k.db;
    let allocators = &k.implicit;
    let body = analysis.body();
    let infer = analysis.infer();
    let owner = owner_path(cx, cx.root());
    // alloc: one body's findings, before they are reported
    let mut calls: Vec<(TextRange, Ty<'db>, Implicit, String)> = Vec::new();
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
        let mut made = collect(db, allocators, infer.expr_ty(id), Walk::Owned);
        for arg in callee_args.types() {
            made.extend(collect(db, allocators, arg, Walk::Owned));
        }
        let inputs = receiver.into_iter().chain(args.iter().copied()).map(|it| infer.expr_ty(it));
        let Some((ty, what)) =
            made.into_iter().find(|(ty, _)| !inputs.clone().any(|input| holds(input, *ty)))
        else {
            continue;
        };
        let Some(range) = analysis.expr_range(id) else { continue };
        let item = match super::macro_name(cx.snippet(range)) {
            Some(called) => format!("this `{called}!` in `{owner}`"),
            None => format!("this call to `{}` in `{owner}`", function.name(db).as_str()),
        };
        let shown = ty.display(db, k.target).to_string();
        let message = message(db, &what, &item, "makes", &shown);
        calls.push((range, ty, what, message));
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
            let written = collect(db, allocators, resolved.to_next_solver(), Walk::Written);
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
        let Some((_, _, what, _)) = made else { continue };
        let shown = local.display(db, k.target).to_string();
        found.push((range, message(db, what, &item, "is", &shown), note(what)));
    }
    found.extend(calls.into_iter().map(|(range, _, what, message)| (range, message, note(&what))));

    // alloc: the ranges of one body's findings
    let ranges: Vec<TextRange> = found.iter().map(|it| it.0).collect();
    for (range, message, note) in found {
        if ranges.iter().any(|it| *it != range && it.contains_range(range)) {
            continue;
        }
        push(Lint::ImplicitAllocator, range, message, Some(note));
    }
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
    }
}

/// What `blaze_mem` has in its place.
fn note(what: &Implicit) -> String {
    let note = match what {
        Implicit::Default { .. } => {
            "`blaze_mem::Frame` (a task's), `blaze_mem::ArenaLocal` (a request's) and `blaze_mem::Kept(tag)` \
             (an image value that outlives requests and is freed on its own) implement `Allocator`: \
             `Vec<T, &Frame>`, `Box<T, &Frame>`, `Arc<[u8], Kept>`, and `blaze_mem::frame::collect(frame, iter)` \
             for a `collect()`"
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
    };
    note.to_owned()
}
