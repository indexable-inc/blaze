//! Every vtable and function the relocation of a constant's or static's value emits is among
//! what code identity hashes of that value (rule 18). Each block of the identity port so far
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
    CallableDefId, ConstId, HasModule, ModuleDefId,
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
    fragment::{DataRequest, FuncTarget, Recorder, Requests},
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
    ("sltail", "static S: &Wrap<[&(dyn Tr + Sync)]> = &Wrap(1, [&A(5) as &(dyn Tr + Sync), &A(6)]);"),
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
    ("enumniche", "pub struct S(pub Option<&'static (dyn Tr + Sync)>, pub Option<&'static (dyn Tr + Sync)>);\nstatic E: S = S(None, Some(&A(3)));"),
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
    let mut cg = Recorder::new(db, &**crate::isa_for(target)?, target, checked);
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate };
    let mut value = None;
    crate::build(&mut cg, env, None, false, |fx| {
        let allocation = db
            .const_eval(id, GenericArgs::empty(), Some(fx.env))
            .map_err(|e| CodegenError::Frontend(format!("const eval: {e:?}")))?;
        value = Some(fx.allocation_blob(allocation, false)?);
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
                let _ = write!(why, "{}identity refuses it: {e}", if why.is_empty() { "" } else { "; " });
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
                || crate::impl_method(db, env, *f, args.as_ref()).is_ok_and(|(callee, callee_args)| {
                    callee == either::Either::Left(g) && erased(interner, callee_args).store() == plain(g_args)
                })
        })
    };
    let fn_item = |ty: hir_ty::next_solver::Ty<'_>| match ty.kind() {
        TyKind::FnDef(def, args) => match def.0 {
            CallableDefId::FunctionId(f) => names(f, &args.skip_binder().store()),
            // A constructor is hashed by its key and arguments, as identity's `callable` does.
            CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => true,
        },
        TyKind::Closure(id, _) => closures.contains(&Body::Closure(blaze_query::db::AsId::as_id(&id.0))),
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
                        blobs.push((format!("data {} (pointed to at {offset} of {at})", data.as_u32()), blob));
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
                    FuncTarget::Instance(instance, _) => format!("{:?} {:?}", instance.def, instance.args.as_ref()),
                    FuncTarget::Symbol(it) | FuncTarget::Path(it) | FuncTarget::Asm(it) => it.clone(),
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
        let db = fixture::db(&format!("//- minicore: {MINICORE}\n//- /src/lib.rs crate:kernel\n{PRELUDE}\n{text}\n")).into_db();
        let items = items(&db, case);
        assert!(!items.is_empty(), "{case}: no constant or static");
        hir_ty::attach_db(&db, || {
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
    assert!(broken.is_empty(), "corpus cases that don't type-check:\n{}", broken.iter().map(|it| it.as_str()).collect::<Vec<_>>().join("\n"));
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
