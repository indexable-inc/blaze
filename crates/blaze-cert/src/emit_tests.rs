//! The emitter's tests (emit.rs): a recording solver's derivation of each fixture's root is
//! accepted by the checker, equals the hand-built derivation where tests.rs has one (so the
//! planted controls there are mutations of the emitted bytes), is refused when the facts it
//! rests on move, stays accepted when an unrelated impl is added, is the same bytes whatever
//! order the impls are declared in and in another arena, and the recording solver answers as
//! the plain one. A rule the certificate cannot express is a typed `NotCertifiable`.
//!
//! What a moved fact does is V1's property (runtime.md row V1, certificates.md "The
//! invariant"): a where-clause edit is one linear check that refuses the old derivation; a new
//! impl that could match is the questions' half (tests.rs), not the derivation's.

use std::println;
use std::vec::Vec as StdVec;

use blaze_own::{Alloc, Vec};
use blaze_solve::assembly::Rule;
use blaze_solve::builtin;
use blaze_solve::eval::Answer;
use blaze_solve::term::{TyKind, TypingMode, UintTy};
use blaze_solve::tys::Tys;
use blaze_solve::witness::Derivations;

use crate::check::CheckError;
use crate::derivation::{Builtin, StepId, View};
use crate::emit::{Emitter, NotCertifiable};
use crate::tests::{CHAIN, Fx, K, Row, SIZED, alloc, by, bytes, chain_rows, fixture, fixture_with, imp};

fn recording() -> Derivations<'static> {
    Derivations::new_in(alloc())
}

/// Goal `i` of `fx` solved (`yes`), then its certificate.
fn certify(fx: &mut Fx<Derivations<'static>>, em: &mut Emitter<'static>, i: usize) -> Result<StdVec<u8>, NotCertifiable> {
    let (a, _) = fx.solve(i);
    assert_eq!(a, Answer::Yes, "goal {i} (line {})", fx.case.goals[i].line);
    let pred = fx.goal(i);
    let mut out: Vec<u8, Alloc<'static>> = Vec::new_in(alloc());
    em.emit(&mut fx.solver, &mut fx.tys, &fx.world, TypingMode::Analysis, fx.env, pred, &mut out)?;
    Ok(out.iter().copied().collect())
}

/// The root's certificate in a fresh recording fixture of `text` (and `extra`).
fn root_certificate(text: &str, extra: &str) -> (Fx<Derivations<'static>>, StdVec<u8>) {
    let mut fx = fixture_with(text, extra, recording());
    let mut em = Emitter::new_in(alloc());
    let b = certify(&mut fx, &mut em, 0).unwrap_or_else(|e| panic!("not certifiable: {e:?}"));
    (fx, b)
}

const CHAIN_MOVED: &str = "
case chain = cert three-deep impl chain, ia's where-clause changed
trait A
trait B
trait C
struct W<T>(T)
impl@ia<T: C> A for W<T>
impl@ib<T: C> B for T
impl@ic C for u32
goal W<u32>: A => yes
";

const CHAIN_REORDERED: &str = "
case chain = cert three-deep impl chain, the impls in another order
trait A
trait B
trait C
struct W<T>(T)
impl@ic C for u32
impl@ib<T: C> B for T
impl@ia<T: B> A for W<T>
goal W<u32>: A => yes
";

const PARAM_ENV: &str = "
case penv = cert a param-env bound under an impl
trait Foo
struct W<T>(T)
impl@wf<T: Foo> Foo for W<T>
params T
env T: Foo
goal W<T>: Foo => yes
";

const NESTED_IMPLS: &str = "
case vec = cert Vec<T>: Clone nesting through two impls
goal Vec<Option<String>>: Clone => yes
";

const TUPLE: &str = "
case clone = cert builtin copy clone of a tuple
struct K
impl@kc Clone for K
goal (K, K): Clone => yes
goal K: Clone => yes
";

