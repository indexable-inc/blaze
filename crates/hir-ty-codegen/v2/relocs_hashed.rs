//! Every vtable and function the relocation of a constant's or static's value emits is among
//! what code identity hashes of that value (rule 21). Each block of the identity port so far
//! was one shape of this bug: `relocate` emitted a vtable or a function for the value that the
//! identity walk ([`hir_ty_identity::hashed::value_edges`]) did not reach, so an edit to its
//! code kept a stale cached result. This checks the whole class over a corpus instead of one
//! shape a review.
//!
//! The relocation is read where the build reads it: a static's from `compiled_static`,
//! a constant's from `FunctionCx::allocation_blob` in a `build` as
//! `compiled_static` makes one. A value either side refuses has no code id, so nothing of it
//! can be stale: it is listed and skipped, and is checked once both take it.

#[path = "tests/db.rs"]
mod db;

use std::fmt::Write as _;

use base_db::Db;
use hir_def::{
    AssocItemId, CallableDefId, ConstId, HasModule, ModuleDefId,
    signatures::{ConstSignature, StaticSignature, TraitSignature},
};
use hir_expand::fixture;
use hir_ty::{
    ParamEnvAndCrate,
    db::HirDatabase,
    mir::devirt::{ObjectKey, object_key, principal_of},
    next_solver::{
        DbInterner, GenericArgs, ParamEnv, StoredGenericArgs, StoredTy, TyKind, TypingMode,
        infer::DbInternerInferExt as _,
    },
};
use hir_ty_identity::{
    hashed::{Hashed, ValueOf, value_edges},
    instance::erased,
};
use hir_ty_mir::mir::dispatch::Body;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};

use crate::{
    CodegenError, ConstBlob, Instance, InstanceDef,
    fragment::{DataRequest, FuncTarget, Recorder, RelocName, Requests},
};
use db::TestDB;

const MINICORE: &str = "sized, copy, clone, drop, size_of, fn, sync, fmt, unsize, coerce_unsized, \
                        dispatch_from_dyn, slice, str, index, range, manually_drop, non_null, \
                        pointee, option, transmute";

