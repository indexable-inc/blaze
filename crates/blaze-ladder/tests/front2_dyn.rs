//! `dyn` and `Fn(A) -> B` in front2's type builder (front2/types.rs, dyn_ty.rs), and dyn
//! compatibility handed to the checker (E0038 from `blaze_typeck::dyn_compat`). Each fn's head
//! is made by the store; its first input is the type under test. Needs the installed toolchain
//! under `$HOME/.blaze/v2/toolchain`, as tests/front2_main.rs does.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{HeadError, Store, Want};
use blaze_ladder::front2::keys::checker_of;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::front2::types::Why;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
trait Tr { type X; fn f(&self); }
trait Two { fn g(&self); }
trait NotDyn { const C: u8; }
fn auto_order(_: &(dyn Tr<X = u8> + Send + Sync)) {}
fn auto_again(_: &(dyn Sync + Tr<X = u8> + Send + Send)) {}
fn binding(_: &dyn Tr<X = u16>) {}
fn fn_in_dyn(_: &dyn Fn(u8) -> u16) {}
fn two_principals(_: &(dyn Tr<X = u8> + Two)) {}
fn bound_twice(_: &dyn Tr<X = u8, X = u16>) {}
fn fn_bound<F: Fn(u8) -> u16>(_: F) {}
fn not_dyn_user() { let _x: Option<&dyn NotDyn> = None; }
fn type_error() -> u8 { true }
fn fn_ptr(_: fn(u8) -> u16) {}
fn fn_ptr_unit(_: unsafe extern \"C\" fn(u8)) {}
fn apit<A: Tr>(_: A, _: impl Two, _: &(impl Tr + ?Sized), _: impl Into<impl Two>) {}
fn rpit() -> impl Two { loop {} }
";

#[test]
fn dyn_types_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/dyns/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "dyns", dir: "/dyns/", root: "/dyns/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let sized_key = checker_of(front.index.lang("sized").expect("core's `Sized` is a lang item"));
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let mut head = |name: &'static str| {
        let k = lib(name);
        let made = store.ensure(&names, &mut front.keys, Want::Fn(k));
        println!("{name}: {made:?}");
        made.map(|()| store.fns.get(&checker_of(k)).expect("a made head").inputs.first().copied())
    };

    // Auto traits: order and repeats do not change the type (the Reference, "Trait objects").
    let a = head("auto_order").expect("auto_order's head");
    let b = head("auto_again").expect("auto_again's head");
    assert_eq!(a, b, "`dyn Tr<X = u8> + Send + Sync` and `dyn Sync + Tr<X = u8> + Send + Send` are one type");
    // A binding is part of the type.
    let c = head("binding").expect("binding's head");
    assert_ne!(a, c, "`X = u8` and `X = u16` are two types");
    // `Fn(A) -> B` inside `dyn`: `Fn<(A,)>` with the `Output = B` binding.
    head("fn_in_dyn").expect("`dyn Fn(u8) -> u16` is built");
    // Two non-auto traits: refused (E0225).
    assert!(matches!(head("two_principals"), Err(HeadError::Item(_, Why::TwoPrincipals(..)))), "two non-auto traits in one `dyn` are refused");
    // One associated type bound twice: refused (E0719).
    assert!(head("bound_twice").is_err(), "`X` bound twice is refused");
    // `Fn(A) -> B` in an ordinary bound: still refused, by its reason (printed).
    let fb = head("fn_bound");
    println!("fn_bound (ordinary `F: Fn(u8) -> u16`): {fb:?}");
    // A fn pointer type: its inputs then its output (`()` when none is written), its ABI and
    // safety in the header (core's `impl<Ret, T> .. for fn(T) -> Ret` headers need it).
    let p = head("fn_ptr").expect("`fn(u8) -> u16` is built").expect("an input");
    let u = head("fn_ptr_unit").expect("`unsafe extern \"C\" fn(u8)` is built").expect("an input");
    // (`impl Trait` parameters, read below.)
    head("apit").expect("`impl Trait` parameters are built");
    // In return position it is an opaque type (the Reference, "Impl trait", "Abstract return
    // types"): built; its callers are proved in tests/front2_bounds.rs.
    head("rpit").expect("a return-position `impl Trait` is built as an opaque type");
    let tys = store.tys.view();
    match (tys.node(p), tys.node(u)) {
        (Ok(blaze_typeck::PNode::FnPtr(lp, hp)), Ok(blaze_typeck::PNode::FnPtr(lu, hu))) => {
            let (lp, lu) = (tys.list(lp).expect("a list"), tys.list(lu).expect("a list"));
            println!("fn_ptr: {:?} {hp:?}; fn_ptr_unit: {:?} {hu:?}", lp.iter().map(|t| tys.node(*t)).collect::<Vec<_>>(), lu.iter().map(|t| tys.node(*t)).collect::<Vec<_>>());
            assert_eq!((lp.len(), lu.len()), (2, 2), "one input and the output each");
            assert_eq!((hp.abi, hp.safety), (blaze_typeck::Abi::Rust, blaze_typeck::Safety::Safe));
            assert_eq!((hu.abi, hu.safety), (blaze_typeck::Abi::C, blaze_typeck::Safety::Unsafe));
            assert!(matches!(tys.node(lu[1]), Ok(blaze_typeck::PNode::Tuple(l)) if l.len == 0), "no written output is `()`");
        }
        other => panic!("fn pointer types: {other:?}"),
    }

    // `impl Trait` in a parameter type (the Reference, "Impl trait", "Anonymous type
    // parameters"): one more type parameter per occurrence after the declared ones, in order of
    // appearance (an outer one before those in its bounds' arguments), each with its bounds and
    // `Sized` unless `?Sized` is written. `apit<A: Tr>(A, impl Two, &(impl Tr + ?Sized),
    // impl Into<impl Two>)`: A = 0, then 1, 2, 3 (`Into<..>`), 4 (the `Two` inside it).
    let ak = checker_of(lib("apit"));
    {
        let f = store.fns.get(&ak).expect("apit's head");
        let tys = store.tys.view();
        let params: Vec<_> = f.inputs.iter().map(|t| tys.node(*t)).collect();
        println!("apit: own {} inputs {params:?}", f.own);
        assert_eq!(f.own, 5, "one declared and four anonymous type parameters");
        assert!(matches!(tys.node(f.inputs[1]), Ok(blaze_typeck::PNode::Param(1))), "`impl Two` is parameter 1");
        assert!(matches!(tys.node(f.inputs[2]), Ok(blaze_typeck::PNode::Ref(_, t)) if matches!(tys.node(t), Ok(blaze_typeck::PNode::Param(2)))), "`&(impl Tr + ?Sized)` is a reference to parameter 2");
        assert!(matches!(tys.node(f.inputs[3]), Ok(blaze_typeck::PNode::Param(3))), "`impl Into<impl Two>` is parameter 3");
        let p = store.preds.get(&ak).expect("apit's predicates");
        let sized = sized_key;
        let on = |i: u32| p.clauses.iter().filter(|c| matches!(c, blaze_typeck::PClause::Trait { args, .. } if tys.list(*args).ok().and_then(|l| l.first().copied()).is_some_and(|s| matches!(tys.node(s), Ok(blaze_typeck::PNode::Param(j)) if j == i)))).filter_map(|c| match c { blaze_typeck::PClause::Trait { trait_, .. } => Some(*trait_), _ => None }).collect::<Vec<_>>();
        println!("apit clauses by parameter: {:?}", (0..5).map(on).collect::<Vec<_>>());
        assert_eq!(on(1).len(), 2, "`impl Two`: `Two` and `Sized`");
        assert!(on(1).contains(&sized), "`impl Two` is `Sized`");
        // The sized hierarchy (RFC 3729; the pinned core's marker.rs: `#[lang = "meta_sized"]
        // pub trait MetaSized: PointeeSized`, `pub trait Sized: MetaSized`): `?Sized` relaxes
        // `Sized` to `MetaSized`, not to nothing (front2_bounds' `relaxed_meta`). Judge-changed
        // 2026-11-16 by the checks integrator (was "`Tr` only") after the MetaSized piece landed.
        let meta = checker_of(front.index.lang("meta_sized").expect("core's `MetaSized` is a lang item"));
        assert!(!on(2).contains(&sized) && on(2).contains(&meta) && on(2).len() == 2, "`impl Tr + ?Sized`: `Tr` and `MetaSized`, no `Sized`");
        assert_eq!(on(4).len(), 2, "the `impl Two` inside `Into<..>`: `Two` and `Sized`");
    }

    // A trait that is not dyn compatible (an associated const) in `dyn`: the checker's E0038.
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let user_fn = lib("not_dyn_user");
    let mut buckets = None;
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, user_fn).unwrap_or_else(|e| panic!("not_dyn_user lowers: {e}"));
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("spelling");
    let scope = body.scope(&spell);
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    match check_body(&mut records, &body, &scope) {
        Err(CheckFail::Diags { diags, .. }) => {
            assert!(diags.iter().any(|(c, _, _)|*c == blaze_typeck::DiagCode::DynIncompatible), "E0038 for `dyn NotDyn`, got {diags:?}");
        }
        other => panic!("not_dyn_user: E0038 expected, got {:?}", other.map(|_| "checked clean")),
    }
    drop(scope);
    drop(spell);
    drop(view);

    // A body with a type error is never a checked body (the planted bug: ignoring `diags`).
    let bad = lower_fn(&names, prepared, records.keys, records.store, lib("type_error")).unwrap_or_else(|e| panic!("type_error lowers: {e}"));
    let view = bad.lowered.view();
    let spell = Spelling::of(&view).expect("spelling");
    let scope = bad.scope(&spell);
    let got = check_body(&mut records, &bad, &scope);
    assert!(matches!(got, Err(CheckFail::Diags { .. })), "`fn type_error() -> u8 {{ true }}` must not count as checked, got {:?}", got.map(|_| "checked clean"));
}

