//! Forms: which edits reach an item's interface, body and closures, and which leave them.

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::{
    AdtId, DefWithBodyId, VariantId,
    expr_store::Body,
    hir::{ClosureKind, Expr},
};
use hir_ty::{
    InferBodyId,
    db::{HirDatabase, InternedClosure, InternedClosureId},
    mir::{BasicBlockId, MirBody, TerminatorKind},
};

use super::{TestDB, edit, item, items, single};
use crate::{
    ItemId,
    form::{Cx, FormTy, Missed, Sub, check, ty_form},
    key,
};

/// A form's digest, its slots by path and what it missed: slots of one fixture compared across
/// edits, which may renumber their ids.
type Named = (ContentId, Vec<String>, Option<Missed>);

fn named(db: &TestDB, sub: Sub) -> Named {
    (sub.digest, sub.refs.iter().map(|&it| key::path(db, it)).collect(), sub.missed)
}

fn interface(db: &TestDB, path: &str) -> Named {
    named(db, Cx::new(db).interface(item(db, path)))
}

#[track_caller]
fn body(db: &TestDB, path: &str) -> Named {
    let id = item(db, path);
    let code = Cx::new(db).body(id).unwrap_or_else(|| panic!("`{path}` has no body"));
    named(db, code.sub)
}

