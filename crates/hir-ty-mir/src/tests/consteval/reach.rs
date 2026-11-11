//! bstd's compile-time reachability walk (`crates/blaze-std/src/own/reach.rs`,
//! docs/v2/plan/design/ownership.md), evaluated by this evaluator over a fixture's types: the
//! walk's text is the file itself (`include_str!`), over a fixture `raw` that reads the
//! reflection intrinsics directly. `Acyclic` is one question; a capability and interior
//! mutability are two more.
//!
//! A cleared type is `GOAL == true`; a refusal is the panic whose message is the path, which a
//! `where const` bound shows (`hir-ty/src/tests/where_const.rs`).

use hegel::{TestCase, generators as gs};

use super::*;

/// The walk itself.
const WALK: &str = include_str!("../../../../blaze-std/src/own/reach.rs");

/// What the walk stands on in a fixture: the reflection intrinsics, `raw` over them, and
/// stand-ins for std's containers with their `Owns` impls.
const BASE: &str = r#"
//- minicore: option, slice, matches, phantom_data, copy, clone, derive, str, panic, pointee, transmute, sized
#![feature(lang_items, marker_trait_attr)]
#[lang = "type_id"]
#[derive(Clone, Copy)]
pub struct TypeId { t: (u64, u64) }

#[rustc_intrinsic] pub const fn type_id<T: ?Sized>() -> TypeId;
#[rustc_intrinsic] pub const fn type_id_variants(id: TypeId) -> usize;
#[rustc_intrinsic] pub const fn type_id_fields(id: TypeId, variant: usize) -> usize;
#[rustc_intrinsic]
pub const fn type_id_field_representing_type(id: TypeId, variant: usize, field: usize) -> TypeId;
#[rustc_intrinsic] pub const fn field_representing_type_actual_type_id(id: TypeId) -> TypeId;
#[rustc_intrinsic] pub const fn field_representing_type_name(id: TypeId) -> &'static str;
#[rustc_intrinsic] pub const fn type_id_variant_name(id: TypeId, variant: usize) -> &'static str;
#[rustc_intrinsic] pub const fn type_id_element_ty(id: TypeId) -> Option<TypeId>;
#[rustc_intrinsic] pub const fn type_id_points_to(id: TypeId) -> Option<TypeId>;
#[rustc_intrinsic] pub const fn type_id_type_of(id: TypeId) -> Type;
#[rustc_intrinsic] pub const fn type_id_generics(id: TypeId) -> &'static [Generic];
#[rustc_intrinsic]
pub const fn type_id_vtable(id: TypeId, of: TypeId) -> Option<core::ptr::DynMetadata<*const ()>>;
#[rustc_intrinsic] pub const fn blaze_type_name_of(id: TypeId) -> &'static str;

#[lang = "type_info"]
pub struct Type { pub kind: TypeKind }
pub enum TypeKind {
    Tuple, Array, Slice, DynTrait(DynTrait), Struct, Enum, Union, Bool, Char, Int, Float,
    Str(Str), Reference, Pointer, FnPtr, Other,
}
pub struct DynTrait { pub predicates: &'static [DynTraitPredicate] }
pub struct DynTraitPredicate { pub trait_ty: Trait }
pub struct Trait { pub ty: TypeId, pub is_auto: bool }
pub struct Str {}
#[lang = "type_info_generic"]
pub enum Generic { Lifetime(Lifetime), Type(GenericType), Const(Const) }
pub struct Lifetime {}
pub struct GenericType { pub ty: TypeId }
pub struct Const { pub ty: TypeId }
#[lang = "field_representing_type"]
pub struct FieldRepresentingType<T: ?Sized, const VARIANT: u32, const FIELD: u32> {
    _phantom: core::marker::PhantomData<fn(T) -> T>,
}

pub mod raw {
    pub use crate::TypeId;
    use crate::*;
    use crate::reach::Shape;