const SIZED_TUPLE: &str = "
case sized = cert builtin sized through a tuple and a struct
struct W<T>(T)
goal (u32, W<u32>): Sized => yes
";

const AUTO: &str = "
case auto = cert auto trait through a struct's fields
struct P(u32, String)
goal P: Send => yes
";

const NODE: &str = "
case node = cert coinductive auto cycle
struct Node(Box<Node>)
goal Node: Send => yes
goal Box<Node>: Send => yes
";

const PROJECTION: &str = "
case proj = cert projection
trait Get { type Out; }
struct W<T>(T)
impl@ig<T> Get for W<T> { type Out = T; }
goal <W<u32> as Get>::Out == u32 => yes
goal u32: Sized => yes
";

const OBJECT: &str = "
case object = cert object bound
goal dyn Debug + Send: Send => yes
goal dyn Debug: Debug => yes
";

const CLOSURE: &str = "
case closure = cert closure Fn goal
goal closure#0<fn, fn((u8,)) -> u8, ()>: Fn<(u8,)> => yes
";

/// Every fixture whose root is certifiable.
const CERTIFIABLE: [&str; 10] = [CHAIN, CHAIN_REORDERED, PARAM_ENV, NESTED_IMPLS, TUPLE, SIZED_TUPLE, AUTO, NODE, PROJECTION, OBJECT];

#[test]
fn every_fixture_root_is_accepted() {
    for text in CERTIFIABLE {
        let (mut fx, b) = root_certificate(text, "");
        fx.check(&b).unwrap_or_else(|e| panic!("{}: {e}", fx.case.name));
    }
    // The object fixture's second root: the principal.
    let mut fx = fixture_with(OBJECT, "", recording());
    let mut em = Emitter::new_in(alloc());
    let b = certify(&mut fx, &mut em, 1).unwrap_or_else(|e| panic!("{e:?}"));
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn chain_is_the_hand_built_derivation() {
    let (mut fx, b) = root_certificate(CHAIN, "");
    let mut hand = fixture(CHAIN, "");
    assert_eq!(b, bytes(&mut hand, &chain_rows(&[UintTy::U32], &[0, 2]), &[]));
    fx.check(&b).unwrap_or_else(|e| panic!("{e}"));
}

#[test]
fn mutations_of_the_emitted_chain_are_refused() {
    let (mut fx, _) = root_certificate(CHAIN, "");
    // A wrong impl key at the root: `ib` is an impl of `B`.
    let mut rows = chain_rows(&[UintTy::U32], &[0, 2]);
    rows[3] = (0, by(K::Impl("ib"), &[UintTy::U32], &[0, 2]));
    let b = bytes(&mut fx, &rows, &[]);
    assert_eq!(fx.check(&b), Err(CheckError::HeaderMismatch { step: StepId(3), impl_: imp("ib") }));
    // A dropped sub-goal.
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U32], &[0]), &[]);
    assert_eq!(fx.check(&b), Err(CheckError::NestedCount { step: StepId(3), want: 2, have: 1 }));
    // A swapped substitution.
    let b = bytes(&mut fx, &chain_rows(&[UintTy::U8], &[0, 2]), &[]);
    assert_eq!(fx.check(&b), Err(CheckError::HeaderMismatch { step: StepId(3), impl_: imp("ia") }));
}

#[test]
fn moved_where_clause_refuses_and_unrelated_impl_accepts() {
    let (_, b) = root_certificate(CHAIN, "");
    // `ia` now asks `T: C`: its second nested step proves `u32: B`.
    let mut moved = fixture(CHAIN_MOVED, "");
    assert_eq!(moved.check(&b), Err(CheckError::NestedMismatch { step: StepId(3), clause: 1 }));
    // An impl of another trait added: the old derivation checks, and a new one is the same.
    let mut more = fixture(CHAIN, "impl@d16 Debug for u16");
    more.check(&b).unwrap_or_else(|e| panic!("{e}"));
    let (_, b2) = root_certificate(CHAIN, "impl@d16 Debug for u16");
    assert_eq!(b, b2);
}