const RECEIVERS: &str = "extern crate alloc;
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::sync::Arc;
use core::pin::Pin;
fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
trait Recv {
    fn by_ref(&self);
    fn by_mut(&mut self);
    fn by_value(self);
    fn typed_ref(self: &Self);
    fn typed_mut(self: &mut Self);
    fn typed_value(self: Self);
    fn boxed(self: Box<Self>);
    fn rc(self: Rc<Self>);
    fn arc(self: Arc<Self>);
    fn pin_mut(self: Pin<&mut Self>);
    fn pin_ref(self: Pin<&Self>);
    fn pin_box(self: Pin<Box<Self>>);
    fn pin_arc(self: core::pin::Pin<alloc::sync::Arc<Self>>);
}
struct MyBox<T: ?Sized>(*const T);
impl<T: ?Sized> core::ops::Deref for MyBox<T> {
    type Target = T;
    fn deref(&self) -> &T { unsafe { &*self.0 } }
}
trait Mine { fn f(self: MyBox<Self>); }
trait MineSized { fn f(self: MyBox<Self>) where Self: Sized; fn g(&self); }
fn use_recv() { let _x: Option<&dyn Recv> = None; }
fn use_future() { let _x: Option<&dyn core::future::Future<Output = u8>> = None; }
fn use_mine_sized() { let _x: Option<&dyn MineSized> = None; }
fn use_mine() { let _x: Option<&dyn Mine> = None; }
fn f(x: core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = u8> + Send>>) {}
";