    pub const fn id<T: ?Sized>() -> TypeId { type_id::<T>() }
    pub const fn same(a: TypeId, b: TypeId) -> bool { a.t.0 == b.t.0 && a.t.1 == b.t.1 }
    pub const fn shape(id: TypeId) -> Shape {
        match type_id_type_of(id).kind {
            TypeKind::Bool | TypeKind::Char | TypeKind::Int | TypeKind::Float
            | TypeKind::Str(_) | TypeKind::FnPtr => Shape::Leaf,
            TypeKind::Tuple | TypeKind::Struct | TypeKind::Enum => Shape::Parts,
            TypeKind::Array | TypeKind::Slice => Shape::Elements,
            TypeKind::Reference => Shape::Borrow,
            TypeKind::Pointer => Shape::Pointer,
            TypeKind::DynTrait(_) => Shape::Dyn,
            _ => Shape::Unseen,
        }
    }
    pub const fn variants(id: TypeId) -> usize { type_id_variants(id) }
    pub const fn fields(id: TypeId, variant: usize) -> usize { type_id_fields(id, variant) }
    pub const fn field(id: TypeId, variant: usize, field: usize) -> TypeId {
        field_representing_type_actual_type_id(type_id_field_representing_type(id, variant, field))
    }
    pub const fn field_name(id: TypeId, variant: usize, field: usize) -> &'static str {
        field_representing_type_name(type_id_field_representing_type(id, variant, field))
    }
    pub const fn variant_name(id: TypeId, variant: usize) -> &'static str {
        type_id_variant_name(id, variant)
    }
    pub const fn inner(id: TypeId) -> Option<TypeId> {
        match type_id_element_ty(id) {
            Some(element) => Some(element),
            None => type_id_points_to(id),
        }
    }
    // The fixture's slices have no const `len`: a stand-in has at most three arguments.
    pub const fn args(id: TypeId) -> usize {
        match type_id_generics(id) {
            [] => 0,
            [_] => 1,
            [_, _] => 2,
            _ => 3,
        }
    }
    pub const fn arg(id: TypeId, at: usize) -> Option<TypeId> {
        let generic = match (type_id_generics(id), at) {
            ([it, ..], 0) | ([_, it, ..], 1) | ([_, _, it, ..], 2) => it,
            _ => return None,
        };
        match generic {
            Generic::Type(it) => Some(it.ty),
            _ => None,
        }
    }
    pub const fn implements(id: TypeId, marker: TypeId) -> bool {
        match type_id_vtable(id, marker) {
            Some(_) => true,
            None => false,
        }
    }
    pub const fn name(id: TypeId) -> &'static str { blaze_type_name_of(id) }
    /// The planted bug of `the_oracle_catches_a_visited_set_keyed_by_name`: two types are
    /// taken for one when their names agree up to the first `<`.
    pub const fn same_name(a: TypeId, b: TypeId) -> bool {
        let (a, b) = (name(a).as_bytes(), name(b).as_bytes());
        let mut at = 0;
        loop {
            let a_ends = at == a.len() || a[at] == b'<';
            let b_ends = at == b.len() || b[at] == b'<';
            if a_ends || b_ends {
                return a_ends && b_ends;
            }
            if a[at] != b[at] {
                return false;
            }
            at += 1;
        }
    }
    #[repr(C)]
    struct Fat { ptr: *const u8, len: usize }
    pub const fn text(bytes: &[u8], len: usize) -> &str {
        // SAFETY: a `&str` is its bytes' address and length, and `len` whole pieces of text
        // were written to `bytes`.
        unsafe {
            core::mem::transmute::<Fat, &str>(Fat {
                ptr: bytes as *const [u8] as *const u8,
                len,
            })
        }
    }
}

pub mod reach {
"#;

/// After the walk: the stand-ins for std's containers.
const STAND_INS: &str = r#"
}

use reach::{Acyclic, Cycle, Inline, Jointly, Mutably, Nothing, Owns, Solely, none};