#[test]
fn bytes_equal_across_declaration_order_and_arenas() {
    let (_, b) = root_certificate(CHAIN, "");
    let (_, reordered) = root_certificate(CHAIN_REORDERED, "");
    assert_eq!(b, reordered);
    // A second arena whose handles differ: a term made first shifts every later one.
    let mut fx = fixture_with(CHAIN, "", recording());
    fx.tys.mk(TyKind::Uint(UintTy::U128)).unwrap_or_else(|e| panic!("{e:?}"));
    let mut em = Emitter::new_in(alloc());
    assert_eq!(certify(&mut fx, &mut em, 0).unwrap_or_else(|e| panic!("{e:?}")), b);
}

#[test]
fn shared_rows_and_back_edges_are_the_hand_built_ones() {
    // `(K, K): Clone`: one `K: Clone` row named twice.
    let (_, b) = root_certificate(TUPLE, "");
    let mut hand = fixture(TUPLE, "");
    let rows = [(1, by(K::Impl("kc"), &[], &[])), (0, by(K::Builtin(Builtin::CopyClone), &[], &[0, 0]))];
    assert_eq!(b, bytes(&mut hand, &rows, &[]));
    // `Node: Send`: a back-edge closed with the root.
    let (_, b) = root_certificate(NODE, "");
    let mut hand = fixture(NODE, "");
    let rows = [(0, Row::Back), (1, by(K::Auto, &[], &[0])), (0, by(K::Auto, &[], &[1]))];
    assert_eq!(b, bytes(&mut hand, &rows, &[(0, 2)]));
    // `<W<u32> as Get>::Out == u32` by the impl's value.
    let (_, b) = root_certificate(PROJECTION, "");
    let mut hand = fixture(PROJECTION, "");
    let rows = [(1, by(SIZED, &[], &[])), (0, by(K::Normalize("ig"), &[UintTy::U32], &[0]))];
    assert_eq!(b, bytes(&mut hand, &rows, &[]));
}

#[test]
fn closure_fn_goal_is_not_certifiable() {
    let mut fx = fixture_with(CLOSURE, "", recording());
    let mut em = Emitter::new_in(alloc());
    match certify(&mut fx, &mut em, 0) {
        Err(NotCertifiable::Unsupported { rule: Rule::Builtin(builtin::Rule::FnTrait(_)), .. }) => {}
        r => panic!("want unsupported FnTrait, got {r:?}"),
    }
    assert_eq!(em.count().not_certifiable[0], 1);
    assert_eq!(em.count().goals, 0);
}

#[test]
fn answers_equal_with_and_without_derivations() {
    for text in CERTIFIABLE.iter().chain([CLOSURE, CHAIN_MOVED].iter()) {
        let mut plain = fixture(text, "");
        let mut rec = fixture_with(text, "", recording());
        for i in 0..plain.case.goals.len() {
            let (a, _) = plain.solve(i);
            let (b, _) = rec.solve(i);
            assert_eq!(a, b, "{} goal {i}", plain.case.name);
        }
    }
}

#[test]
fn count_line() {
    let mut em = Emitter::new_in(alloc());
    let mut rows = 0u32;
    for text in CERTIFIABLE.iter().chain([CLOSURE].iter()) {
        let mut fx = fixture_with(text, "", recording());
        if let Ok(b) = certify(&mut fx, &mut em, 0) {
            rows += View::read(&b).unwrap_or_else(|e| panic!("{e}")).rows();
        }
    }
    let c = em.count();
    assert_eq!(c.goals, 10);
    assert_eq!(c.not_certifiable.iter().sum::<u64>(), 1);
    assert_eq!(u64::from(rows), c.steps.iter().sum::<u64>());
    println!("blaze-cert emit: {c}");
}