/// A typed receiver is read from its type as written and resolved (front2/receiver.rs
/// `classify`, through dyn_ty.rs `member`). The Reference, "Traits", "Dyn compatibility":
/// "Dispatchable functions must: ... Have a receiver with one of the following types: `&Self`
/// (i.e. `&self`), `&mut Self` (i.e `&mut self`), `Box<Self>`, `Rc<Self>`, `Arc<Self>`,
/// `Pin<P>` where `P` is one of the types above"; "Explicitly non-dispatchable functions
/// require: Have a `where Self: Sized` bound (receiver type of `Self` (i.e. `self`) implies
/// this)".
///
/// CLEAN: `dyn Recv` (every listed form, written as `self: T` too), `dyn Future` (core's
/// `poll(self: Pin<&mut Self>, ..)`), `dyn MineSized` (`self: MyBox<Self>` with `where Self:
/// Sized`) and the head `f(Pin<Box<dyn Future<Output = u8> + Send>>)`. (A call of `f` with
/// `Box::pin(async { 1 })` needs the unsizing coercion `Pin<Box<async block>>` to
/// `Pin<Box<dyn Future>>` at an argument, the checker's, not this rule: not here.) REFUSED: `dyn Mine` (`self: MyBox<Self>`, a user type without
/// `DispatchFromDyn`): E0038. CONTROL (the cause removed): the same `use_recv` over a store whose
/// pointer types were never looked up (`Pointers::NOT_LOOKED_UP`) is the checker's
/// `DynUndecided`, not a clean body.
///
/// Prints, last: "receivers: 4 clean, `dyn Mine` refused (E0038), undecided without the pointer types".
#[test]
fn typed_receivers_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/rcvs/src/main.rs".to_string(), RECEIVERS.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "rcvs", dir: "/rcvs/", root: "/rcvs/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: RECEIVERS }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let pointers = blaze_ladder::front2::receiver::Pointers::find(&front.index, front.resolver);
    println!("pointers: {pointers:?}");
    // A library key's `Debug` prints only its crate: prove `Box`, `Rc` and `Arc` are three items.
    {
        use blaze_ladder::front2::receiver::Pointer;
        let (Pointer::At(b), Pointer::At(r), Pointer::At(a)) = (pointers.boxed, pointers.rc, pointers.arc) else { panic!("alloc's Box, Rc and Arc are all found: {pointers:?}") };
        assert!(b != r && b != a && r != a, "`Box`, `Rc` and `Arc` are three different items");
    }
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    store.pointers = pointers;
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    // (body, clean expected)
    let cases: [(&'static str, bool); 5] = [
        ("use_recv", true),
        ("use_future", true),
        ("use_mine_sized", true),
        ("f", true),
        ("use_mine", false),
    ];
    let mut wrong = Vec::new();
    let mut clean = 0usize;
    for (name, want_clean) in cases {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(lib(name))) {
            wrong.push(format!("{name}: no head: {e:?}"));
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, lib(name)) {
            Ok(b) => b,
            Err(e) => {
                wrong.push(format!("{name}: does not lower: {e}"));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let got = check_body(&mut records, &body, &scope);
        match (want_clean, got) {
            (true, Ok(_)) => clean += 1,
            (false, Err(CheckFail::Diags { diags, .. })) if diags.iter().any(|(c, _, _)| *c == blaze_typeck::DiagCode::DynIncompatible) => {}
            (true, other) => wrong.push(format!("{name}: clean expected, got {:?}", other.map(|_| "checked clean"))),
            (false, other) => wrong.push(format!("{name}: E0038 expected (`self: MyBox<Self>` is no dispatchable receiver), got {:?}", other.map(|_| "checked clean"))),
        }
    }
    for w in &wrong {
        println!("{w}");
    }
    assert!(wrong.is_empty(), "{} of {} receiver cases wrong (printed above)", wrong.len(), cases.len());

    // The control: the cause removed. A store that never looked up `Box`, `Rc`, `Arc` and `Pin`
    // reads no typed receiver, so `dyn Recv` is the checker's named gap again.
    let mut bare = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut bare_buckets = None;
    let k = lib("use_recv");
    bare.ensure(&names, &mut front.keys, Want::Fn(k)).unwrap_or_else(|e| panic!("use_recv's head over the bare store: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut bare, k).unwrap_or_else(|e| panic!("use_recv lowers over the bare store: {e}"));
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("spelling");
    let scope = body.scope(&spell);
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut bare, buckets: &mut bare_buckets, order: BuildOrder::AsAsked, consts: None };
    let got = format!("{:?}", check_body(&mut records, &body, &scope).map(|_| "checked clean"));
    assert!(got.contains("DynUndecided"), "without the pointer types `dyn Recv` must stay undecided, got {got}");
    println!("receivers: {clean} clean, `dyn Mine` refused (E0038), undecided without the pointer types");
}

mod supertrait_args {
    //! End-to-end tests for `dyn Trait` vtable construction when a trait has supertraits with
    //! concrete or compound type arguments (`Buffer: AsRef<[u8]> + AsMut<[u8]>`,
    //! `Sub<T>: Super<Option<T>>`, `Sub<T>: Super<[T]>`, and transitive chains).

    use blaze_ladder::crates::{Inputs, Toolchain};
    use blaze_ladder::front2::keys::checker_of;
    use blaze_ladder::front2::program::Front2Program;
    use blaze_ladder::front2::program::lazy::LazyProgram;
    use blaze_ladder::program_texts::ProgramTexts;
    use blaze_ladder::refusal::SourceFile;
    use blaze_ladder::run::{FrontChoice, NewFront, build_survey, run_with};
    use blaze_ladder::seam::FrontEnd;
    use blaze_scope::names::{self, Written};
    use blaze_surface::{Namespace, PathKind};

    const NEW: NewFront<'static> = NewFront {
        package: "ladder",
        edition: "2024",
        shelf: None,
        overflow: blaze_build::OverflowChecks::Off,
    };

    const HEAD: &str = "#![feature(no_core, lang_items, rustc_attrs)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> LegacyReceiver for &mut T {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u16 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for usize {}
impl Copy for i32 {}
impl<T: PointeeSized> Copy for &T {}

#[lang = \"unsize\"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a mut U> for &'a mut T {}

#[lang = \"dispatch_from_dyn\"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}
impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a mut U> for &'a mut T {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_: usize, _: usize) -> ! {
    loop {}
}
";

    const BUFFER_PROGRAM: &str = "
pub trait AsRef<T: PointeeSized>: PointeeSized {
    fn as_ref(&self) -> &T;
}

pub trait AsMut<T: PointeeSized>: PointeeSized {
    fn as_mut(&mut self) -> &mut T;
}

pub trait Buffer: AsRef<[u8]> + AsMut<[u8]> {
    fn set_first(&mut self, b: u8);
    fn tag(&self) -> u8;
}

struct SmallBuf {
    data: [u8; 4],
}

impl AsRef<[u8]> for SmallBuf {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

impl AsMut<[u8]> for SmallBuf {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl Buffer for SmallBuf {
    fn set_first(&mut self, b: u8) {
        self.data[0] = b;
    }
    fn tag(&self) -> u8 {
        10
    }
}

fn read_as_ref(r: &dyn AsRef<[u8]>) -> u8 {
    let s = r.as_ref();
    s[0] + s[1] + s[2] + s[3]
}

fn bump_second_as_mut(m: &mut dyn AsMut<[u8]>, delta: u8) {
    let s = m.as_mut();
    s[1] = s[1] + delta;
}

fn exercise(buf: &mut dyn Buffer) -> u8 {
    buf.set_first(5);
    buf.as_mut()[2] = 7;
    bump_second_as_mut(buf, 3);
    let direct = buf.as_ref()[3];
    let sum = read_as_ref(buf);
    sum + direct + buf.tag()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let mut b = SmallBuf { data: [1, 2, 3, 4] };
    exercise(&mut b) as i32
}
";

    const COMPOUND_PARAM_PROGRAM: &str = "
enum MyOpt<T> {
    None,
    Some(T),
}

trait Wrap<U> {
    fn get_wrap(&self) -> U;
}

trait SubOpt<T>: Wrap<MyOpt<T>> {
    fn own_val(&self) -> T;
}

trait SliceView<U: PointeeSized> {
    fn view(&self) -> &U;
}

trait SubSlice<T>: SliceView<[T]> {
    fn slice_tag(&self) -> u32;
}

struct Holder32(u32);
struct Holder64(u64);

impl Wrap<MyOpt<u32>> for Holder32 {
    fn get_wrap(&self) -> MyOpt<u32> {
        MyOpt::Some(self.0 + 1)
    }
}

impl SubOpt<u32> for Holder32 {
    fn own_val(&self) -> u32 {
        self.0
    }
}

impl Wrap<MyOpt<u64>> for Holder64 {
    fn get_wrap(&self) -> MyOpt<u64> {
        MyOpt::Some(self.0 + 2)
    }
}

impl SubOpt<u64> for Holder64 {
    fn own_val(&self) -> u64 {
        self.0
    }
}

struct Bytes([u8; 3]);
struct Words([u16; 2]);

impl SliceView<[u8]> for Bytes {
    fn view(&self) -> &[u8] {
        &self.0
    }
}

impl SubSlice<u8> for Bytes {
    fn slice_tag(&self) -> u32 {
        1
    }
}

impl SliceView<[u16]> for Words {
    fn view(&self) -> &[u16] {
        &self.0
    }
}

impl SubSlice<u16> for Words {
    fn slice_tag(&self) -> u32 {
        2
    }
}

fn call_opt32(x: &dyn SubOpt<u32>) -> u32 {
    let w = match x.get_wrap() {
        MyOpt::Some(v) => v,
        MyOpt::None => 0,
    };
    w + x.own_val()
}

fn call_opt64(x: &dyn SubOpt<u64>) -> u64 {
    let w = match x.get_wrap() {
        MyOpt::Some(v) => v,
        MyOpt::None => 0,
    };
    w + x.own_val()
}

fn call_slice_u8(x: &dyn SubSlice<u8>) -> u32 {
    let s = x.view();
    (s[0] as u32) + (s[2] as u32) + x.slice_tag()
}

fn call_slice_u16(x: &dyn SubSlice<u16>) -> u32 {
    let s = x.view();
    (s[0] as u32) + (s[1] as u32) + x.slice_tag()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let h32 = Holder32(10);
    let h64 = Holder64(20);
    let bs = Bytes([3, 9, 4]);
    let ws = Words([5, 6]);
    let total = call_opt32(&h32) + (call_opt64(&h64) as u32) + call_slice_u8(&bs) + call_slice_u16(&ws);
    total as i32
}
";

    const TRANSITIVE_PROGRAM: &str = "
static NUM: u32 = 19;

trait C<Y: PointeeSized> {
    fn c_get(&self) -> &Y;
}

trait B<X: PointeeSized>: C<X> {
    fn b_get(&self) -> &X;
}

trait A: B<[u8]> {
    fn a_tag(&self) -> u32;
}

trait RefSuper<Z> {
    fn read_ref(&self) -> Z;
}

trait MidRef<X: 'static>: RefSuper<&'static X> {
    fn mid_tag(&self) -> u32;
}

trait Top: A + MidRef<u32> {
    fn top_tag(&self) -> u32;
}

struct Item {
    bytes: [u8; 2],
}

impl C<[u8]> for Item {
    fn c_get(&self) -> &[u8] {
        &self.bytes
    }
}

impl B<[u8]> for Item {
    fn b_get(&self) -> &[u8] {
        &self.bytes
    }
}

impl A for Item {
    fn a_tag(&self) -> u32 {
        3
    }
}

impl RefSuper<&'static u32> for Item {
    fn read_ref(&self) -> &'static u32 {
        &NUM
    }
}

impl MidRef<u32> for Item {
    fn mid_tag(&self) -> u32 {
        7
    }
}

impl Top for Item {
    fn top_tag(&self) -> u32 {
        11
    }
}

fn via_c(c: &dyn C<[u8]>) -> u32 {
    c.c_get()[0] as u32
}

fn via_mid(m: &dyn MidRef<u32>) -> u32 {
    *m.read_ref() + m.mid_tag()
}

fn via_ref_super(r: &dyn RefSuper<&'static u32>) -> u32 {
    *r.read_ref()
}

fn run_top(t: &dyn Top) -> u32 {
    let from_c_direct = t.c_get()[1] as u32;
    let from_b_direct = t.b_get()[0] as u32;
    let from_a = t.a_tag();
    let from_c_up = via_c(t);
    let from_mid_up = via_mid(t);
    let from_ref_up = via_ref_super(t);
    let from_top = t.top_tag();
    from_c_direct + from_b_direct + from_a + from_c_up + from_mid_up + from_ref_up + from_top
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let it = Item { bytes: [4, 6] };
    run_top(&it) as i32
}
";

    fn survey_clean(tail: &str, label: &str) {
        let alloc = blaze_own::testing::heap().alloc();
        let text = [HEAD, tail].concat();
        let files = [SourceFile { path: "src/main.rs", text: &text }];
        let tc = Toolchain::none(alloc);
        let texts = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, blaze_build::Panic::Unwind)
            .unwrap_or_else(|e| panic!("{label}: texts: {e}"));
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("{label}: inputs: {e}"));
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{label}: resolve: {e}"));
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame");
        let mut left_out = Default::default();
        let mut program = Front2Program::survey(&names, &frame, &files, &mut left_out, alloc)
            .unwrap_or_else(|e| panic!("{label}: survey: {e}"));
        assert!(left_out.is_empty(), "{label}: front2 left out {left_out:?}");
        let mut refused = Default::default();
        let counts = build_survey(&mut program, &mut refused, blaze_build::OverflowChecks::Off, alloc)
            .unwrap_or_else(|e| panic!("{label}: build_survey: {e}"));
        assert!(refused.is_empty(), "{label}: refused {refused:?} ({}/{})", counts.built, counts.tried);
        assert!(counts.built > 0, "{label}: expected built instances");
    }

    fn run(tail: &str) -> Result<u8, String> {
        let text = [HEAD, tail].concat();
        let files = [SourceFile { path: "src/main.rs", text: &text }];
        run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}"))
    }

    #[test]
    fn buffer_as_ref_as_mut_u8_slice_calls_and_upcasts() {
        survey_clean(BUFFER_PROGRAM, "Buffer");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(run(BUFFER_PROGRAM), Ok(35), "Buffer calls and prefix + off-chain upcasts");
    }

    #[test]
    fn parameterized_compound_supertrait_args_multiple_instantiations() {
        survey_clean(COMPOUND_PARAM_PROGRAM, "Compound");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(run(COMPOUND_PARAM_PROGRAM), Ok(84), "SubOpt<u32/u64> and SubSlice<u8/u16>");
    }

    #[test]
    fn transitive_supertrait_concrete_and_compound_args() {
        survey_clean(TRANSITIVE_PROGRAM, "Transitive");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(run(TRANSITIVE_PROGRAM), Ok(73), "transitive concrete & compound supertraits + upcasts");
    }

    fn shape_for(arg_ty: &str) -> (blaze_ids::ContentId, blaze_ids::ContentId) {
        let alloc = blaze_own::testing::heap().alloc();
        let tail = format!(
            "
pub trait AsRef<T: PointeeSized>: PointeeSized {{
    fn as_ref(&self) -> &T;
}}
pub trait Sub: AsRef<{arg_ty}> {{
    fn f(&self);
}}
fn use_sub(_: &dyn Sub) {{}}
#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {{
    let _f: fn(&dyn Sub) = use_sub;
    0
}}
"
        );
        let text = [HEAD, &tail].concat();
        let files = [SourceFile { path: "src/main.rs", text: &text }];
        let tc = Toolchain::none(alloc);
        let texts = ProgramTexts::new(&files, "src/main.rs", NEW, &tc, blaze_build::Panic::Unwind).expect("texts");
        let user = texts.user();
        let inputs = Inputs::new(&user, &tc).expect("inputs");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, blaze_own::testing::bump()).expect("names");
        let key = |ns: Namespace, name: &'static str| {
            let found = names::path(&names, names.root(0), ns, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
                .expect("path");
            checker_of(found.whole(1).expect("whole").key)
        };
        let sub = key(Namespace::Types, "Sub");
        let use_sub = key(Namespace::Values, "use_sub");
        let main = key(Namespace::Values, "main");
        let wf = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("whole frame");
        let lf = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("lazy frame");
        let whole = Front2Program::new(&names, &wf, &files, alloc).expect("whole program");
        let mut lazy = LazyProgram::new(&names, &lf, &files, Some(main), alloc).expect("lazy program");
        lazy.check_one(use_sub).expect("check_one").expect("use_sub fn");
        let w_shape = whole.object_trait(sub).expect("whole Sub object_trait").shape;
        let l_shape = lazy.object_trait(sub).expect("lazy Sub object_trait").shape;
        (w_shape, l_shape)
    }

    #[test]
    fn object_table_shape_hash_covers_supertrait_argument_types() {
        let (w_u8, l_u8) = shape_for("[u8]");
        let (w_u16, l_u16) = shape_for("[u16]");
        assert_eq!(w_u8, l_u8, "whole and lazy object_trait shapes must agree for AsRef<[u8]>");
        assert_eq!(w_u16, l_u16, "whole and lazy object_trait shapes must agree for AsRef<[u16]>");
        assert_ne!(w_u8, w_u16, "changing supertrait argument [u8] -> [u16] must change ObjectTable::shape");
    }

    const AUTOREF_PROMOTED_DYN_PROGRAM: &str = "#![feature(no_core, lang_items, rustc_attrs, auto_traits)]