pub struct Box<T: ?Sized>(*mut T);
pub struct Vec<T>(*mut T, usize);
pub struct Rc<T: ?Sized>(*const T);
pub struct Weak<T: ?Sized>(*const T);
pub struct UnsafeCell<T: ?Sized>(T);
pub struct RefCell<T: ?Sized> { borrows: isize, value: UnsafeCell<T> }
/// No `Owns` impl: the walk finds the cell inside by its fields.
pub struct Mutex<T: ?Sized> { state: u8, value: UnsafeCell<T> }
/// A generic struct with no `Owns` impl.
pub struct W<T> { w: T }
unsafe impl<T> Owns<Inline> for Option<T> {}
unsafe impl<T: ?Sized> Owns<Solely> for Box<T> {}
unsafe impl<T> Owns<Solely> for Vec<T> {}
unsafe impl<T: ?Sized> Owns<Jointly> for Rc<T> {}
unsafe impl<T: ?Sized> Owns<Nothing> for Weak<T> {}
unsafe impl<T: ?Sized> Owns<Mutably> for UnsafeCell<T> {}
unsafe impl<T: ?Sized> Owns<Mutably> for RefCell<T> {}
"#;

/// A fixture: the walk, the stand-ins, `items`, and `GOAL`.
fn fixture_of(walk: &str, items: &str, goal: &str) -> String {
    format!("{BASE}{walk}{STAND_INS}{items}\nconst GOAL: bool = {goal};\n")
}

/// The walk clears `ty` for the cycle question.
#[track_caller]
fn acyclic(items: &str, ty: &str) {
    check_number(&fixture_of(WALK, items, &format!("none::<{ty}, Cycle>()")), 1);
}

/// The walk refuses `goal` with exactly `path`.
#[track_caller]
fn refused(items: &str, goal: &str, path: &str) {
    let path = path.to_owned();
    check_fail(&fixture_of(WALK, items, goal), move |e| {
        e == ConstEvalError::MirEvalError(MirEvalError::Panic(path))
    });
}

/// The walk refuses `ty` for the cycle question with exactly `path`.
#[track_caller]
fn cyclic(items: &str, ty: &str, path: &str) {
    refused(items, &format!("none::<{ty}, Cycle>()"), path);
}

#[test]
fn recursion_through_sole_ownership_is_acyclic() {
    acyclic("struct List { head: u8, tail: Option<Box<List>> }", "List");
    acyclic("struct Tree { kids: Vec<Tree> }", "Tree");
    // Two types that hold each other, each behind a pointer it alone owns: the walk ends.
    acyclic("struct A { b: Option<Box<B>> } struct B { a: Vec<A>, n: (u8, [u16; 2]) }", "A");
}

#[test]
fn a_counted_pointer_fixed_at_creation_is_acyclic() {
    acyclic("struct Bc<T> { x: Rc<T> }", "Bc<u8>");
    // `Bc<Bc<u8>>` reaches a counted pointer, but nothing sets one after the value exists.
    acyclic("struct Bc<T> { x: Rc<T> }", "Bc<Bc<u8>>");
    // A weak pointer owns nothing, behind a cell or not.
    acyclic("struct Back { parent: RefCell<Weak<Back>>, up: Weak<Back> }", "Back");
    // A reference owns nothing.
    acyclic("struct Lent<'a> { cell: &'a RefCell<Option<Rc<Lent<'a>>>> }", "Lent<'static>");
    // A cell with no counted pointer in it.
    acyclic("struct Count { n: RefCell<u32>, list: Mutex<Vec<Box<Count>>> }", "Count");
}

#[test]
fn a_counted_pointer_behind_a_cell_that_leads_back_is_refused_with_its_path() {
    cyclic(
        "struct Node { next: RefCell<Option<Rc<Node>>> }",
        "Node",
        "`Node.next`: a `Rc<Node>` behind `RefCell<Option<Rc<Node>>>` reaches `Node`",
    );
    let family = "struct Parent { kids: Vec<Rc<Child>> }
                  struct Child { parent: RefCell<Option<Rc<Parent>>> }";
    cyclic(
        family,
        "Child",
        "`Child.parent`: a `Rc<Parent>` behind `RefCell<Option<Rc<Parent>>>` reaches \
         `Parent.kids: Vec<Rc<Child>>`, which reaches `Child`",
    );
    cyclic(
        family,
        "Parent",
        "`Parent.kids`, then `Child.parent`: a `Rc<Parent>` behind \
         `RefCell<Option<Rc<Parent>>>` reaches `Parent`",
    );
    // Through a tuple and an enum's variant, each named.
    cyclic(
        "struct T2 { pair: (u8, RefCell<Option<Rc<T2>>>) }",
        "T2",
        "`T2.pair`, then `(u8, RefCell<Option<Rc<T2>>>).1`: a `Rc<T2>` behind \
         `RefCell<Option<Rc<T2>>>` reaches `T2`",
    );
    cyclic(
        "enum E { Leaf, Link(RefCell<Option<Rc<E>>>) }",
        "E",
        "`E::Link.0`: a `Rc<E>` behind `RefCell<Option<Rc<E>>>` reaches `E`",
    );
}