/// What the cases share: traits with impls, a `dyn` tail struct, a handler over `Fn`, and a
/// type with `Debug` and `Display`.
const PRELUDE: &str = r#"
use core::fmt::{Debug, Display, Formatter};
pub trait Tr { fn f(&self) -> u32; }
pub struct A(pub u32);
impl Tr for A { fn f(&self) -> u32 { self.0 } }
pub struct B(pub u32, pub u32);
impl Tr for B { fn f(&self) -> u32 { self.0 + self.1 } }
pub struct Holder(pub &'static (dyn Tr + Sync));
impl Tr for Holder { fn f(&self) -> u32 { self.0.f() + 1000 } }
pub struct Wrap<T: ?Sized>(pub u32, pub T);
fn log() -> u32 { 1 }
pub trait Handler { fn call(&self, r: u32) -> u32; }
impl<F: Fn(u32) -> u32> Handler for F { fn call(&self, r: u32) -> u32 { log() + self(r) } }
fn index(r: u32) -> u32 { r }
fn pick(_: u32) {}
pub struct MyT(pub u32);
impl Debug for MyT { fn fmt(&self, _: &mut Formatter<'_>) -> core::fmt::Result { Ok(()) } }
impl Display for MyT { fn fmt(&self, _: &mut Formatter<'_>) -> core::fmt::Result { pick(1); Ok(()) } }
fn keep(_: &(dyn Display + Sync)) {}
fn keep_str(_: &str) {}
"#;

/// The corpus: each case is a crate of its own after [`PRELUDE`], every constant and static of
/// it checked. Shapes from the identity port's rounds 9 to 12, the relocate lane's repros
/// (`/tmp/reloc-lane`: p1-p4, rv, rv2, rv4, rv5, rv6) and the joint review, written for
/// minicore: no `String`, `format!`, `MaybeUninit`, `as_ptr`, `split_at` or `ptr::metadata`
/// (a union, casts and `transmute` stand in). p2's `M`, a `DynMetadata` static, is left out:
/// under minicore its initializer does not type-check (`HasErrors`); `tuptail` holds one.
const CORPUS: &[(&str, &str)] = &[
    // Identity rounds 9-12.
    ("handler_table", r#"static ROUTES: &[(&str, &(dyn Handler + Sync))] = &[("/", &index)];"#),
    ("handler_at_tail", "static W: &Wrap<dyn Handler + Sync> = &Wrap(0, index);"),
    (
        "debug_and_tail",
        "pub static S: (&'static (dyn Debug + Sync), &'static Wrap<dyn Display + Sync>) = (&MyT(1), &Wrap(0, MyT(2)));",
    ),
    ("tail_only", "pub static S: &'static Wrap<dyn Display + Sync> = &Wrap(0, MyT(2));"),
    (
        "handler_inside_tail",
        "pub trait Tr2 {}\npub struct Inner(pub &'static (dyn Handler + Sync));\nimpl Tr2 for Inner {}\n\
         static W: &Wrap<dyn Tr2 + Sync> = &Wrap(0, Inner(&index));",
    ),
    (
        "debug_and_inside_tail",
        "pub trait Tr2 {}\npub struct Inner(pub &'static (dyn Display + Sync));\nimpl Tr2 for Inner {}\n\
         pub static S: (&'static (dyn Debug + Sync), &'static Wrap<dyn Tr2 + Sync>) = (&MyT(1), &Wrap(0, Inner(&MyT(2))));\n\
         pub static ONLY: &'static Wrap<dyn Tr2 + Sync> = &Wrap(0, Inner(&MyT(2)));",
    ),
    (
        "pre",
        "pub struct Pre<T: ?Sized>(pub &'static (dyn Display + Sync), pub T);\n\
         pub static S: &'static Pre<dyn Debug + Sync> = &Pre(&MyT(2), MyT(1));",
    ),
    (
        "dyn_behind_dyn",
        "pub struct W(&'static dyn Display);\n\
         impl Debug for W { fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result { Display::fmt(self.0, f) } }\n\
         const C: &dyn Debug = &W(&MyT(0));",
    ),
    (
        "clfn",
        "static W: &(dyn Fn() -> u32 + Sync) = &{ let h: &'static (dyn Handler + Sync) = &index; move || h.call(1) };",
    ),
    (
        "cldisp",
        "pub static S: &'static (dyn Fn() -> u32 + Sync) = &{ let h: &'static (dyn Display + Sync) = &MyT(2); move || { keep(h); 0 } };",
    ),
    (
        "raw_past_end",
        "#[repr(C)] pub struct Two(pub &'static u32, pub [&'static u32; 0]);\n\
         pub struct P(pub *const &'static (dyn Display + Sync), pub &'static (dyn Display + Sync));\n\
         unsafe impl Sync for P {}\nconst T: &Two = &Two(&1, []);\n\
         pub static S: P = P(&T.1 as *const [&'static u32; 0] as *const &'static (dyn Display + Sync), &MyT(1));",
    ),
    (
        "raw_runs_past",
        "#[repr(C)] pub struct Pair(pub u32, pub u32);\n\
         pub struct P(pub *const &'static (dyn Display + Sync), pub &'static (dyn Display + Sync));\n\
         unsafe impl Sync for P {}\nconst T: &Pair = &Pair(1, 2);\n\
         pub static S: P = P(&T.1 as *const u32 as *const &'static (dyn Display + Sync), &MyT(1));",
    ),
    (
        "raw_fits",
        "pub struct P(pub *const &'static (dyn Display + Sync));\nunsafe impl Sync for P {}\n\
         pub static S: P = P(&(&MyT(2) as &'static (dyn Display + Sync)) as *const &'static (dyn Display + Sync));",
    ),
    (
        "dead_temporary",
        "pub trait B2 { fn b(&self) -> u32; }\nimpl B2 for u16 { fn b(&self) -> u32 { 0 } }\n\
         const fn keep_b(_: &dyn B2) {}\n\
         const C: &dyn Tr = { let x = 1u16; let y: &dyn B2 = &x; keep_b(y); &A(2) };",
    ),
    // The relocate lane's repros.
    (
        "p1",
        "static AA: A = A(7);\n\
         pub struct Raw(*const dyn Tr);\nunsafe impl Sync for Raw {}\n\
         static RAW_STATIC: Raw = Raw(&AA as &dyn Tr as *const dyn Tr);\n\
         static RAW_CONST_MEM: Raw = Raw(&B(10, 20) as &dyn Tr as *const dyn Tr);\n\
         const RAW_C: *const dyn Tr = &A(9) as &dyn Tr;\n\
         pub trait Has { type Obj: ?Sized + 'static; }\npub struct H;\nimpl Has for H { type Obj = dyn Display + Sync; }\n\
         static ALIAS: &<H as Has>::Obj = &5u32;\n\
         pub struct W<U: Has>(&'static U::Obj);\n\
         static WW: W<H> = W(&MyT(8) as &(dyn Display + Sync));\n\
         const WC: W<H> = W(&MyT(9) as &(dyn Display + Sync));\n\
         pub struct RawRef(*const &'static dyn Tr);\nunsafe impl Sync for RawRef {}\n\
         static BEHIND_STATIC: RawRef = RawRef(&(&AA as &dyn Tr) as *const &dyn Tr);\n\
         static BEHIND_MEM: RawRef = RawRef(&(&B(3, 4) as &dyn Tr) as *const &dyn Tr);\n\
         fn eleven() -> u32 { 11 }\npub struct RawFn(*const fn() -> u32);\nunsafe impl Sync for RawFn {}\n\
         static FN_BEHIND: RawFn = RawFn(&(eleven as fn() -> u32) as *const fn() -> u32);",
    ),
    (
        "p2",
        "static AA: A = A(7);\n\
         pub struct Two(&'static u8, &'static &'static (dyn Tr + Sync));\n\
         const T: &(u8, &(dyn Tr + Sync)) = &(1u8, &A(21) as &(dyn Tr + Sync));\n\
         static TWO: Two = Two(&T.0, &T.1);\n\
         static TWO2: Two = { let t: &'static (u8, &'static (dyn Tr + Sync)) = &(2u8, &A(22) as &(dyn Tr + Sync)); Two(&t.0, &t.1) };\n\
         static WRAP: &Wrap<dyn Tr + Sync> = &Wrap(100, A(5));\n\
         pub struct RawSlice(*const [&'static dyn Tr]);\nunsafe impl Sync for RawSlice {}\n\
         static SLICE: RawSlice = RawSlice(&[&A(1) as &dyn Tr, &A(2)] as *const [&dyn Tr]);\n\
         pub struct Vt { name: &'static str }\npub struct SyncPtr(*const ());\nunsafe impl Sync for SyncPtr {}\n\
         static C1: SyncPtr = SyncPtr(&Vt { name: \"x\" } as *const Vt as *const ());\n\
         static W3: &Wrap<dyn Tr + Sync> = &Wrap(1, Holder(&A(2) as &(dyn Tr + Sync)));\n\
         #[repr(C, packed(2))] pub struct Pk<X: ?Sized>(u16, core::mem::ManuallyDrop<X>);\n\
         static PK: &Pk<dyn Tr + Sync> = &Pk(1, core::mem::ManuallyDrop::new(Holder(&A(9) as &(dyn Tr + Sync))));\n\
         static F: &(dyn Fn() -> usize + Sync) = &{ let s: &'static str = \"hi\"; move || { keep_str(s); 2 } };\n\
         pub trait Has { type Obj: ?Sized + 'static; }\npub struct H;\nimpl Has for H { type Obj = dyn Tr + Sync; }\n\
         pub struct Outer<U: Has>(&'static Wrap<U::Obj>);\n\
         static AT: Outer<H> = Outer(&Wrap(3, A(4)) as &Wrap<dyn Tr + Sync>);",
    ),
    (
        "p3",
        "static AA: A = A(7);\npub struct RawU8(*const u8);\nunsafe impl Sync for RawU8 {}\n\
         static CAST: RawU8 = RawU8(&(&AA as &dyn Tr) as *const &dyn Tr as *const u8);",
    ),
    (
        "p4",
        "pub struct Two(*const u8, &'static &'static (dyn Tr + Sync));\nunsafe impl Sync for Two {}\n\
         static S: Two = { let t: &'static &'static (dyn Tr + Sync) = &(&A(1) as &(dyn Tr + Sync)); Two(t as *const _ as *const u8, t) };",
    ),
    (
        "asptr",
        "pub struct P(pub *const &'static (dyn Tr + Sync));\nunsafe impl Sync for P {}\n\
         const ARR: &[&(dyn Tr + Sync)] = &[&A(1), &B(2, 3)];\n\
         static S: P = P(ARR as *const [&(dyn Tr + Sync)] as *const &'static (dyn Tr + Sync));",
    ),
    (
        "asptr2",
        "pub struct P(pub *const &'static (dyn Tr + Sync));\nunsafe impl Sync for P {}\n\
         const ARR: &[&(dyn Tr + Sync)] = &[&A(1), &A(2)];\n\
         static S: P = P(ARR as *const [&(dyn Tr + Sync)] as *const &'static (dyn Tr + Sync));",
    ),
    (
        "muninit",
        "#[derive(Clone, Copy)] pub struct Nothing;\n\
         pub union MaybeUninit<T: Copy> { uninit: Nothing, value: T }\n\
         unsafe impl<T: Copy> Sync for MaybeUninit<T> {}\n\
         static S: MaybeUninit<&'static (dyn Tr + Sync)> = MaybeUninit { value: &A(4) };",
    ),
    (
        "nested",
        "pub struct Outer<T: ?Sized>(pub u8, pub Wrap<T>);\n\
         static N: &Outer<dyn Tr + Sync> = &Outer(1, Wrap(2, Holder(&B(3, 4))));",
    ),
    (
        "num",
        "fn g() -> u32 { 7 }\n\
         static S: ([u64; 4], fn() -> u32) = ([0x0600_0000, 0x0600_0001, 0x0600_0002, 0x0600_0003], g);",
    ),
    (
        "order",
        "pub struct P(pub *const &'static u32);\nunsafe impl Sync for P {}\n\
         static S: (&'static [&'static u32], P) = { let a: &'static [&'static u32] = &[&1, &2, &3]; (a, P(&a[1] as *const &'static u32)) };",
    ),
    (
        "sltail",
        "static S: &Wrap<[&(dyn Tr + Sync)]> = &Wrap(1, [&A(5) as &(dyn Tr + Sync), &A(6)]);",
    ),
    (
        "stack",
        "#[repr(C)] pub struct Two(pub [u8; 2], pub [u8; 0]);\n\
         pub struct R(pub core::ops::Range<*const u8>, pub &'static [u8; 2]);\nunsafe impl Sync for R {}\n\
         static S: R = { let a: &'static Two = &Two([1, 2], []); let b: &'static [u8; 2] = &[3, 4]; \
             R(core::ops::Range { start: &a.0 as *const [u8; 2] as *const u8, end: &a.1 as *const [u8; 0] as *const u8 }, b) };",
    ),
    (
        "tupcl",
        "static W: &(dyn Fn() -> u32 + Sync) = &{ let t: (u8, &'static (dyn Tr + Sync)) = (1, &B(1, 2)); move || t.1.f() + t.0 as u32 };",
    ),
    ("c_plain", "static W: &(dyn Handler + Sync) = &index;"),
    (
        "enumniche",
        "pub struct S(pub Option<&'static (dyn Tr + Sync)>, pub Option<&'static (dyn Tr + Sync)>);\nstatic E: S = S(None, Some(&A(3)));",
    ),
    (
        "pknest",
        "pub struct Wrap8<X: ?Sized>(pub u8, pub X);\n\
         #[repr(C, packed(2))] pub struct Pk<X: ?Sized>(pub u16, pub core::mem::ManuallyDrop<Wrap8<X>>);\n\
         static PK: &Pk<dyn Tr + Sync> = &Pk(1, core::mem::ManuallyDrop::new(Wrap8(2, Holder(&A(9)))));",
    ),
    (
        "tuptail",
        "const fn mk() -> &'static (u8, dyn Tr + Sync) {\n\
             let p: &'static (u8, Holder) = &(1u8, Holder(&A(5)));\n\
             let m = unsafe { core::mem::transmute::<&(dyn Tr + Sync), (*const (), core::ptr::DynMetadata<dyn Tr + Sync>)>(&Holder(&A(0))).1 };\n\
             unsafe { core::mem::transmute::<(*const (), core::ptr::DynMetadata<dyn Tr + Sync>), &'static (u8, dyn Tr + Sync)>((p as *const (u8, Holder) as *const (), m)) }\n\
         }\n\
         static T: &(u8, dyn Tr + Sync) = mk();",
    ),
    (
        "emp",
        "#[repr(C)] pub struct Three(pub [u8; 3], pub [u8; 0]);\n\
         static S: (&[u8], &[u8], &[u8]) = { let t: &'static Three = &Three([1, 2, 3], []); \
             (&t.0, unsafe { core::mem::transmute::<(*const u8, usize), &'static [u8]>((&t.0[1] as *const u8, 0)) }, &t.1) };",
    ),
    (
        "pkpre",
        "pub struct Wrap2<X: ?Sized>(pub &'static u32, pub X);\n\
         #[repr(C, packed(2))] pub struct Pk<X: ?Sized>(pub u16, pub core::mem::ManuallyDrop<Wrap2<X>>);\n\
         static PK: &Pk<dyn Tr + Sync> = &Pk(1, core::mem::ManuallyDrop::new(Wrap2(&77, Holder(&A(9)))));",
    ),
    // The key review's (`/tmp/blaze-lanes/v2-xcheck-review.md`, C1 and C2): values whose bytes
    // an edit keeps and whose emitted code it changes.
    (
        "which_static",
        "pub static SA: u32 = 1;\npub static SB: u32 = 2;\n\
         pub static BOTH: (&u32, &u32) = (&SA, &SB);\n\
         pub const P: (&(&u32, &u32), &u32) = (&BOTH, &SA);",
    ),
    (
        "provenance",
        "pub static SX: u8 = 5;\npub union U { pub p: *const u8, pub n: usize }\n\
         pub const P: (&u8, U) = (&SX, U { p: &SX as *const u8 });",
    ),
    (
        "alignment",
        "pub const P: (&u8, &u8, &u8, &u8, &u8, &u8, *const u8) = \
         (&1u8, &2u8, &3u8, &4u8, &5u8, &6u8, &[0u8; 8] as *const [u8; 8] as *const u8);",
    ),
    // Words the code generator refuses (key review C5), each beside a vtable so identity walks
    // the value: a `&dyn`'s metadata, an id with no provenance (a `DynMetadata` one does not
    // type-check under minicore: `HasErrors`); a vtable
    // id in a function pointer; a vtable read as another trait's (made by a binding typed as
    // `Tr`'s, so the unsizing is to `Tr` whatever the transmute's arguments infer: spelled with
    // `&A(1)` as the argument, both sides refused it as `Display` not implemented); a vtable of
    // the same trait with other arguments (`Tr<u8>`'s read as `Tr<u16>`'s, the round-3 review's
    // C2: the trait's definition alone matches); an id, a number, in memory
    // only provenance reaches (a union of two non-empty fields says nothing of its bytes, so no
    // type reaches what its pointer points to). `Q` is one both take.
    (
        "refusals",
        "pub const DYN_NUMBER: (&(dyn Tr + Sync), &(dyn Tr + Sync)) = \
             (&A(1), unsafe { core::mem::transmute::<(&A, usize), &(dyn Tr + Sync)>((&A(2), 0x0600_0000)) });\n\
         pub const FN_NUMBER: (&(dyn Tr + Sync), fn() -> u32) = \
             (&A(1), unsafe { core::mem::transmute::<usize, fn() -> u32>(0x0600_0000) });\n\
         pub const WRONG_TRAIT: &(dyn Display + Sync) = { let t: &'static (dyn Tr + Sync) = &A(1); \
             unsafe { core::mem::transmute::<&(dyn Tr + Sync), &(dyn Display + Sync)>(t) } };\n\
         pub union W { pub p: *const usize, pub n: usize }\n\
         pub const ONLY_PROVENANCE: (&(dyn Tr + Sync), W) = (&A(1), W { p: &0x0600_0000usize });\n\
         pub trait G<T> { fn g(&self) -> u32; }\n\
         impl G<u8> for A { fn g(&self) -> u32 { 8 } }\n\
         impl G<u16> for A { fn g(&self) -> u32 { 16 } }\n\
         pub const WRONG_ARGS: &(dyn G<u16> + Sync) = { let t: &'static (dyn G<u8> + Sync) = &A(1); \
             unsafe { core::mem::transmute::<&(dyn G<u8> + Sync), &(dyn G<u16> + Sync)>(t) } };\n\
         pub const Q: &(dyn Tr + Sync) = &A(3);",
    ),
    // A `&dyn`'s parts as two raw pointers (the xcheck2 review's M1): the second holds the
    // vtable's id with provenance, which rustc keeps as a pointer to the vtable.
    (
        "parts",
        "pub struct Parts(pub *const (), pub *const ());\n\
         pub const P: Parts = unsafe { core::mem::transmute::<&(dyn Tr + Sync), Parts>(&A(1)) };",
    ),
    // A constant that is a copy of another, which const eval evaluates by copying its memory,
    // alignments included (the xcheck2 review's C4): its reader reads `Q`'s initializer, `P`.
    (
        "copy",
        "#[repr(align(64))] pub struct Big(pub u8);\n\
         pub const P: (&u8, &Big, &(dyn Tr + Sync)) = (&1u8, &Big(2), &A(3));\n\
         pub const Q: (&u8, &Big, &(dyn Tr + Sync)) = P;",
    ),
];

/// One of the corpus's values, by the name it is written with.
struct Item {
    name: String,
    of: ValueOf,
}

fn items(db: &TestDB, case: &str) -> Vec<Item> {
    let krate = fixture::test_crate(db);
    hir_def::test_items::declarations(db, krate)
        .into_iter()
        .filter_map(|it| match it {
            ModuleDefId::StaticId(id) => Some(Item {
                name: format!("{case}::{}", StaticSignature::of(db, id).name.as_str()),
                of: ValueOf::Static(id),
            }),
            ModuleDefId::ConstId(id) => {
                let name = ConstSignature::of(db, id).name.as_ref()?.as_str().to_owned();
                Some(Item { name: format!("{case}::{name}"), of: ValueOf::Const(id) })
            }
            _ => None,
        })
        .collect()
}

/// A constant's value relocated as the build relocates it: in a `build` like
/// `compiled_static`'s, through `allocation_blob`.
fn compiled_const(db: &Db, id: ConstId) -> crate::Result<(Requests, ConstBlob)> {
    let krate = id.krate(db);
    let checked = crate::crate_checks_overflow(db, krate).unwrap_or(false);
    let target = crate::target_of_crate(db, krate)?;
    let mut cg = Recorder::new(db, &**crate::isa_for(target)?, target, checked, None);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
    let mut value = None;
    crate::build(&mut cg, env, None, false, |fx| {
        let allocation = db
            .const_eval(id, GenericArgs::empty(), Some(fx.env))
            .map_err(|e| CodegenError::Frontend(format!("const eval: {e:?}")))?;
        value = Some(fx.allocation_blob(allocation, crate::ConstOf::Item(id.into()), false)?);
        Ok(())
    })?;
    Ok((cg.finish().0, value.expect("`build` ran the closure")))
}

/// What the check found over the corpus.
#[derive(Default)]
struct Found {
    misses: Vec<String>,
    refused: Vec<String>,
    /// Vtables and functions found among the hashed inputs, by item.
    checked: Vec<(String, usize, usize)>,
}

fn check_item(db: &TestDB, item: &Item, found: &mut Found) {
    let owned;
    let compiled = match item.of {
        ValueOf::Static(id) => crate::compiled_static(db, id)
            .as_ref()
            .map(|it| (&it.requests, &it.blob))
            .map_err(ToString::to_string),
        ValueOf::Const(id) => match compiled_const(db, id) {
            Ok(it) => {
                owned = it;
                Ok((&owned.0, &owned.1))
            }
            Err(e) => Err(e.to_string()),
        },
    };
    let hashed = value_edges(db, item.of);
    let (requests, blob, hashed) = match (compiled, hashed) {
        (Ok((requests, blob)), Ok(hashed)) => (requests, blob, hashed),
        (compiled, hashed) => {
            let mut why = String::new();
            if let Err(e) = &compiled {
                let _ = write!(why, "codegen refuses it: {e}");
            }
            if let Err(e) = &hashed {
                let _ = write!(
                    why,
                    "{}identity refuses it: {e}",
                    if why.is_empty() { "" } else { "; " }
                );
            }
            found.refused.push(format!("{}: {why}", item.name));
            return;
        }
    };
    let krate = match item.of {
        ValueOf::Static(id) => id.krate(db),
        ValueOf::Const(id) => id.krate(db),
    };
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::Codegen);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };

    // The vtables identity hashes, by codegen's key: the concrete type and the object type.
    let vtables: Vec<(StoredTy, Option<ObjectKey>)> = hashed
        .iter()
        .filter_map(|it| match it {
            Hashed::Vtable { concrete, dyn_ty } => {
                let key = principal_of(interner, dyn_ty.as_ref(), concrete.as_ref())
                    .map(|it| object_key(&infcx, ParamEnv::empty(), it));
                Some((concrete.clone(), key))
            }
            _ => None,
        })
        .collect();
    let calls: Vec<(hir_def::FunctionId, StoredGenericArgs)> = hashed
        .iter()
        .filter_map(|it| match it {
            Hashed::Call { func, args } => Some((*func, args.clone())),
            _ => None,
        })
        .collect();
    let closures: Vec<Body> = hashed
        .iter()
        .filter_map(|it| match it {
            Hashed::Closure { body, .. } => Some(*body),
            _ => None,
        })
        .collect();
    let plain = |args: &StoredGenericArgs| erased(interner, args.as_ref()).store();
    // `f` with `args` names `g` with `g_args` as the code generator dispatches a pointer to it.
    let names = |g: hir_def::FunctionId, g_args: &StoredGenericArgs| {
        calls.iter().any(|(f, args)| {
            (*f == g && plain(args) == plain(g_args))
                || crate::impl_method(db, env, *f, args.as_ref()).is_ok_and(
                    |(callee, callee_args)| {
                        callee == either::Either::Left(g)
                            && erased(interner, callee_args).store() == plain(g_args)
                    },
                )
        })
    };
    let fn_item = |ty: hir_ty::next_solver::Ty<'_>| match ty.kind() {
        TyKind::FnDef(def, args) => match def.0 {
            CallableDefId::FunctionId(f) => names(f, &args.skip_binder().store()),
            // A constructor is hashed by its key and arguments, as identity's `callable` does.
            CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => true,
        },
        TyKind::Closure(id, _) => {
            closures.contains(&Body::Closure(blaze_query::db::AsId::as_id(&id.0)))
        }
        _ => false,
    };

    let target = hir_ty::display::DisplayTarget::from_crate(db, krate);
    let spell = |ty: &StoredTy| {
        use hir_ty::display::HirDisplay as _;
        ty.as_ref().display_truncated(db, Some(120), target).to_string()
    };
    let object = |key: &Option<ObjectKey>| match key {
        Some(key) => format!(
            "`dyn {}{:?}`",
            TraitSignature::of(db, key.trait_).name.as_str(),
            key.args.as_ref().as_slice()
        ),
        None => "a `dyn` with no principal".to_owned(),
    };
    let (mut vtables_seen, mut funcs_seen) = (0, 0);
    // The value's own blob and every blob it points to; a vtable's own blob holds its
    // methods and supertraits' vtables, which the vtable's key covers.
    let mut blobs = vec![("the value".to_owned(), blob)];
    let mut walked: Vec<u32> = Vec::new();
    while let Some((at, blob)) = blobs.pop() {
        for &(offset, data, _) in &blob.relocs {
            match &requests.datas[data.as_u32() as usize] {
                DataRequest::Blob { blob, .. } => {
                    if !walked.contains(&data.as_u32()) {
                        walked.push(data.as_u32());
                        blobs.push((
                            format!("data {} (pointed to at {offset} of {at})", data.as_u32()),
                            blob,
                        ));
                    }
                }
                DataRequest::Vtable { ty, trait_ref, .. } => {
                    vtables_seen += 1;
                    let key = trait_ref.as_ref().map(|(trait_, args)| ObjectKey {
                        trait_: *trait_,
                        args: GenericArgs::new_from_slice(&args.as_ref().as_slice()[1..]).store(),
                    });
                    if !vtables.iter().any(|(concrete, theirs)| concrete == ty && *theirs == key) {
                        found.misses.push(format!(
                            "{}: at {offset} of {at}, the vtable of `{}` for {} is emitted but not hashed \
                             (identity hashes {})",
                            item.name,
                            spell(ty),
                            object(&key),
                            vtables
                                .iter()
                                .map(|(ty, key)| format!("`{}` for {}", spell(ty), object(key)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                }
                _ => {}
            }
        }
        for &(offset, func) in &blob.fn_relocs {
            funcs_seen += 1;
            let target = &requests.funcs[func.as_u32() as usize].target;
            let hashed = match target {
                FuncTarget::Instance(Instance { def, args, .. }, _) => match *def {
                    InstanceDef::Fn(g) => names(g, args),
                    InstanceDef::Closure(id) => closures.contains(&Body::Closure(id)),
                    InstanceDef::ReifyShim { .. }
                    | InstanceDef::FnPointerShim
                    | InstanceDef::CallOnceShim => fn_item(args.as_ref().type_at(0)),
                    _ => false,
                },
                _ => false,
            };
            if !hashed {
                let target = match target {
                    FuncTarget::Instance(instance, _) => {
                        format!("{:?} {:?}", instance.def, instance.args.as_ref())
                    }
                    FuncTarget::Symbol(it) | FuncTarget::Path(it) | FuncTarget::Asm(it) => {
                        it.clone()
                    }
                };
                found.misses.push(format!(
                    "{}: at {offset} of {at}, the function {target} is emitted but not hashed (identity calls {:?}, closures {:?})",
                    item.name,
                    calls.iter().map(|(f, args)| (f, args.as_ref())).collect::<Vec<_>>(),
                    closures
                ));
            }
        }
    }
    found.checked.push((item.name.clone(), vtables_seen, funcs_seen));
}

/// Every vtable and function the relocation of each constant and static in the corpus emits
/// is among the inputs identity hashes for its code id; a miss names the item, the offset
/// and the vtable's type and trait, or the function.
#[test]
fn every_vtable_and_function_a_value_relocates_to_is_hashed() {
    let mut found = Found::default();
    for (case, text) in CORPUS {
        let db = fixture::db(&format!(
            "//- minicore: {MINICORE}\n//- /src/lib.rs crate:kernel\n{PRELUDE}\n{text}\n"
        ))
        .into_db();
        let items = items(&db, case);
        assert!(!items.is_empty(), "{case}: no constant or static");
        blaze_query::db::attach(&db, || {
            for item in &items {
                check_item(&db, item, &mut found);
            }
        });
    }
    for (name, vtables, funcs) in &found.checked {
        eprintln!("checked {name}: {vtables} vtables, {funcs} functions");
    }
    for refused in &found.refused {
        eprintln!("skipped {refused}");
    }
    // A fixture that doesn't type-check is refused by both sides and would check nothing.
    let broken: Vec<_> = found.refused.iter().filter(|it| it.contains("HasErrors")).collect();
    assert!(
        broken.is_empty(),
        "corpus cases that don't type-check:\n{}",
        broken.iter().map(|it| it.as_str()).collect::<Vec<_>>().join("\n")
    );
    assert!(
        found.misses.is_empty(),
        "{} emitted but not hashed:\n{}",
        found.misses.len(),
        found.misses.join("\n")
    );
    let vtables: usize = found.checked.iter().map(|it| it.1).sum();
    let funcs: usize = found.checked.iter().map(|it| it.2).sum();
    assert!(vtables > 0 && funcs > 0, "the corpus checked {vtables} vtables and {funcs} functions");
}

// ---------------------------------------------------------------------------------------------
// The cross-check: a value's code id changes exactly when what the build emits for it does.

/// The relocate lane's own fixtures (`v2::tests`), each a whole crate: its constants and
/// statics are checked as the corpus's are.
const FIXTURES: &[(&str, &str)] = &[
    ("pointers_to_dyn", super::tests::POINTERS_TO_DYN),
    ("constant_memory", super::tests::CONSTANT_MEMORY),
];

/// An edit of a case's source: every `from` in it becomes `to`. An edit whose `from` a case
/// does not hold is not made to it. In `to`, [`ADDRESS`] is the first word of the case's `P`
/// before the edit.
struct Edit {
    name: &'static str,
    from: &'static str,
    to: &'static str,
    /// What alone the edit changes of the case's `P`'s value, checked before the sides are
    /// compared: an edit that keeps the bytes tests that what else the build emits by is hashed.
    only: Option<Only>,
}

const fn edit(name: &'static str, from: &'static str, to: &'static str) -> Edit {
    Edit { name, from, to, only: None }
}

const fn edit_only(name: &'static str, from: &'static str, to: &'static str, only: Only) -> Edit {
    Edit { name, from, to, only: Some(only) }
}

/// Stands for the first word of the case's `P`'s value before the edit, in an edit's `to`.
const ADDRESS: &str = "{address}";

/// What of a value an edit alone changes.
#[derive(Clone, Copy, Debug)]
enum Only {
    /// Which words have provenance.
    Provenance,
    /// What a piece of memory is aligned to.
    Alignment,
    /// Which static a range of memory is.
    Static,
}

/// An item put first in every case, which moves every other one: no code changes.
const MOVED: &str = "pub struct MovedEveryItem;\n";

/// What every case holds besides its own items, which [`EDITS`]' last edit changes: no value
/// reaches it.
const UNRELATED: &str = "\npub fn unrelated_to_any_value() -> u32 { 1 }\n";

/// Edits made to every case that holds what they change: the impls behind the vtables (the
/// code their slots run, and what that calls), and code no value reaches.
const EDITS: &[Edit] = &[
    edit(
        "impl Tr for A",
        "impl Tr for A { fn f(&self) -> u32 { self.0 } }",
        "impl Tr for A { fn f(&self) -> u32 { self.0 + 1 } }",
    ),
    edit(
        "impl Tr for B",
        "fn f(&self) -> u32 { self.0 + self.1 }",
        "fn f(&self) -> u32 { self.0 * self.1 }",
    ),
    edit(
        "impl Tr for C",
        "impl Tr for C { fn f(&self) -> u32 { self.0 } }",
        "impl Tr for C { fn f(&self) -> u32 { self.0 + 3 } }",
    ),
    edit(
        "impl Tr for G",
        "impl Tr for G { fn f(&self) -> u32 { self.0 } }",
        "impl Tr for G { fn f(&self) -> u32 { self.0 + 7 } }",
    ),
    edit("impl Tr for Holder", "self.0.f() + 1000", "self.0.f() + 2000"),
    edit("impl Display for MyT", "pick(1); Ok(())", "pick(2); Ok(())"),
    edit("impl Handler for Fn", "log() + self(r)", "log() * self(r)"),
    edit("fn log, called by a vtable's method", "fn log() -> u32 { 1 }", "fn log() -> u32 { 2 }"),
    edit(
        "an unrelated function",
        "pub fn unrelated_to_any_value() -> u32 { 1 }",
        "pub fn unrelated_to_any_value() -> u32 { 2 }",
    ),
];

/// Each case's own edits: the bytes behind a pointer, the impl or type behind a vtable, a
/// slice tail's length, an enum's variant, a function a pointer runs, a closure's body.
const CASE_EDITS: &[(&str, Edit)] = &[
    ("handler_table", edit("the pointee", r#"("/", &index)"#, r#"("/x", &index)"#)),
    ("handler_at_tail", edit("the pointee", "Wrap(0, index)", "Wrap(1, index)")),
    ("debug_and_tail", edit("the pointee", "&MyT(1)", "&MyT(3)")),
    ("tail_only", edit("the pointee", "Wrap(0, MyT(2))", "Wrap(0, MyT(4))")),
    (
        "handler_inside_tail",
        edit("the pointee", "&Wrap(0, Inner(&index))", "&Wrap(5, Inner(&index))"),
    ),
    ("debug_and_inside_tail", edit("the pointee", "&MyT(1)", "&MyT(9)")),
    ("pre", edit("the pointee", "&Pre(&MyT(2), MyT(1))", "&Pre(&MyT(3), MyT(1))")),
    ("dyn_behind_dyn", edit("the pointee", "&W(&MyT(0))", "&W(&MyT(1))")),
    ("clfn", edit("the closure's body", "h.call(1)", "h.call(2)")),
    ("cldisp", edit("the pointee", "&MyT(2)", "&MyT(3)")),
    ("raw_past_end", edit("the pointee", "&MyT(1)", "&MyT(2)")),
    ("raw_past_end", edit("the memory a raw pointer is one past", "Two(&1, [])", "Two(&2, [])")),
    ("raw_runs_past", edit("the memory a raw pointer runs past", "Pair(1, 2)", "Pair(1, 3)")),
    ("raw_fits", edit("the pointee", "&MyT(2)", "&MyT(5)")),
    ("dead_temporary", edit("the pointee", "&A(2)", "&A(3)")),
    ("dead_temporary", edit("a dead temporary", "let x = 1u16", "let x = 2u16")),
    ("p1", edit("a static pointed to", "static AA: A = A(7);", "static AA: A = A(70);")),
    ("p1", edit("the pointee", "&B(10, 20)", "&B(11, 20)")),
    ("p1", edit("the pointee", "&5u32", "&6u32")),
    (
        "p1",
        edit("a function a pointer runs", "fn eleven() -> u32 { 11 }", "fn eleven() -> u32 { 12 }"),
    ),
    ("p2", edit("the pointee", "&A(21)", "&A(23)")),
    ("p2", edit("the pointee", "Wrap(100, A(5))", "Wrap(101, A(5))")),
    ("p2", edit("the pointee", r#"name: "x""#, r#"name: "y""#)),
    ("p2", edit("the closure's body", "keep_str(s); 2", "keep_str(s); 3")),
    ("p3", edit("a static pointed to", "static AA: A = A(7);", "static AA: A = A(8);")),
    ("p4", edit("the pointee", "&A(1)", "&A(2)")),
    ("asptr", edit("the pointee", "&B(2, 3)", "&B(2, 4)")),
    ("asptr", edit("the type behind a vtable", "&B(2, 3)]", "&A(3)]")),
    ("asptr2", edit("the pointee", "&A(2)]", "&A(5)]")),
    ("muninit", edit("the pointee", "&A(4)", "&A(5)")),
    ("muninit", edit("the type behind a vtable", "&A(4)", "&B(4, 0)")),
    ("nested", edit("the pointee", "Holder(&B(3, 4))", "Holder(&B(3, 5))")),
    ("num", edit("a number equal to an id", "0x0600_0003", "0x0600_0004")),
    ("num", edit("a function a pointer runs", "fn g() -> u32 { 7 }", "fn g() -> u32 { 8 }")),
    ("order", edit("the pointee", "&[&1, &2, &3]", "&[&1, &5, &3]")),
    ("sltail", edit("the pointee", "&A(5)", "&A(7)")),
    ("sltail", edit("the slice tail's length", "&A(6)]", "&A(6), &A(8)]")),
    ("sltail", edit("the type behind a vtable", "&A(6)]", "&B(6, 1)]")),
    ("stack", edit("the pointee", "[1, 2]", "[1, 9]")),
    ("tupcl", edit("the pointee", "&B(1, 2)", "&B(1, 3)")),
    ("tupcl", edit("the closure's body", "t.0 as u32 };", "t.0 as u32 + 1 };")),
    ("enumniche", edit("the pointee", "Some(&A(3))", "Some(&A(4))")),
    ("enumniche", edit("the enum's variant", "S(None,", "S(Some(&A(1)),")),
    ("pknest", edit("the pointee", "Holder(&A(9))", "Holder(&A(8))")),
    ("pknest", edit("the packed prefix", "Wrap8(2,", "Wrap8(3,")),
    ("tuptail", edit("the pointee", "(1u8, Holder(&A(5)))", "(2u8, Holder(&A(5)))")),
    ("emp", edit("the pointee", "Three([1, 2, 3], [])", "Three([1, 2, 4], [])")),
    ("emp", edit("the slice's length", "(&t.0[1] as *const u8, 0)", "(&t.0[1] as *const u8, 1)")),
    ("pkpre", edit("the pointee", "Wrap2(&77,", "Wrap2(&78,")),
    ("pointers_to_dyn", edit("the pointee", "&A(9)", "&A(10)")),
    ("pointers_to_dyn", edit("the pointee", "&B(8)", "&B(9)")),
    ("pointers_to_dyn", edit("the tail's concrete value", "&Wrap(100, D(5))", "&Wrap(100, D(6))")),
    (
        "pointers_to_dyn",
        edit(
            "the raw slice's length",
            "&[&E(1) as &dyn Tr]",
            "&[&E(1) as &dyn Tr, &E(2) as &dyn Tr]",
        ),
    ),
    (
        "pointers_to_dyn",
        edit("a function a pointer runs", "fn eleven() -> u32 { 11 }", "fn eleven() -> u32 { 12 }"),
    ),
    ("constant_memory", edit("the pointee", "Vt { name: &5 }", "Vt { name: &6 }")),
    (
        "constant_memory",
        edit("a static pointed to", "End([1, 2, 3, 4], ())", "End([1, 2, 3, 5], ())"),
    ),
    ("constant_memory", edit("the pointee", "&K(4)", "&K(5)")),
    (
        "constant_memory",
        edit("the slice tail's length", "&G(6) as &dyn Tr]", "&G(6) as &dyn Tr, &K(7) as &dyn Tr]"),
    ),
    (
        "constant_memory",
        edit("the slice tail's length", "&Prefix(&9, [1, 2])", "&Prefix(&9, [1, 2, 3])"),
    ),
    (
        "constant_memory",
        edit("a function a pointer runs", "fn seven() -> u32 { 7 }", "fn seven() -> u32 { 70 }"),
    ),
    (
        "constant_memory",
        edit("a function a pointer runs", "fn eight() -> u32 { 8 }", "fn eight() -> u32 { 80 }"),
    ),
    ("constant_memory", edit("the pointee", "End8([2; 8], ())", "End8([3; 8], ())")),
    ("constant_memory", edit("the pointee", "E3([1, 2, 3], [])", "E3([1, 2, 4], [])")),
    ("constant_memory", edit("the pointee", "&[&1, &2, &3]", "&[&1, &4, &3]")),
    (
        "which_static",
        edit_only("which static is pointed to", "(&BOTH, &SA)", "(&BOTH, &SB)", Only::Static),
    ),
    (
        "which_static",
        edit("a static pointed to", "pub static SA: u32 = 1;", "pub static SA: u32 = 3;"),
    ),
    (
        "provenance",
        edit_only(
            "a pointer made an equal number",
            "U { p: &SX as *const u8 }",
            "U { n: {address} }",
            Only::Provenance,
        ),
    ),
    ("provenance", edit("a static pointed to", "pub static SX: u8 = 5;", "pub static SX: u8 = 6;")),
    (
        "alignment",
        // Six pieces first put the eighth at an address a multiple of 8 either way: only its
        // alignment changes, which its reader, reading `P`'s initializer, sees as `P` has it.
        edit_only(
            "a piece's alignment",
            "&[0u8; 8] as *const [u8; 8]",
            "&0u64 as *const u64",
            Only::Alignment,
        ),
    ),
    ("alignment", edit("the pointee", "&[0u8; 8] as", "&[1u8; 8] as")),
    ("refusals", edit("the pointee", "&A(3)", "&A(4)")),
    ("parts", edit("the pointee", "&A(1)", "&A(2)")),
    ("parts", edit("the type behind a vtable", "Parts>(&A(1))", "Parts>(&B(1, 2))")),
    ("copy", edit("the pointee", "&Big(2)", "&Big(3)")),
    ("copy", edit("the type behind a vtable", "&A(3)", "&B(3, 4)")),
];

/// The constants and statics a case's source declares, by name, each a static or not, with its
/// type as written and its initializer, read from the text: a reader is made for each, and the
/// database's own list is held to it.
fn declared(source: &str) -> Vec<(String, bool, String, String)> {
    let mut found = Vec::new();
    for (keyword, is_static) in [("static ", true), ("const ", false)] {
        for (at, _) in source.match_indices(keyword) {
            if at > 0
                && !source[..at]
                    .ends_with(|c: char| c.is_whitespace() || c == ';' || c == '{' || c == '}')
            {
                continue;
            }
            let rest = &source[at + keyword.len()..];
            let name: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if name.is_empty() || name == "fn" || name == "mut" {
                continue;
            }
            if let Some(ty) = rest[name.len()..].trim_start().strip_prefix(':') {
                found.push((name, is_static, written_type(ty), initializer(ty)));
            }
        }
    }
    found.sort();
    found
}

/// Where the type a declaration's text starts with ends: at its `=` or `;`, outside brackets.
fn type_end(text: &str) -> usize {
    let mut depth = 0i32;
    let mut last = ' ';
    for (at, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            '>' if last == '-' => {}
            ')' | ']' | '}' | '>' => depth -= 1,
            '=' | ';' if depth == 0 => return at,
            _ => {}
        }
        last = c;
    }
    text.len()
}

/// The initializer after a declaration's type, from its `=` to the `;` that ends it, outside
/// brackets and string literals; empty for one with none.
fn initializer(text: &str) -> String {
    let end = type_end(text);
    let Some(rest) = text[end..].strip_prefix('=') else { return String::new() };
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    for (at, c) in rest.char_indices() {
        match (in_string, c) {
            (true, _) if escaped => escaped = false,
            (true, '\\') => escaped = true,
            (true, '"') => in_string = false,
            (true, _) => {}
            (false, '"') => in_string = true,
            (false, '(' | '[' | '{') => depth += 1,
            (false, ')' | ']' | '}') => depth -= 1,
            (false, ';') if depth == 0 => return rest[..at].trim().to_owned(),
            _ => {}
        }
    }
    rest.trim().to_owned()
}

/// The type a declaration's text starts with, up to its `=` (outside brackets), each reference
/// in it given `'static`, which an associated constant's type spells.
fn written_type(text: &str) -> String {
    let ty = text[..type_end(text)].trim();
    let mut out = String::with_capacity(ty.len() + 16);
    for (at, c) in ty.char_indices() {
        out.push(c);
        if c == '&' && !ty[at + 1..].trim_start().starts_with('\'') {
            out.push_str("'static ");
        }
    }
    out
}

/// The reader of an item: a function whose code id is the item's value's, among the rest.
fn reader(name: &str) -> String {
    format!("__xcheck_read_{name}")
}

/// What a reader reads, an associated constant of `()` whose value is the item's (for a static,
/// a reference to it).
const READ: &str = "__XCHECK_";

/// A case's crate: `head` (the fixture's lines), its `source`, and a reader for each of its
/// constants and statics. A reader reads the item through a trait's constant, by projection:
/// a constant read by name is in the reader's own code by its interface, which is its value,
/// so it would hide what [`hir_ty_identity`]'s `Facts::allocation` misses (key review C3). A
/// projection's own code names only the trait's constant, and its value is hashed by the
/// facts alone. A constant's reader's constant is its initializer, evaluated as the item's is
/// (placed alike, which an edit of an alignment needs); one that is a copy of another constant
/// (`copy::Q`) reads the copy. A static's is a reference to it.
fn crate_text(head: &str, source: &str) -> String {
    let mut text = format!("{head}{source}\n");
    for (name, is_static, ty, init) in declared(source) {
        let (ty, value) = match is_static {
            true => (format!("&'static {ty}"), format!("&{name}")),
            false => {
                assert!(!init.is_empty(), "the constant `{name}` has no initializer to read");
                (ty, init)
            }
        };
        let _ = writeln!(
            text,
            "pub trait XcheckGet{name} {{ const {READ}{name}: {ty}; }}\n\
             impl XcheckGet{name} for () {{ const {READ}{name}: {ty} = {value}; }}\n\
             pub fn {}() {{ let _x = <() as XcheckGet{name}>::{READ}{name}; }}",
            reader(&name)
        );
    }
    text
}

/// Each reader's constant, by the name of the item it reads.
fn read_consts(db: &TestDB) -> Vec<(String, ConstId)> {
    let krate = fixture::test_crate(db);
    hir_def::test_items::impls(db, krate)
        .into_iter()
        .flat_map(|imp| imp.impl_items(db).items.to_vec())
        .filter_map(|(_, item)| match item {
            AssocItemId::ConstId(id) => {
                let name = ConstSignature::of(db, id)
                    .name
                    .as_ref()?
                    .as_str()
                    .strip_prefix(READ)?
                    .to_owned();
                Some((name, id))
            }
            _ => None,
        })
        .collect()
}

/// What the code generator reads of an evaluated value, field by field, for an edit's
/// precondition to compare.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Value {
    bytes: Vec<u8>,
    /// Each piece's address, bytes, alignment and the words in it with provenance.
    pieces: Vec<(usize, Vec<u8>, Option<usize>, Vec<usize>)>,
    value_pointers: Vec<usize>,
    /// Each static's memory: where it is, its size, which static it is.
    statics: Vec<(usize, usize, String)>,
    /// Whether const eval's table of ids (functions and vtables) has entries: only such a
    /// value does identity walk (`Facts::allocation`), so only in such a value does it refuse
    /// what the code generator refuses.
    ids: bool,
}

impl Value {
    /// The value with what `only` names of it left out.
    fn without(&self, only: Only) -> Value {
        let mut it = self.clone();
        match only {
            Only::Provenance => {
                it.value_pointers.clear();
                it.pieces.iter_mut().for_each(|piece| piece.3.clear());
            }
            Only::Static => it.statics.iter_mut().for_each(|memory| memory.2.clear()),
            Only::Alignment => it.pieces.iter_mut().for_each(|piece| piece.2 = None),
        }
        it
    }
}

fn value(db: &TestDB, id: ConstId) -> Result<Value, String> {
    let value =
        db.const_eval(id, GenericArgs::empty(), None).map_err(|e| format!("const eval: {e:?}"))?;
    let map = &value.memory_map;
    let ids = matches!(map, hir_ty::MemoryMap::Complex(complex) if complex.vtable.tys().len() > 0);
    let (pieces, statics) = match map {
        hir_ty::MemoryMap::Complex(complex) => (
            (complex.memory.iter())
                .map(|(&at, bytes)| {
                    (
                        at,
                        bytes.to_vec(),
                        map.align_at(at),
                        map.pointers_in(at..at + bytes.len()).to_vec(),
                    )
                })
                .collect(),
            (complex.statics.iter())
                .map(|it| {
                    (
                        it.start,
                        it.size,
                        blaze_items::key::path(db, blaze_items::ItemId::Static(it.id)).to_string(),
                    )
                })
                .collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    Ok(Value {
        bytes: value.memory.to_vec(),
        pieces,
        value_pointers: map.value_pointers().to_vec(),
        statics,
        ids,
    })
}

/// What the build emits for a value and for everything its relocations reach, in an order its
/// bytes and relocations fix, spelled with nothing of the database's ids, so two databases of
/// two sources compare: each blob's bytes and relocations, each vtable by its type and trait
/// with its slots, each function's code and what it relocates to, each static's value. Where a
/// panic's location is is left out: the build looks it up when it stitches.
struct Emitted<'a> {
    db: &'a Db,
    target: hir_ty::display::DisplayTarget,
    out: String,
    /// The functions, vtables and statics spelled, each once.
    seen: std::collections::BTreeSet<String>,
    /// The functions and statics still to spell, in the order they were met.
    work: std::collections::VecDeque<Pending>,
}

enum Pending {
    Func(Instance, hir_ty::traits::StoredParamEnvAndCrate, String),
    Static(hir_def::StaticId, String),
}

/// The most functions and statics one value's emitted code may reach (rule 18).
const REACHED: usize = 4000;

impl<'a> Emitted<'a> {
    fn ty(&self, ty: hir_ty::next_solver::Ty<'_>) -> String {
        use hir_ty::display::HirDisplay as _;
        ty.display_truncated(self.db, None, self.target).to_string()
    }

    fn args(&self, args: GenericArgs<'_>) -> String {
        let spelled: Vec<String> = (args.as_slice().iter())
            .map(|arg| match arg.ty() {
                Some(ty) => self.ty(ty),
                None => format!("{arg:?}"),
            })
            .collect();
        spelled.join(", ")
    }

    fn func_name(&self, instance: &Instance) -> String {
        let def = match instance.def {
            InstanceDef::Fn(_) => "fn".to_owned(),
            InstanceDef::Closure(_) => "closure".to_owned(),
            InstanceDef::Coroutine(_) => "coroutine".to_owned(),
            InstanceDef::CoroutineByMove(_) => "coroutine by move".to_owned(),
            InstanceDef::DropGlue => "drop glue".to_owned(),
            InstanceDef::CallOnceShim => "call-once shim".to_owned(),
            InstanceDef::VtableShim(f) => {
                format!(
                    "vtable shim of {}",
                    blaze_items::key::path(self.db, blaze_items::ItemId::Function(f))
                )
            }
            InstanceDef::FnPointerShim => "fn pointer shim".to_owned(),
            InstanceDef::ReifyShim { receiver } => format!("reify shim (receiver {receiver})"),
        };
        format!(
            "{def} {} [{}] checked {}",
            crate::backtrace_name(self.db, instance),
            self.args(instance.args.as_ref()),
            instance.checked
        )
    }

    fn target(&mut self, target: &FuncTarget) -> String {
        match target {
            FuncTarget::Instance(instance, env) => {
                let name = self.func_name(instance);
                if self.seen.insert(format!("fn {name}")) {
                    self.work.push_back(Pending::Func(instance.clone(), env.clone(), name.clone()));
                }
                name
            }
            FuncTarget::Symbol(it) => format!("symbol {it}"),
            FuncTarget::Path(it) => format!("path {it}"),
            FuncTarget::Asm(it) => format!("asm {it}"),
        }
    }

    fn data(&mut self, requests: &Requests, data: u32, depth: usize) {
        match &requests.datas[data as usize] {
            DataRequest::Blob { blob, identity } => {
                let _ = writeln!(self.out, "blob {identity:?}");
                self.blob(requests, blob, depth + 1);
            }
            DataRequest::Vtable { ty, trait_ref, blob } => {
                let trait_ = match trait_ref {
                    Some((trait_, args)) => {
                        format!(
                            "{} [{}]",
                            TraitSignature::of(self.db, *trait_).name.as_str(),
                            self.args(args.as_ref())
                        )
                    }
                    None => "no principal".to_owned(),
                };
                let name = format!("vtable of {} for {trait_}", self.ty(ty.as_ref()));
                let _ = writeln!(self.out, "{name}");
                if self.seen.insert(name) {
                    self.blob(requests, blob, depth + 1);
                }
            }
            DataRequest::Static(id) => {
                let name = blaze_items::key::path(self.db, blaze_items::ItemId::Static(*id));
                let _ = writeln!(self.out, "static {name}");
                if self.seen.insert(format!("static {name}")) {
                    self.work.push_back(Pending::Static(*id, name));
                }
            }
            DataRequest::Location(_) => self.out.push_str("a location\n"),
            DataRequest::Declaration(f) => {
                let _ = writeln!(
                    self.out,
                    "the declaration of {}",
                    blaze_items::key::path(self.db, blaze_items::ItemId::Function(*f))
                );
            }
            DataRequest::FilePath(_) => self.out.push_str("a file's path\n"),
            DataRequest::Text(parts) => {
                for part in parts {
                    match part {
                        crate::fragment::TextPart::Text(it) => {
                            let _ = write!(self.out, "{it:?} ");
                        }
                        crate::fragment::TextPart::FilePath(_) => self.out.push_str("<file> "),
                    }
                }
                self.out.push('\n');
            }
        }
    }

    fn blob(&mut self, requests: &Requests, blob: &ConstBlob, depth: usize) {
        assert!(depth < 256, "a blob more than 256 pointers deep");
        let _ = writeln!(
            self.out,
            "{:indent$}bytes {:02x?} align {}",
            "",
            blob.bytes,
            blob.align,
            indent = depth * 2
        );
        for &(offset, data, add) in &blob.relocs {
            let _ = write!(self.out, "{:indent$}@{offset}+{add} -> ", "", indent = depth * 2);
            self.data(requests, data.as_u32(), depth);
        }
        for &(offset, func) in &blob.fn_relocs {
            let name = self.target(&requests.funcs[func.as_u32() as usize].target);
            let _ = writeln!(self.out, "{:indent$}@{offset} -> {name}", "", indent = depth * 2);
        }
        for (offset, ty) in &blob.type_ids {
            let _ = writeln!(
                self.out,
                "{:indent$}@{offset} type id of {}",
                "",
                self.ty(ty.as_ref()),
                indent = depth * 2
            );
        }
    }

    /// Every function and static met, and what they reach, until none is left.
    fn drain(&mut self) -> Result<(), String> {
        let mut spelled = 0;
        while let Some(pending) = self.work.pop_front() {
            spelled += 1;
            if spelled > REACHED {
                return Err(format!(
                    "its emitted code reaches more than {REACHED} functions and statics"
                ));
            }
            match pending {
                Pending::Func(instance, env, name) => {
                    let _ = writeln!(self.out, "== fn {name}");
                    match crate::compiled_instance(self.db, instance, env) {
                        Ok(compiled) => {
                            let code = &compiled.code;
                            let _ =
                                writeln!(self.out, "code {:02x?} align {}", code.bytes, code.align);
                            for reloc in code.relocs.iter() {
                                let _ = write!(
                                    self.out,
                                    "@{} {:?}+{} -> ",
                                    reloc.offset, reloc.kind, reloc.addend
                                );
                                match reloc.name {
                                    RelocName::Func(index) => {
                                        let name = self.target(
                                            &compiled.requests.funcs[index as usize].target,
                                        );
                                        let _ = writeln!(self.out, "{name}");
                                    }
                                    RelocName::Data(index) => {
                                        self.data(&compiled.requests, index, 0)
                                    }
                                    other => {
                                        let _ = writeln!(self.out, "{other:?}");
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(self.out, "refused: {e}");
                        }
                    }
                }
                Pending::Static(id, name) => {
                    let _ = writeln!(self.out, "== static {name}");
                    match crate::compiled_static(self.db, id) {
                        Ok(compiled) => {
                            let _ = writeln!(self.out, "writable {}", compiled.writable);
                            self.blob(&compiled.requests, &compiled.blob, 0);
                        }
                        Err(e) => {
                            let _ = writeln!(self.out, "refused: {e}");
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// What the build emits for `item`'s value, as its reader's constant `read` holds it, and
/// everything it reaches, or why it doesn't: a static the build refuses is refused here.
fn emitted(db: &TestDB, item: &Item, read: ConstId) -> Result<String, String> {
    let krate = match item.of {
        ValueOf::Static(id) => id.krate(db),
        ValueOf::Const(id) => id.krate(db),
    };
    let mut out = Emitted {
        db,
        target: hir_ty::display::DisplayTarget::from_crate(db, krate),
        out: String::new(),
        seen: Default::default(),
        work: Default::default(),
    };
    if let ValueOf::Static(id) = item.of {
        crate::compiled_static(db, id).as_ref().map_err(ToString::to_string)?;
    }
    let (requests, blob) = compiled_const(db, read).map_err(|e| e.to_string())?;
    out.blob(&requests, &blob, 0);
    out.drain()?;
    Ok(out.out)
}

/// The code id of `item`'s reader, which reads its value and nothing else of the case, or why
/// it has none.
fn read_code_id(
    db: &TestDB,
    item: &Item,
    readers: &[(String, hir_def::FunctionId)],
) -> Result<hir_ty_identity::CodeId, String> {
    let name = reader(item.name.rsplit("::").next().expect("a name"));
    let Some(&(_, func)) = readers.iter().find(|it| it.0 == name) else {
        panic!("{}: no reader `{name}`", item.name);
    };
    let env =
        ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: fixture::test_crate(db) }.store();
    let root = hir_ty_identity::Instance {
        subject: hir_ty_identity::Subject::Body(Body::Fn(func)),
        args: GenericArgs::empty().store(),
    };
    hir_ty_identity::code_id(db, &root, &env).map_err(|why| why.to_string())
}

/// Of an item of a case's crate: what the build emits for it, its reader's code id, and the
/// value its reader reads.
struct Side {
    name: String,
    emitted: Result<String, String>,
    id: Result<hir_ty_identity::CodeId, String>,
    value: Result<Value, String>,
}

fn sides(case: &str, text: &str) -> Vec<Side> {
    let db = fixture::db(text).into_db();
    let items = items(&db, case);
    let krate = fixture::test_crate(&db);
    let readers: Vec<(String, hir_def::FunctionId)> = hir_def::test_items::declarations(&db, krate)
        .into_iter()
        .filter_map(|it| match it {
            ModuleDefId::FunctionId(id) => Some((
                hir_def::signatures::FunctionSignature::of(&db, id).name.as_str().to_owned(),
                id,
            )),
            _ => None,
        })
        .filter(|it| it.0.starts_with("__xcheck_read_"))
        .collect();
    assert_eq!(
        readers.len(),
        items.len(),
        "{case}: the readers made from the text ({:?}) are not one per item ({:?})",
        readers.iter().map(|it| &it.0).collect::<Vec<_>>(),
        items.iter().map(|it| &it.name).collect::<Vec<_>>()
    );
    let reads = read_consts(&db);
    assert_eq!(reads.len(), items.len(), "{case}: the readers' constants are not one per item");
    blaze_query::db::attach(&db, || {
        (items.iter())
            .map(|item| {
                let short = item.name.rsplit("::").next().expect("a name");
                let Some(&(_, read)) = reads.iter().find(|it| it.0 == short) else {
                    panic!("{}: no reader's constant", item.name);
                };
                Side {
                    name: item.name.clone(),
                    emitted: emitted(&db, item, read),
                    id: read_code_id(&db, item, &readers),
                    value: value(&db, read),
                }
            })
            .collect()
    })
}

/// The code generator's refusals of a word of a value that identity makes too, worded alike:
/// a word holding one of const eval's ids that no provenance makes a pointer (`refuse_ids_left`
/// and `vtable_word`), a function pointer holding no function, a pointer to memory const eval
/// did not keep, a vtable read as another trait's.
const SHARED_REFUSALS: [&str; 5] = [
    "it has no provenance, so const eval did not make it a pointer",
    "an id with no provenance, a vtable made a number",
    "whose value is no function const eval made",
    "rustc refuses a dangling pointer in a constant's value",
    "rustc refuses a vtable of another trait in a constant's value",
];

/// Which of [`SHARED_REFUSALS`] a refusal is.
fn shared_refusal(refusal: &str) -> Option<&'static str> {
    SHARED_REFUSALS.into_iter().find(|it| refusal.contains(it))
}

/// The first line where two emitted texts differ, for a finding to show.
fn first_difference(a: &str, b: &str) -> String {
    let (mut a_lines, mut b_lines) = (a.lines(), b.lines());
    for at in 1.. {
        match (a_lines.next(), b_lines.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (None, None) => return "none".to_owned(),
            (x, y) => {
                let cut = |it: Option<&str>| {
                    it.map_or("<end>".to_owned(), |it| it.chars().take(240).collect())
                };
                return format!("line {at}: `{}` became `{}`", cut(x), cut(y));
            }
        }
    }
    unreachable!()
}

/// For every constant and static of the relocate lane's cases (the corpus, `p1`..`p4`, `rv`..
/// `rv6`, and the codegen relocation tests' fixtures) and every edit of its case's source (the
/// pointee, the impl or type behind a vtable, a slice tail's length, a function a pointer runs,
/// an item first that moves the rest, an unrelated function), the item's code id changes if and
/// only if what the build emits for its value and everything that reaches does (rule 21 and
/// its converse: no input missed, nothing hashed the build doesn't emit). Each side refusing is
/// listed. No value the code generator builds is refused by identity (a refused value would
/// get no code id, and its caller none). Where the code generator refuses a word of a corpus
/// value for one of [`SHARED_REFUSALS`] (an id with no provenance, a function pointer holding
/// no function, a dangling pointer, a vtable of another trait), identity refuses it too, with
/// the same reason. That holds for the values identity walks, those whose table of functions
/// and vtables has entries; past the corpus, identity may take a value the code generator
/// refuses (a dangling pointer in a value that holds no id, a union's or a `TypeId`'s wording),
/// which is the safe way to differ: a value the code generator refuses has no code to reuse.
/// Each item is read through a trait's constant (see [`crate_text`]), so only
/// `Facts::allocation` hashes its value, and the edits that keep its bytes (which static,
/// provenance, alignment) are checked to.
#[test]
fn a_values_code_id_changes_exactly_when_its_emitted_code_does() {
    // alloc: the test's cases, each its head and source
    let mut cases: Vec<(&str, String, String)> = CORPUS
        .iter()
        .map(|(case, text)| {
            let head = format!("//- minicore: {MINICORE}\n//- /src/lib.rs crate:kernel\n");
            (*case, head, format!("{PRELUDE}\n{text}\n{UNRELATED}"))
        })
        .collect();
    for (case, text) in FIXTURES {
        let text = text.trim_start();
        let (head, source) = text.split_at(text.find('\n').expect("a fixture's first line") + 1);
        cases.push((*case, head.to_owned(), format!("{source}{UNRELATED}")));
    }
    let mut findings = Vec::new();
    let mut refusals = Vec::new();
    // The values both sides refuse for one of the shared reasons.
    let mut agreed: Vec<String> = Vec::new();
    // Per item: the edits that changed both sides, and those that changed neither.
    let mut counts: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for (case, head, source) in &cases {
        let base = sides(case, &crate_text(head, source));
        for Side { name, emitted, id, value } in &base {
            // A value that holds no id identity does not walk, and may take (see above).
            let walked = value.as_ref().map_or(true, |it| it.ids);
            match (emitted, id) {
                (Ok(_), Ok(_)) => {
                    counts.entry(name.clone()).or_default();
                }
                (Err(e), Ok(_)) if walked && shared_refusal(e).is_some() => findings
                    .push(format!("{name}: the code generator refuses a word identity takes: {e}")),
                (Ok(_), Err(why)) => findings.push(format!(
                    "{name}: identity refuses what the code generator builds: {why}"
                )),
                (Err(e), Err(why)) => {
                    if let Some(reason) = shared_refusal(e) {
                        match why.contains(reason) {
                            true => agreed.push(name.clone()),
                            false => findings.push(format!(
                                "{name}: identity refuses for another reason than the code generator's `{reason}`: {why}"
                            )),
                        }
                    }
                    refusals.push(format!(
                        "{name}: the code generator refuses it: {e}; identity: {why}"
                    ));
                }
                (Err(e), Ok(_)) => refusals
                    .push(format!("{name}: the code generator refuses it: {e}; identity takes it")),
            }
        }
        let moved =
            Edit { name: "an item first, moving every other", from: "", to: "", only: None };
        let edits = EDITS
            .iter()
            .chain(CASE_EDITS.iter().filter(|it| it.0 == *case).map(|it| &it.1))
            .filter(|it| source.contains(it.from))
            .chain(std::iter::once(&moved));
        let mut made = 0;
        for edit in edits {
            let p = || {
                let name = format!("{case}::P");
                let side = base
                    .iter()
                    .find(|it| it.name == name)
                    .unwrap_or_else(|| panic!("{case}: no `P`"));
                (
                    name,
                    side.value
                        .as_ref()
                        .unwrap_or_else(|e| panic!("{case}::P does not evaluate: {e}")),
                )
            };
            let to = match edit.to.contains(ADDRESS) {
                true => {
                    let word: [u8; 8] = p().1.bytes[..8].try_into().expect("a word");
                    edit.to.replace(ADDRESS, &format!("{:#x}", usize::from_le_bytes(word)))
                }
                false => edit.to.to_owned(),
            };
            let edited = match edit.from {
                "" => format!("{MOVED}{source}"),
                from => source.replace(from, &to),
            };
            made += 1;
            let after = sides(case, &crate_text(head, &edited));
            if let Some(only) = edit.only {
                let (name, before) = p();
                let after =
                    after.iter().find(|it| it.name == name).expect("the same items after an edit");
                let after = after
                    .value
                    .as_ref()
                    .unwrap_or_else(|e| panic!("{name}, after `{}`: {e}", edit.name));
                assert_eq!(
                    before.without(only),
                    after.without(only),
                    "{name}, after `{}`: more changed than {only:?}",
                    edit.name
                );
                assert_ne!(
                    before, after,
                    "{name}, after `{}`: its {only:?} did not change",
                    edit.name
                );
            }
            for (
                Side { name, emitted, id, .. },
                Side { emitted: emitted_after, id: id_after, .. },
            ) in base.iter().zip(&after)
            {
                let (Ok(emitted), Ok(id), Ok(emitted_after), Ok(id_after)) =
                    (emitted, id, emitted_after, id_after)
                else {
                    if emitted.is_ok() && id.is_ok() {
                        refusals.push(format!(
                            "{name}, after `{}`: refused ({:?} / {:?})",
                            edit.name,
                            emitted_after.as_ref().err(),
                            id_after.as_ref().err()
                        ));
                    }
                    continue;
                };
                let (code_changed, id_changed) = (emitted != emitted_after, id != id_after);
                eprintln!(
                    "compared {name} after `{}`: emitted changed {code_changed}, code id changed {id_changed}",
                    edit.name
                );
                let count = counts.entry(name.clone()).or_default();
                match (code_changed, id_changed) {
                    (true, true) => count.0 += 1,
                    (false, false) => count.1 += 1,
                    (true, false) => findings.push(format!(
                        "{name}, after `{}`: what the build emits changed and its code id did not ({})",
                        edit.name,
                        first_difference(emitted, emitted_after)
                    )),
                    (false, true) => findings.push(format!(
                        "{name}, after `{}`: its code id changed and what the build emits did not",
                        edit.name
                    )),
                }
            }
        }
        assert!(made > 2, "{case}: no edit of its own applies");
    }
    for refused in &refusals {
        eprintln!("refused {refused}");
    }
    // A case or an edit that doesn't type-check is refused by both sides and checks nothing.
    let broken: Vec<_> =
        refusals.iter().filter(|it| it.contains("HasErrors")).map(String::as_str).collect();
    assert!(broken.is_empty(), "cases or edits that don't type-check:\n{}", broken.join("\n"));
    for (name, (changed, kept)) in &counts {
        eprintln!("checked {name}: {changed} edits changed both, {kept} changed neither");
    }
    assert!(findings.is_empty(), "{} findings:\n{}", findings.len(), findings.join("\n"));
    // Not vacuous: the values identity used to refuse are checked, each changed by an edit,
    // every case has a value an edit changes, and every value one that changes nothing.
    for name in ["DYN_NUMBER", "FN_NUMBER", "WRONG_TRAIT", "WRONG_ARGS", "ONLY_PROVENANCE"] {
        let name = format!("refusals::{name}");
        assert!(
            agreed.contains(&name),
            "{name} is not refused by both sides for one shared reason"
        );
    }
    for name in [
        "asptr::S",
        "muninit::S",
        "sltail::S",
        "which_static::P",
        "provenance::P",
        "alignment::P",
        "parts::P",
        "copy::Q",
    ] {
        let changed = counts.get(name).map_or(0, |it| it.0);
        assert!(changed > 0, "{name} is not checked, or no edit changed it");
    }
    for (case, _, _) in &cases {
        let prefix = format!("{case}::");
        let changed = counts.iter().any(|(name, it)| name.starts_with(&prefix) && it.0 > 0);
        assert!(changed, "{case}: no value of it checked with an edit that changed it");
    }
    let unchanged: Vec<_> =
        counts.iter().filter(|it| it.1.1 == 0).map(|it| it.0.as_str()).collect();
    assert!(unchanged.is_empty(), "values no edit left alone: {unchanged:?}");
}