#![no_core]
#![no_main]
#![allow(internal_features)]

#[lang = \"pointee_sized\"]
trait PointeeSized {}

#[lang = \"meta_sized\"]
trait MetaSized: PointeeSized {}

#[lang = \"sized\"]
trait Sized: MetaSized {}

#[lang = \"freeze\"]
unsafe auto trait Freeze {}

#[lang = \"legacy_receiver\"]
trait LegacyReceiver: PointeeSized {}

impl<T: PointeeSized> LegacyReceiver for &T {}
impl<T: PointeeSized> LegacyReceiver for &mut T {}

#[lang = \"copy\"]
trait Copy {}

impl Copy for u8 {}
impl Copy for u32 {}
impl Copy for u64 {}
impl Copy for i32 {}
impl<T: PointeeSized> Copy for &T {}

#[lang = \"unsize\"]
trait Unsize<T: PointeeSized>: PointeeSized {}

#[lang = \"coerce_unsized\"]
trait CoerceUnsized<T: PointeeSized> {}

impl<'a, 'b: 'a, T: PointeeSized + Unsize<U>, U: PointeeSized> CoerceUnsized<&'a U> for &'b T {}

#[lang = \"dispatch_from_dyn\"]
trait DispatchFromDyn<T> {}

impl<'a, T: PointeeSized + Unsize<U>, U: PointeeSized> DispatchFromDyn<&'a U> for &'a T {}