/// `Mutex<Vec<Rc<Job>>>` is acyclic exactly when `Job` cannot reach the type holding the
/// mutex; the cell is found through `Mutex`'s own fields.
#[test]
fn a_pool_is_acyclic_exactly_when_its_jobs_cannot_reach_it() {
    acyclic("struct Pool { jobs: Mutex<Vec<Rc<Job>>> } struct Job { n: u32 }", "Pool");
    let looped = "struct Pool { jobs: Mutex<Vec<Rc<Job>>> } struct Job { pool: Rc<Pool> }";
    cyclic(
        looped,
        "Pool",
        "`Pool.jobs`, then `Mutex<Vec<Rc<Job>>>.value`: a `Rc<Job>` behind \
         `UnsafeCell<Vec<Rc<Job>>>` reaches `Job.pool: Rc<Pool>`, which reaches `Pool`",
    );
    // What holds the pool is not on the cycle: no chain leads from a `Holder` back to it.
    acyclic(&format!("{looped} struct Holder {{ pool: Pool }}"), "Holder");
}

/// The answer is the instance's: `Slot<u8>` is acyclic and `Slot<Loop>` is not, in one
/// program, and the walk of one does not answer for the other.
#[test]
fn a_generic_type_is_decided_per_instance() {
    let items = "struct Slot<T> { at: RefCell<Option<Rc<T>>> } struct Loop { slot: Slot<Loop> }";
    acyclic(items, "Slot<u8>");
    cyclic(
        items,
        "Slot<Loop>",
        "`Slot<Loop>.at`: a `Rc<Loop>` behind `RefCell<Option<Rc<Loop>>>` reaches \
         `Loop.slot: Slot<Loop>`, which reaches `Slot<Loop>`",
    );
    // Both instances on one path: `W<u8>` walked first does not stand for `W<Slot<Loop>>`.
    cyclic(
        &format!("{items} struct Two {{ a: W<u8>, b: W<RefCell<Option<Rc<Two>>>> }}"),
        "Two",
        "`Two.b`, then `W<RefCell<Option<Rc<Two>>>>.w`: a `Rc<Two>` behind \
         `RefCell<Option<Rc<Two>>>` reaches `Two`",
    );
}

#[test]
fn what_reflection_cannot_see_into_is_refused() {
    cyclic(
        "struct Raw { p: *mut u8 }",
        "Raw",
        "`Raw.p`: a `*mut u8`, a raw pointer, which reflection does not see through",
    );
    cyclic(
        "union U { a: u8, b: u16 } struct HasU { u: U }",
        "HasU",
        "`HasU.u`: a `U`, which reflection does not see into (a union, a closure or a \
         foreign type)",
    );
    cyclic(
        "trait Any {} struct Holds { job: Box<dyn Any> }",
        "Holds",
        "`Holds.job`: a `dyn Any`, whose hidden type reflection does not see",
    );
    // Unseen on the way back from a counted pointer behind a cell: it could lead back.
    cyclic(
        "trait Any {} struct Open { next: RefCell<Option<Rc<Box<dyn Any>>>> }",
        "Open",
        "`Open.next`: a `Rc<Box<dyn Any>>` behind `RefCell<Option<Rc<Box<dyn Any>>>>` reaches \
         a `dyn Any`, whose hidden type reflection does not see, so it could reach `Open`",
    );
}