fn closures(db: &TestDB, f: ItemId) -> Vec<InternedClosureId<'_>> {
    let ItemId::Function(func) = f else { panic!("{f:?} is no fn") };
    let owner = DefWithBodyId::from(func);
    Body::of(db, owner)
        .exprs()
        .filter_map(|(expr, it)| match it {
            // An `async` block is a coroutine, interned as an `InternedCoroutineId`.
            Expr::Closure {
                closure_kind: closure_kind @ (ClosureKind::Closure | ClosureKind::CoroutineClosure(_)),
                ..
            } => Some(InternedClosureId::new(
                db,
                InternedClosure { owner: owner.into(), expr, kind: closure_kind },
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn formatting_and_comments_keep_interface_and_body() {
    let (mut db, file) = single(
        r#"
struct S { x: u32 }
fn f(s: &S) -> u32 { s.x + 1 }
"#,
    );
    let (interface_before, body_before) = (interface(&db, "f"), body(&db, "f"));
    let s = interface(&db, "S");
    edit(
        &mut db,
        file,
        r#"
// A struct.
struct S {
    /// Its field.
    x: u32,
}

/* A function. */
fn f(
    s: &S,
) -> u32 {
    // One more.
    s.x
        + 1
}
"#,
    );
    assert_eq!(interface(&db, "f"), interface_before);
    assert_eq!(body(&db, "f"), body_before);
    assert_eq!(interface(&db, "S"), s);
}

#[test]
fn renaming_a_fn_keeps_interface_and_body() {
    let (mut db, file) = single(
        r#"
struct S;
fn f(s: S) -> S { s }
"#,
    );
    let (interface_before, body_before) = (interface(&db, "f"), body(&db, "f"));
    edit(
        &mut db,
        file,
        r#"
struct S;
fn g(s: S) -> S { s }
"#,
    );
    assert_eq!(interface(&db, "g"), interface_before);
    assert_eq!(body(&db, "g"), body_before);
}

#[test]
fn whitespace_in_a_raw_string_is_code() {
    let (mut db, file) = single(
        r#"
fn f() -> &'static str { r"a b" }
"#,
    );
    let before = body(&db, "f");
    edit(
        &mut db,
        file,
        r#"
fn f() -> &'static str { r"a  b" }
"#,
    );
    assert_ne!(body(&db, "f").0, before.0);
}

#[test]
fn a_field_type_is_the_struct_s_interface() {
    let (mut db, file) = single("struct S { x: u32 }");
    let before = interface(&db, "S");
    edit(&mut db, file, "struct S { x: u64 }");
    assert_ne!(interface(&db, "S").0, before.0);
}

#[test]
fn where_clause_order_is_not_observed() {
    let (mut db, file) = single(
        r#"
trait A {}
trait B {}
fn f<T>() where T: A, T: B {}
"#,
    );
    let before = interface(&db, "f");
    edit(
        &mut db,
        file,
        r#"
trait A {}
trait B {}
fn f<T>() where T: B, T: A {}
"#,
    );
    assert_eq!(interface(&db, "f"), before);
}

#[test]
fn trait_method_order_is_the_trait_s_interface() {
    let (mut db, file) = single(
        r#"
trait Tr {
    fn a(&self) -> u8;
    fn b(&self) -> u16;
}
"#,
    );
    let before = interface(&db, "Tr");
    edit(
        &mut db,
        file,
        r#"
trait Tr {
    fn b(&self) -> u16;
    fn a(&self) -> u8;
}
"#,
    );
    let after = interface(&db, "Tr");
    assert_ne!(after.1, before.1, "a trait's items are slots in declaration order");
}

#[test]
fn a_capture_edit_reaches_the_owner_and_the_closure_s_captures() {
    // #906
    let (mut db, file) = single(
        r#"
//- minicore: sized, copy, clone, add, fn
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c = || a;
    c() + b
}
"#,
    );
    let f = item(&db, "f");
    let owner = body(&db, "f");
    assert_eq!(owner.2, None);
    let captures: Vec<_> =
        closures(&db, f).into_iter().map(|it| Cx::new(&db).captures(it).digest).collect();
    edit(
        &mut db,
        file,
        r#"
fn f() -> u8 {
    let a = 1u8;
    let b = 2u8;
    let c = || b;
    c() + a
}
"#,
    );
    let f = item(&db, "f");
    let after: Vec<_> =
        closures(&db, f).into_iter().map(|it| Cx::new(&db).captures(it).digest).collect();
    assert_eq!(captures.len(), 1);
    let after_owner = body(&db, "f");
    assert_eq!(after_owner.2, None);
    assert_ne!(after_owner.0, owner.0);
    assert_ne!(after, captures);
}

#[test]
fn an_impl_trait_s_auto_traits_are_the_fn_s_interface() {
    let (mut db, file) = single(
        r#"
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
pub trait Tr {}
pub struct S<T> { a: T }
impl<T> Tr for S<T> {}
pub fn f() -> impl Tr { S { a: 0u8 } }
"#,
    );
    let f = item(&db, "f");
    let (before, auto_before) = (interface(&db, "f"), Cx::new(&db).auto_traits(f, 0));
    assert_eq!(auto_before.missed, None);
    edit(
        &mut db,
        file,
        r#"
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
pub trait Tr {}
pub struct S<T> { a: T }
impl<T> Tr for S<T> {}
pub fn f() -> impl Tr { S { a: NoMarker } }
"#,
    );
    let f = item(&db, "f");
    assert_ne!(Cx::new(&db).auto_traits(f, 0).digest, auto_before.digest);
    assert_ne!(interface(&db, "f").0, before.0);
}

#[test]
fn an_async_fn_s_future_auto_traits_are_its_interface() {
    let (mut db, file) = single(
        r#"
//- minicore: future, sized
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
async fn yielding() {}
pub async fn f() {
    let kept = 0u8;
    yielding().await;
    let _moved = kept;
}
"#,
    );
    let f = item(&db, "f");
    let (before, auto_before) = (interface(&db, "f"), Cx::new(&db).auto_traits(f, 0));
    assert_eq!(auto_before.missed, None);
    edit(
        &mut db,
        file,
        r#"
#![feature(auto_traits, negative_impls)]
pub auto trait Marker {}
pub struct NoMarker;
impl !Marker for NoMarker {}
async fn yielding() {}
pub async fn f() {
    let kept = NoMarker;
    yielding().await;
    let _moved = kept;
}
"#,
    );
    let f = item(&db, "f");
    assert_ne!(Cx::new(&db).auto_traits(f, 0).digest, auto_before.digest);
    assert_ne!(interface(&db, "f").0, before.0);
}

#[test]
fn a_const_fn_s_body_is_its_interface_and_a_fn_s_is_not() {
    let (mut db, file) = single(
        r#"
const fn c() -> u8 { 1 }
fn p() -> u8 { 1 }
"#,
    );
    let (c, p, p_body) = (interface(&db, "c"), interface(&db, "p"), body(&db, "p"));
    edit(
        &mut db,
        file,
        r#"
const fn c() -> u8 { 2 }
fn p() -> u8 { 2 }
"#,
    );
    assert_ne!(interface(&db, "c").0, c.0);
    assert_eq!(interface(&db, "p"), p);
    assert_ne!(body(&db, "p").0, p_body.0);
}

#[test]
fn type_name_spells_the_type_s_name() {
    let (mut db, file) = single(
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct A;
fn f() -> &'static str { intrinsics::type_name::<A>() }
"#,
    );
    let before = body(&db, "f");
    edit(
        &mut db,
        file,
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}
struct B;
fn f() -> &'static str { intrinsics::type_name::<B>() }
"#,
    );
    assert_ne!(body(&db, "f").0, before.0);
}

#[test]
fn only_the_name_and_id_intrinsics_observe_names() {
    let (db, _) = single(
        r#"
#![feature(intrinsics)]
mod intrinsics {
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
    #[rustc_intrinsic]
    pub const fn type_id<T: ?Sized + 'static>() -> u128;
    #[rustc_intrinsic]
    pub const fn size_of<T>() -> usize;
    #[rustc_intrinsic]
    pub fn sqrtf32(x: f32) -> f32;
}
mod plain {
    pub fn type_name() -> &'static str { "" }
}
"#,
    );
    let observes = |path: &str| {
        let ItemId::Function(func) = item(&db, path) else { panic!("`{path}` is no fn") };
        crate::observes_names(&db, func)
    };
    assert!(observes("intrinsics::type_name"));
    assert!(observes("intrinsics::type_id"));
    assert!(!observes("intrinsics::size_of"));
    assert!(!observes("plain::type_name"));
    // Not an intrinsic of the pinned nightly: refused where it is called, keyed on names here.
    assert!(observes("intrinsics::sqrtf32"));
}

#[test]
fn a_no_mangle_fn_s_name_is_its_interface() {
    let (mut db, file) = single(
        r#"
#[no_mangle]
pub extern "C" fn f() {}
"#,
    );
    let before = interface(&db, "f");
    edit(
        &mut db,
        file,
        r#"
#[no_mangle]
pub extern "C" fn g() {}
"#,
    );
    assert_ne!(interface(&db, "g").0, before.0);
}

/// Fixtures that between them write every kind of item, generic parameters of each kind, where
/// clauses and `dyn` bounds two or more at a time, matches, loops, conditional drops, closures
/// in closures, `async` bodies, `impl Trait` and both kinds of declarative macro.
const FIXTURES: &[&str] = &[
    r#"
//- minicore: sized, copy, clone, add, ord, option, drop, fn, send, index, slice
pub trait A {}
pub trait B {}
pub trait C<T> {
    type Out;
    fn c(&self, t: T) -> Self::Out;
}
pub struct S<T> { pub a: T, b: Option<T> }
pub enum E { X(u8), Y { y: u16 }, Z }
pub union U { a: u32, b: f32 }
impl A for u8 {}
impl B for u8 {}
impl<T: Copy> C<T> for S<T> where T: A, T: B {
    type Out = T;
    fn c(&self, t: T) -> T { t }
}
pub type Alias<T> = S<Option<T>>;
pub fn g<T>(x: T) -> T where T: A + B + Copy, Option<T>: Copy { x }
pub fn d(x: &dyn C<u8, Out = u8>, _y: &(dyn A + Send)) -> u8 { x.c(1) }
pub fn h<'a, T: 'a, const N: usize>(x: &'a [T; N]) -> &'a T { &x[0] }
pub const K: u8 = 3;
pub static ST: [u16; 2] = [4, 5];
"#,
    r#"
//- minicore: sized, copy, clone, add, ord, option, drop, fn
pub struct D(pub u8);
impl Drop for D { fn drop(&mut self) {} }
pub enum E { X(u8), Y { y: u16 }, Z }
pub fn m(e: E) -> u16 {
    match e {
        E::X(a) => a as u16,
        E::Y { y } => y,
        E::Z => 0,
    }
}
pub fn l(n: u32) -> u32 {
    let mut s = 0;
    let mut i = 0;
    while i < n {
        s += i;
        i += 1;
    }
    loop {
        if s > 10 {
            break;
        }
        s += 1;
    }
    s
}
pub fn cl(a: u8) -> u8 {
    let b = 2u8;
    let f = |x: u8| x + a;
    let g = move || b;
    let h = || {
        let k = || f(1);
        k()
    };
    f(g()) + h()
}
fn helper(d: &D) -> u8 { d.0 }
pub fn dr(c: bool) -> u8 {
    let d = D(1);
    let e = D(2);
    if c {
        let _moved = e;
    }
    helper(&d)
}
pub const fn kf() -> u32 { 5 }
pub const K: u32 = kf() + 1;
pub fn arr() -> [u32; 3] { [1, 2, K] }
pub fn s() -> &'static str { "hi" }
pub fn opt(o: Option<u8>) -> u8 { if let Some(x) = o { x } else { 0 } }
pub fn tup() -> (u8, u16) {
    let (a, b) = (1, 2);
    (b, a)
}
"#,
    r#"
//- minicore: sized, copy, clone, add, future
#![feature(decl_macro)]
macro_rules! m {
    ($e:expr) => { $e + 1 };
    () => { 0 };
}
pub macro n($e:expr) { $e }
async fn y() -> u8 { 1 }
pub async fn a() -> u8 {
    let k = 2u8;
    y().await + k
}
pub fn uses_m() -> u8 { m!(1u8) + m!() + n!(2) }
pub trait T {
    fn t(&self) -> u8 { 1 }
    fn u(&self) -> u8;
}
pub struct W;
impl T for W {
    fn u(&self) -> u8 { 2 }
}
pub fn imp() -> impl T { W }
"#,
];

/// Every MIR body of the fixture: of each item with one, then of the closures in them,
/// transitively.
fn mir_bodies(db: &TestDB) -> Vec<&MirBody<'_>> {
    let mut bodies = Vec::new();
    for id in items(db) {
        let def = match id {
            ItemId::Function(it) => DefWithBodyId::from(it),
            ItemId::Const(it) => it.into(),
            ItemId::Static(it) => it.into(),
            ItemId::Adt(_)
            | ItemId::Trait(_)
            | ItemId::Impl(_)
            | ItemId::Alias(_)
            | ItemId::Macro(_) => continue,
        };
        if let Ok(body) = db.mir_body(InferBodyId::DefWithBodyId(def)) {
            bodies.push(body);
        }
    }
    let mut at = 0;
    while at < bodies.len() {
        let body = bodies[at];
        for &closure in &body.closures {
            if let Ok(closure) = db.mir_body_for_closure(closure) {
                bodies.push(closure);
            }
        }
        at += 1;
    }
    bodies
}

#[test]
fn shared_type_forms_are_the_forms_each_cx_spelled() {
    for fixture in FIXTURES {
        let (db, _) = single(fixture);
        let mut forms = 0;
        for id in items(&db) {
            let path = key::path(&db, id);
            let (new, old) = (Cx::new(&db), Cx::reference(&db));
            assert_eq!(new.interface(id), old.interface(id), "the interface of `{path}`");
            assert_eq!(new.body(id), old.body(id), "the body of `{path}`");
            forms += 2;
            match id {
                ItemId::Function(_) => {
                    assert_eq!(
                        new.auto_traits(id, 0),
                        old.auto_traits(id, 0),
                        "the auto traits of `{path}`"
                    );
                    for closure in closures(&db, id) {
                        assert_eq!(
                            new.closure_code(closure),
                            old.closure_code(closure),
                            "a closure's code in `{path}`"
                        );
                        assert_eq!(
                            new.captures(closure),
                            old.captures(closure),
                            "a closure's captures in `{path}`"
                        );
                        forms += 2;
                    }
                }
                ItemId::Adt(AdtId::StructId(it)) => {
                    let variant = VariantId::StructId(it);
                    for (field, _) in it.fields(&db).fields().iter() {
                        assert_eq!(
                            new.field(variant, field),
                            old.field(variant, field),
                            "a field of `{path}`"
                        );
                        forms += 1;
                    }
                }
                ItemId::Adt(AdtId::EnumId(_) | AdtId::UnionId(_))
                | ItemId::Trait(_)
                | ItemId::Impl(_)
                | ItemId::Const(_)
                | ItemId::Static(_)
                | ItemId::Alias(_)
                | ItemId::Macro(_) => {}
            }
        }
        for body in mir_bodies(&db) {
            for (_, local) in body.locals.iter() {
                let ty = local.ty.as_ref();
                let old = Cx::reference(&db).reference_ty(ty);
                assert_eq!(*ty_form(&db, FormTy::new(&db, local.ty)), old, "the form of {ty:?}");
                assert_eq!(*Cx::new(&db).ty(ty), old.sub, "the form of {ty:?}");
                forms += 1;
            }
        }
        assert!(forms > 10, "{forms} forms compared of\n{fixture}");
    }
}

/// A walk's order, block numbers and unwinds, kept as `Canon` kept them before its columns.
type HashedWalk =
    (Vec<BasicBlockId>, HashMap<BasicBlockId, u32>, HashMap<(BasicBlockId, usize), BasicBlockId>);

fn hashed_walk(body: &MirBody<'_>) -> HashedWalk {
    let mut order = Vec::new();
    let mut blocks = HashMap::default();
    let mut stack = vec![body.start_block];
    while let Some(id) = stack.pop() {
        if blocks.contains_key(&id) {
            continue;
        }
        blocks.insert(id, order.len() as u32);
        order.push(id);
        if let Some(terminator) = &body.basic_blocks[id].terminator {
            stack.extend(successors(&terminator.kind).into_iter().rev());
        }
    }
    let unwinds = body
        .panic_unwinds
        .iter()
        .filter(|it| blocks.contains_key(&it.0) && blocks.contains_key(&it.2))
        .map(|&(block, at, to)| ((block, at), to))
        .collect();
    (order, blocks, unwinds)
}

fn successors(kind: &TerminatorKind) -> Vec<BasicBlockId> {
    match kind {
        TerminatorKind::Goto { target } => vec![*target],
        TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().to_vec(),
        TerminatorKind::Drop { target, unwind, .. }
        | TerminatorKind::DropAndReplace { target, unwind, .. }
        | TerminatorKind::FalseUnwind { real_target: target, unwind } => {
            std::iter::once(*target).chain(*unwind).collect()
        }
        TerminatorKind::Call { target, cleanup, .. } => {
            target.iter().chain(cleanup).copied().collect()
        }
        TerminatorKind::Assert { target, cleanup, .. } => {
            std::iter::once(*target).chain(*cleanup).collect()
        }
        TerminatorKind::Yield { resume, drop, .. } => {
            std::iter::once(*resume).chain(*drop).collect()
        }
        TerminatorKind::FalseEdge { real_target, imaginary_target } => {
            vec![*real_target, *imaginary_target]
        }
        TerminatorKind::UnwindResume
        | TerminatorKind::Abort
        | TerminatorKind::Return
        | TerminatorKind::Unreachable
        | TerminatorKind::CoroutineDrop
        | TerminatorKind::TailCall { .. } => Vec::new(),
    }
}

#[test]
fn the_dense_walk_is_the_hashed_walk() {
    let mut walked = 0;
    for fixture in FIXTURES {
        let (db, _) = single(fixture);
        for body in mir_bodies(&db) {
            let new = check::walk(body);
            let (order, blocks, unwinds) = hashed_walk(body);
            assert_eq!(new.order, order);
            let numbers: Vec<_> = body
                .basic_blocks
                .iter()
                .filter_map(|(id, _)| blocks.get(&id).map(|&number| (id, number)))
                .collect();
            assert_eq!(new.numbers, numbers);
            let unwinds = &unwinds;
            let unwinds: Vec<_> = order
                .iter()
                .flat_map(|&id| {
                    (0..body.basic_blocks[id].statements.len())
                        .filter_map(move |at| unwinds.get(&(id, at)).map(|&to| ((id, at), to)))
                })
                .collect();
            assert_eq!(new.unwinds, unwinds);
            walked += 1;
        }
    }
    assert!(walked > 10, "{walked} bodies walked");
}

#[test]
fn the_quiet_first_walk_finds_the_locals_a_written_one_does() {
    for fixture in FIXTURES {
        let (db, _) = single(fixture);
        for body in mir_bodies(&db) {
            let (quiet, written) = check::uses(&db, body);
            assert_eq!(quiet, written);
        }
    }
}

#[test]
fn a_path_written_in_place_is_the_path_spelled() {
    for fixture in FIXTURES {
        let (db, _) = single(fixture);
        for id in items(&db) {
            let path = key::path(&db, id);
            let mut written = Vec::new();
            key::path_part(&db, id, &mut |it| written.extend_from_slice(it));
            let mut spelled = (path.len() as u64).to_le_bytes().to_vec();
            spelled.extend_from_slice(path.as_bytes());
            assert_eq!(written, spelled, "the path of `{path}`");
        }
    }
}

#[test]
fn debug_and_counted_lengths_write_the_old_bytes() {
    let (new, old) = check::debug(&0u8);
    assert_eq!(new, old);
    let (new, old) = check::debug("a \"quoted\"\nstr");
    assert_eq!(new, old);
    let (new, old) = check::debug(&Some((1u32, -2i64)));
    assert_eq!(new, old);
    let (new, old) = check::debug(&[1u16, 2, 3][..]);
    assert_eq!(new, old);
    let (new, old) = check::debug(&Missed::ErrorType);
    assert_eq!(new, old);
    for len in [0usize, 1, 7, 8, 300] {
        let bytes: Vec<u8> = (0..len).map(|it| it as u8).collect();
        let (new, old) = check::counted(&bytes);
        assert_eq!(new, old, "{len} bytes");
    }
}

#[test]
fn slots_by_scan_are_slots_by_map() {
    let (db, _) = single(FIXTURES[0]);
    let all = items(&db);
    assert!(all.len() > 9, "{} items", all.len());
    for distinct in [1, 8, 9, all.len()] {
        let distinct = &all[..distinct];
        let ids: Vec<ItemId> = distinct
            .iter()
            .chain(distinct.iter().rev())
            .chain(distinct.iter().step_by(2))
            .copied()
            .collect();
        let (new, old) = check::slots(&ids);
        assert_eq!(new, old, "{} distinct items", distinct.len());
    }
}

#[test]
fn a_reused_buffer_starts_empty() {
    let (db, _) = single(FIXTURES[0]);
    let all = items(&db);
    for _ in 0..3 {
        let (digest, spans, refs, slots) = check::reused(&all);
        assert_eq!(digest, 0u64.to_le_bytes(), "only the empty tag's length");
        assert!(spans.is_empty());
        assert!(refs.is_empty());
        assert_eq!(slots, 0);
    }
}