#[lang = \"panic_bounds_check\"]
fn panic_bounds_check(_: usize, _: usize) -> ! {
    loop {}
}

enum Fork {
    Frontier,
    Homestead,
    Cancun,
    Prague,
}

impl Fork {
    fn score(&self) -> u8 {
        match self {
            Fork::Frontier => 11,
            Fork::Homestead => 22,
            Fork::Cancun => 33,
            Fork::Prague => 44,
        }
    }

    fn as_static_ref(&self) -> &Self {
        self
    }
}

trait Hardfork {
    fn score(&self) -> u8;
}

impl Hardfork for Fork {
    fn score(&self) -> u8 {
        Fork::score(self)
    }
}

// Mirrors `#[auto_impl(&, Box)]` on `alloy_hardforks::Hardfork`.
impl<'a, T: 'a + Hardfork + ?Sized> Hardfork for &'a T {
    fn score(&self) -> u8 {
        T::score(self)
    }
}

#[inline(never)]
fn make_promoted_ref() -> &'static Fork {
    // Implicit shared autoref on a unit enum variant (mirrors `EthereumHardfork::Frontier.boxed()`).
    Fork::Cancun.as_static_ref()
}

#[inline(never)]
fn clobber_stack(seed: u64) -> u64 {
    let a = [seed ^ 0xd8d8_d8d8_d8d8_d8d8u64; 32];
    a[0] ^ a[31]
}