/// A trait with `Acyclic` as a supertrait answers for its `dyn` type where it is owned
/// outside every cell; behind a cell the hidden type could hold a pointer back, so it is
/// refused whatever the trait says.
#[test]
fn a_dyn_type_whose_trait_is_acyclic_is_passed_over_outside_a_cell() {
    let job = "trait Job: Acyclic {} struct Queue { first: Box<dyn Job>, rest: Vec<Rc<dyn Job>> }";
    acyclic(job, "Queue");
    cyclic(
        "trait Job: Acyclic {} struct Slot { job: RefCell<Option<Box<dyn Job>>> }",
        "Slot",
        "`Slot.job`: a `dyn Job` behind `RefCell<Option<Box<dyn Job>>>`, whose hidden type \
         could hold a pointer back",
    );
    // The trait of the type the walk starts at is not asked: the type is being decided.
    cyclic(
        "trait Job: Acyclic {}",
        "dyn Job",
        "a `dyn Job`, whose hidden type reflection does not see",
    );
}

/// The same walk with another question: is a capability reachable, owned or borrowed, and is
/// there interior mutability in the value's own bytes.
#[test]
fn the_walk_answers_other_questions() {
    let items = "
        use reach::{Ask, Find, Question, Through};
        trait Capability {}
        struct Dir;
        impl Capability for Dir {}
        struct AnyCapability;
        impl Question for AnyCapability {
            const ASK: Ask = Ask {
                find: Find::Implements(raw::id::<dyn Capability>()),
                through: Through::Borrowed,
                noun: \"a capability\",
                unseen_refuses: true,
                vouch: None,
            };
        }
        struct AnyCell;
        impl Question for AnyCell {
            const ASK: Ask = Ask {
                find: Find::Implements(raw::id::<dyn Owns<Mutably>>()),
                through: Through::InPlace,
                noun: \"interior mutability\",
                unseen_refuses: true,
                vouch: None,
            };
        }
        struct Plain { n: u32, list: Vec<(u8, Option<Box<Plain>>)> }
        struct Config { name: u32, paths: Paths }
        struct Paths { extra: Vec<u8>, root: Dir }
        struct Lent<'a> { dir: Option<&'a Dir> }
        struct Key { id: u32, far: Box<RefCell<u8>> }
        struct Counted { hits: RefCell<u8> }
    ";
    check_number(&fixture_of(WALK, items, "none::<Plain, AnyCapability>()"), 1);
    refused(
        items,
        "none::<Config, AnyCapability>()",
        "`Config.paths: Paths`, then `Paths.root: Dir`: `Dir` is a capability",
    );
    // Borrowed counts: the path goes through the reference (whose spelling is the
    // printer's, so only the end is compared).
    check_fail(&fixture_of(WALK, items, "none::<Lent<'static>, AnyCapability>()"), |e| {
        matches!(
            e,
            ConstEvalError::MirEvalError(MirEvalError::Panic(path))
                if path.contains(".dir: ") && path.ends_with(": `Dir` is a capability")
        )
    });
    // A cell behind a pointer is not in the value's own bytes; one in a field is.
    check_number(&fixture_of(WALK, items, "none::<Key, AnyCell>()"), 1);
    refused(
        items,
        "none::<Counted, AnyCell>()",
        "`Counted.hits: RefCell<u8>`: `RefCell<u8>` is interior mutability",
    );
}

// The property test: random type graphs against a reference model.

/// A field's type in a generated program.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Ty {
    Byte,
    /// The struct `S{0}`.
    S(usize),
    Opt(Box<Ty>),
    Boxed(Box<Ty>),
    List(Box<Ty>),
    Counted(Box<Ty>),
    Weak(Box<Ty>),
    Cell(Box<Ty>),
    /// `W<..>`: a generic struct the walk reads by its field.
    Wrap(Box<Ty>),
}

impl Ty {
    fn text(&self) -> String {
        match self {
            Ty::Byte => "u8".to_owned(),
            Ty::S(at) => format!("S{at}"),
            Ty::Opt(it) => format!("Option<{}>", it.text()),
            Ty::Boxed(it) => format!("Box<{}>", it.text()),
            Ty::List(it) => format!("Vec<{}>", it.text()),
            Ty::Counted(it) => format!("Rc<{}>", it.text()),
            Ty::Weak(it) => format!("Weak<{}>", it.text()),
            Ty::Cell(it) => format!("RefCell<{}>", it.text()),
            Ty::Wrap(it) => format!("W<{}>", it.text()),
        }
    }

    /// What a value of the type owns directly.
    fn owned(&self, structs: &[Vec<Ty>]) -> Vec<Ty> {
        match self {
            Ty::Byte | Ty::Weak(_) => Vec::new(),
            Ty::S(at) => structs[*at].clone(),
            Ty::Opt(it)
            | Ty::Boxed(it)
            | Ty::List(it)
            | Ty::Counted(it)
            | Ty::Cell(it)
            | Ty::Wrap(it) => vec![(**it).clone()],
        }
    }
}

/// Every type a value of `from` owns, at any depth, `from` included.
fn owned_by(from: &Ty, structs: &[Vec<Ty>]) -> Vec<Ty> {
    let mut seen = vec![from.clone()];
    let mut at = 0;
    while at < seen.len() {
        for next in seen[at].owned(structs) {
            if !seen.contains(&next) {
                seen.push(next);
            }
        }
        at += 1;
    }
    seen
}

/// The reference model: `S{root}` can own itself when some cell it owns (at any depth) owns
/// (at any depth) a counted pointer whose pointee owns (at any depth) an `S{root}`. An
/// explicit search with no pruning and no position flag, over the same description.
fn model_is_acyclic(structs: &[Vec<Ty>], root: usize) -> bool {
    let root = Ty::S(root);
    for cell in owned_by(&root, structs) {
        let Ty::Cell(inside) = &cell else { continue };
        for pointer in owned_by(inside, structs) {
            let Ty::Counted(pointee) = &pointer else { continue };
            if owned_by(pointee, structs).contains(&root) {
                return false;
            }
        }
    }
    true
}

/// The program of `structs`.
fn program(structs: &[Vec<Ty>]) -> String {
    let mut out = String::new();
    for (at, fields) in structs.iter().enumerate() {
        out.push_str(&format!("struct S{at} {{ "));
        for (field, ty) in fields.iter().enumerate() {
            out.push_str(&format!("f{field}: {}, ", ty.text()));
        }
        out.push_str("}\n");
    }
    out
}

/// What the walk `walk` says of `S{root}`: `Some(acyclic)`, or `None` when its evaluation
/// failed some other way than a refusal's panic.
fn walk_says(walk: &str, structs: &[Vec<Ty>], root: usize) -> Option<bool> {
    let fixture = fixture_of(walk, &program(structs), &format!("none::<S{root}, Cycle>()"));
    let fx = fixture::db(&fixture);
    let file_id = fx.only_file();
    let db = fx.into_db();
    blaze_query::db::attach(&db, || match eval_goal(&db, file_id) {
        Ok(_) => Some(true),
        Err(e) => match simplify(e) {
            ConstEvalError::MirEvalError(MirEvalError::Panic(_)) => Some(false),
            _ => None,
        },
    })
}

/// A type for a field of `S{of}` among `count` structs. In place (`behind` false) only a
/// later struct may be named, so no generated struct holds itself by value; behind a pointer
/// any may.
fn draw_ty(tc: &TestCase, of: usize, count: usize, behind: bool, depth: u8) -> Ty {
    let pick = tc.draw(gs::integers::<u8>().max_value(if depth == 0 { 1 } else { 8 }));
    let inner = |behind: bool| Box::new(draw_ty(tc, of, count, behind, depth - 1));
    match pick {
        0 => Ty::Byte,
        1 => {
            let first = if behind { 0 } else { of + 1 };
            if first >= count {
                return Ty::Byte;
            }
            let at = tc.draw(gs::integers::<usize>().max_value(count - 1 - first));
            Ty::S(first + at)
        }
        2 => Ty::Opt(inner(behind)),
        3 => Ty::Boxed(inner(true)),
        4 => Ty::List(inner(true)),
        5 => Ty::Counted(inner(true)),
        6 => Ty::Weak(inner(true)),
        7 => Ty::Cell(inner(behind)),
        _ => Ty::Wrap(inner(behind)),
    }
}