#[inline(never)]
fn dispatch_dyn(h: &dyn Hardfork) -> u8 {
    h.score()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let r: &'static Fork = make_promoted_ref();
    let _noise = clobber_stack(0x1234_5678);
    // Concrete `Self` behind `dyn Hardfork` is `&'static Fork`, exercising
    // `<&Fork as Hardfork>::score` -> `Fork::score` after `make_promoted_ref`'s frame is gone.
    let via_dyn = dispatch_dyn(&r);
    // Also exercise an autoref on a promoted `Expr::Ref` (`&&Fork` via `Hardfork for &T`).
    let via_ref_autoref = (&Fork::Homestead).score();
    (via_dyn + via_ref_autoref) as i32
}
";

    /// Regression test for the `tempo-chainspec` SIGTRAP (`brk #0xff02`, bad enum discriminant
    /// `0xd8`): `EthereumHardfork::Variant.boxed()` autorefs a unit variant rvalue to `&'static
    /// Self` via `AdjustKind::Borrow { mutbl: Not, raw: false }` (no `Expr::Ref`). Without
    /// promoting shared autorefs, the returned `&'static Fork` pointed into the returning fn's
    /// dead stack frame, which subsequent calls overwrote before `<&Fork as Hardfork>::score`
    /// matched on the discriminant.
    #[test]
    fn autoref_promoted_enum_survives_stack_clobber_through_dyn_ref() {
        let files = [SourceFile { path: "src/main.rs", text: AUTOREF_PROMOTED_DYN_PROGRAM }];
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            run_with(&files, "src/main.rs", None, FrontChoice::New(NEW), blaze_own::testing::heap().shared()).map_err(|e| format!("{e}")),
            Ok(55),
            "promoted autoref `&'static Fork` (33) + `(&Fork::Homestead).score()` (22)"
        );
    }
}