fn draw_structs(tc: &TestCase) -> Vec<Vec<Ty>> {
    let count = 1 + tc.draw(gs::integers::<usize>().max_value(3));
    (0..count)
        .map(|of| {
            let fields = tc.draw(gs::integers::<usize>().max_value(3));
            (0..fields).map(|_| draw_ty(tc, of, count, false, 3)).collect()
        })
        .collect()
}

/// The invariant: for a random program of structs whose fields are plain, boxed, in a `Vec`,
/// behind a counted or weak pointer, in a cell or in a generic struct, in any nesting and
/// recursion, the walk clears a struct for the cycle question exactly when the reference
/// model ([`model_is_acyclic`]) finds no way for a value of it to own itself.
#[hegel::test(test_cases = 40)]
fn the_walk_agrees_with_the_reference_model(tc: TestCase) {
    let structs = draw_structs(&tc);
    let root = tc.draw(gs::integers::<usize>().max_value(structs.len() - 1));
    let model = model_is_acyclic(&structs, root);
    let walk = walk_says(WALK, &structs, root);
    assert_eq!(walk, Some(model), "S{root} of\n{}", program(&structs));
}

/// The walk with one line replaced, which must be there to replace.
fn planted(from: &str, to: &str) -> String {
    assert_eq!(WALK.matches(from).count(), 1, "the line a control replaces: {from:?}");
    WALK.replace(from, to)
}

/// Whether the oracle (the walk against the model) faults `walk` on `S0` of `structs`.
fn oracle_faults(walk: &str, structs: &[Vec<Ty>]) -> bool {
    walk_says(walk, structs, 0) != Some(model_is_acyclic(structs, 0))
}

fn cell(inside: Ty) -> Ty {
    Ty::Cell(Box::new(inside))
}
fn counted(inside: Ty) -> Ty {
    Ty::Counted(Box::new(inside))
}

/// Control: a walk that forgets it is behind a cell when it goes through an `Option` clears
/// `S0 { f0: RefCell<Option<Rc<S0>>> }`, and the oracle faults it. The real walk passes.
#[test]
fn the_oracle_catches_a_shared_pointer_inside_an_option_in_a_cell_missed() {
    let structs = vec![vec![cell(Ty::Opt(Box::new(counted(Ty::S(0)))))]];
    let bug = planted(
        "        Holds::Inline => {\n            walk.add_args(at, cell);",
        "        Holds::Inline => {\n            walk.add_args(at, NONE);",
    );
    assert!(oracle_faults(&bug, &structs));
    assert!(!oracle_faults(WALK, &structs));
}

/// Control: a walk that takes any pointer back to the start for a cycle refuses
/// `S0 { f0: Option<Box<S0>> }`, and the oracle faults it. The real walk passes.
#[test]
fn the_oracle_catches_a_box_recursion_taken_for_a_cycle() {
    let structs = vec![vec![Ty::Opt(Box::new(Ty::Boxed(Box::new(Ty::S(0)))))]];
    let bug = planted(
        "        Holds::Solely => {\n            if owned {",
        "        Holds::Solely => {\n            if at != 0 && raw::same(raw::arg(ty, 0).unwrap(), start) {\n                return Some(too_large(start));\n            }\n            if owned {",
    );
    assert!(oracle_faults(&bug, &structs));
    assert!(!oracle_faults(WALK, &structs));
}

/// Control: a walk whose visited set is keyed by a type's name up to its arguments takes
/// `W<RefCell<Option<Rc<S0>>>>` for the `W<u8>` it walked first and clears `S0`, and the
/// oracle faults it. The real walk, keyed by the instance's id, passes.
#[test]
fn the_oracle_catches_a_visited_set_keyed_by_name() {
    let structs = vec![vec![
        Ty::Wrap(Box::new(Ty::Byte)),
        Ty::Wrap(Box::new(cell(Ty::Opt(Box::new(counted(Ty::S(0))))))),
    ]];
    let bug = planted(
        "if raw::same(seen.ty, ty) && (seen.cell == NONE) == (cell == NONE) {",
        "if raw::same_name(seen.ty, ty) && (seen.cell == NONE) == (cell == NONE) {",
    );
    assert!(oracle_faults(&bug, &structs));
    assert!(!oracle_faults(WALK, &structs));
}
