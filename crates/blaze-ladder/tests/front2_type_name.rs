//! `core::intrinsics::type_name::<T>()` in constants, evaluated by front2's constant evaluator
//! with the real core and alloc: consts.rs `ConstsWorld` answers `ConstWorld::type_names` with
//! the item paths of the names source (`type_name_path`), printed by blaze-build's one printer
//! (type_name.rs). Before it, the hello copy (round 3, tree 4dd8a4485e) stopped six times at
//! "`type_name` in a world that names no items (`ConstWorld::type_names`) is not evaluated yet",
//! reached through `core::any::type_name`'s `const { intrinsics::type_name::<T>() }`.
//!
//! The rule: the pinned core, library/core/src/any.rs, `type_name`: "Returns the name of a type
//! as a string slice"; rustc prints an item by its def path from its crate's name (the
//! declaring module, never a re-export: `alloc::string::String`, not `std::string::String`).
//!
//! Lines it prints (and asserts):
//!   `OPTION: core::option::Option<alloc::string::String>` (two crates' paths, an argument)
//!   `LOCAL: type_names::D` (this crate's item, by the crate's name)
//!   `MIXED: (u8, &str, [i32; 3])` (primitives, a reference, a tuple, an array's length)
//!   `FN_PTR: fn(u8) -> bool`
//!   `DYN: &dyn core::fmt::Debug` (a trait's path)
//!   `ANY: u8` (through `core::any::type_name`, the path hello reaches)
//!
//! A wrong fix fails here: a path read from a re-export gives `std::option::Option<..>`; a path
//! without the crate's name gives `option::Option<..>` and `D`. Without the fix every line is
//! the refusal "`type_name` in a world that names no items".
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_consts.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "#![feature(core_intrinsics)]

extern crate alloc;
use alloc::string::String;
use core::intrinsics::type_name;

fn main() {}

pub struct D(u8);

const OPTION: &str = type_name::<Option<String>>();
const LOCAL: &str = type_name::<D>();
const MIXED: &str = type_name::<(u8, &'static str, [i32; 3])>();
const FN_PTR: &str = type_name::<fn(u8) -> bool>();
const DYN: &str = type_name::<&'static dyn core::fmt::Debug>();
const ANY: &str = core::any::type_name::<u8>();
";

/// A line of the outcome, as it happens: appended to the file `FRONT2_WORK_LIST` names, since a
/// passing test's output is not shown by the lane's check script.
fn note(s: &str) {
    eprintln!("{s}");
    if let Ok(p) = std::env::var("FRONT2_WORK_LIST") {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{s}");
        }
    }
}

#[test]
fn type_name_prints_item_paths_from_the_records() {
    let heap = blaze_own::testing::heap();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/type_names/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "type_names", dir: "/type_names/", root: "/type_names/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let mut buckets = None;
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let cases: [(&str, &str); 6] = [
        ("OPTION", "core::option::Option<alloc::string::String>"),
        ("LOCAL", "type_names::D"),
        ("MIXED", "(u8, &str, [i32; 3])"),
        ("FN_PTR", "fn(u8) -> bool"),
        ("DYN", "&dyn core::fmt::Debug"),
        ("ANY", "u8"),
    ];
    let mut keys = Vec::new();
    for (name, _) in cases {
        let k = lib(name);
        keys.push(front.keys.item(k).unwrap_or_else(|_| panic!("{name}'s key")));
    }
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let mut consts = Consts::default();
    let mut lines = Vec::new();
    for ((name, _), key) in cases.iter().zip(keys) {
        let line = match consts.value(&mut records, prepared, key) {
            Ok(rec) => {
                // A `&str` record: its root is the (address, length) pair, the text another
                // allocation of the same record the address points to.
                let root = rec.data.allocs.first().map(|a| a.bytes.as_slice()).unwrap_or(&[]);
                let len = root.get(8..16).map(|b| u64::from_le_bytes(b.try_into().expect("eight bytes")));
                let text = rec.data.allocs.iter().skip(1).find_map(|a| std::str::from_utf8(a.bytes.as_slice()).ok().filter(|t| Some(t.len() as u64) == len));
                match text {
                    Some(t) => format!("{name}: {t}"),
                    None => format!("{name}: a record that is no `&str`: {rec:?}"),
                }
            }
            Err(e) => format!("{name}: refused: {e}"),
        };
        note(&line);
        lines.push(line);
    }
    let want: Vec<String> = cases.iter().map(|(n, v)| format!("{n}: {v}")).collect();
    assert_eq!(lines, want);
}

const LOCAL_NEW: blaze_ladder::run::NewFront<'static> =
    blaze_ladder::run::NewFront { package: "ladder", edition: "2024", shelf: None, overflow: blaze_build::OverflowChecks::On };

const LOCAL_PRELUDE: &str = include_str!("../../blaze-cli/tests/ladder/front2/x0-async-01/src/prelude.rs");

const LOCAL_HEAD: &str = "#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod prelude;
use prelude::*;

mod intr {
    use super::Sized;
    #[rustc_intrinsic]
    pub const fn type_name<T: ?Sized>() -> &'static str;
}

const fn type_name<T: ?Sized>() -> &'static str {
    const { intr::type_name::<T>() }
}

const fn type_name_of_val<T: ?Sized>(_: &T) -> &'static str {
    type_name::<T>()
}

fn str_eq(a: &str, b: &str) -> bool {
    let (pa, la): (*const u8, usize) = unsafe { transmute(a) };
    let (pb, lb): (*const u8, usize) = unsafe { transmute(b) };
    if la != lb {
        return false;
    }
    let mut i: usize = 0;
    while i < la {
        let ca = unsafe { *((pa as usize + i) as *const u8) };
        let cb = unsafe { *((pb as usize + i) as *const u8) };
        if ca != cb {
            return false;
        }
        i += 1;
    }
    true
}

#[rustc_intrinsic]
const unsafe fn transmute<Src, Dst>(src: Src) -> Dst;
";

fn run_local_no_core(body: &str) -> Result<u8, String> {
    let text = [LOCAL_HEAD, body].concat();
    let files = [SourceFile { path: "src/main.rs", text: &text }, SourceFile { path: "src/prelude.rs", text: LOCAL_PRELUDE }];
    blaze_ladder::run::run_with(&files, "src/main.rs", None, blaze_ladder::run::FrontChoice::New(LOCAL_NEW), blaze_own::testing::heap().shared())
        .map_err(|e| format!("{e}").replace('\n', " "))
}

#[test]
fn local_fn_and_nested_local_fn_in_free_fn_runtime_and_const() {
    let got = run_local_no_core(
        "
fn outer() -> u8 {
    fn inner() -> u8 {
        fn deep() {}
        let rt_deep = type_name_of_val(&deep);
        let ct_deep: &str = const { type_name_of_val(&deep) };
        let mut ok = 0u8;
        if str_eq(rt_deep, \"ladder::outer::inner::deep\") { ok += 1; }
        if str_eq(ct_deep, \"ladder::outer::inner::deep\") { ok += 2; }
        ok
    }
    let rt_inner = type_name_of_val(&inner);
    let ct_inner: &str = const { type_name_of_val(&inner) };
    let mut ok = inner();
    if str_eq(rt_inner, \"ladder::outer::inner\") { ok += 4; }
    if str_eq(ct_inner, \"ladder::outer::inner\") { ok += 8; }
    ok
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    outer() as i32
}
",
    );
    assert_eq!(got, Ok(15), "all four runtime/const checks for local and nested local fns must match rustc");
}

#[test]
fn inherent_impl_method_and_local_fn_inside_inherent_method_runtime_and_const() {
    let got = run_local_no_core(
        "
struct Collider;
struct Gen<T>(T);

impl Collider {
    const ASSOC_LOCAL: &'static str = {
        fn _cf() {}
        type_name_of_val(&_cf)
    };

    fn spike(&self) -> u8 {
        fn _f() {}
        let rt_f = type_name_of_val(&_f);
        let ct_f: &str = const { type_name_of_val(&_f) };
        let rt_method = type_name_of_val(&Self::spike);
        let c = || {};
        let rt_closure = type_name_of_val(&c);
        let mut ok = 0u8;
        if str_eq(rt_f, \"ladder::Collider::spike::_f\") { ok += 1; }
        if str_eq(ct_f, \"ladder::Collider::spike::_f\") { ok += 2; }
        if str_eq(rt_method, \"ladder::Collider::spike\") { ok += 4; }
        if str_eq(rt_closure, \"ladder::Collider::spike::{{closure}}\") { ok += 8; }
        if str_eq(Self::ASSOC_LOCAL, \"ladder::Collider::ASSOC_LOCAL::_cf\") { ok += 16; }
        ok
    }
}

impl<T> Gen<T> {
    fn gmethod(&self) -> u8 {
        fn _gf() {}
        let rt = type_name_of_val(&_gf);
        let ct: &str = const { type_name_of_val(&_gf) };
        let mut ok = 0u8;
        if str_eq(rt, \"ladder::Gen<_>::gmethod::_gf\") { ok += 32; }
        if str_eq(ct, \"ladder::Gen<_>::gmethod::_gf\") { ok += 64; }
        ok
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = Collider.spike();
    let b = Gen(7u8).gmethod();
    (a + b) as i32
}
",
    );
    assert_eq!(got, Ok(127), "inherent impl method, local fn, closure, assoc const, and generic inherent impl must match rustc");
}

#[test]
fn trait_impl_method_and_local_fn_inside_trait_impl_runtime_and_const() {
    let got = run_local_no_core(
        "
struct Collider;
struct Gen<T>(T);

trait Tr {
    fn tr_m(&self) -> u8;
}

trait TrArg<A> {
    fn am(&self) -> u8;
}

impl Tr for Collider {
    fn tr_m(&self) -> u8 {
        fn _tf() {}
        let rt = type_name_of_val(&_tf);
        let ct: &str = const { type_name_of_val(&_tf) };
        let c = || {};
        let rt_c = type_name_of_val(&c);
        let mut ok = 0u8;
        if str_eq(rt, \"<ladder::Collider as ladder::Tr>::tr_m::_tf\") { ok += 1; }
        if str_eq(ct, \"<ladder::Collider as ladder::Tr>::tr_m::_tf\") { ok += 2; }
        if str_eq(rt_c, \"<ladder::Collider as ladder::Tr>::tr_m::{{closure}}\") { ok += 4; }
        ok
    }
}

impl Tr for &Collider {
    fn tr_m(&self) -> u8 {
        fn _ref_f() {}
        let rt = type_name_of_val(&_ref_f);
        if str_eq(rt, \"<&ladder::Collider as ladder::Tr>::tr_m::_ref_f\") { 8 } else { 0 }
    }
}

impl<T> TrArg<u8> for Gen<T> {
    fn am(&self) -> u8 {
        fn _af() {}
        let rt = type_name_of_val(&_af);
        let ct: &str = const { type_name_of_val(&_af) };
        let mut ok = 0u8;
        if str_eq(rt, \"<ladder::Gen<_> as ladder::TrArg<u8>>::am::_af\") { ok += 16; }
        if str_eq(ct, \"<ladder::Gen<_> as ladder::TrArg<u8>>::am::_af\") { ok += 32; }
        ok
    }
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = Collider.tr_m();
    let r = &Collider;
    let b = <&Collider as Tr>::tr_m(&r);
    let c = Gen(1u8).am();
    (a + b + c) as i32
}
",
    );
    assert_eq!(got, Ok(63), "trait impl methods (plain, ref self, generic with trait arg) must match rustc");
}

#[test]
fn local_structs_inside_fn_impl_method_and_nested_local_fn() {
    let got = run_local_no_core(
        "
struct Collider;
trait Tr {
    fn tr_m(&self) -> u8;
}

fn free_local_struct() -> u8 {
    struct LocalStruct;
    let rt = type_name::<LocalStruct>();
    let ct: &str = const { type_name::<LocalStruct>() };
    fn inner() -> u8 {
        struct DeepStruct;
        let rt_d = type_name::<DeepStruct>();
        let ct_d: &str = const { type_name::<DeepStruct>() };
        let mut ok = 0u8;
        if str_eq(rt_d, \"ladder::free_local_struct::inner::DeepStruct\") { ok += 1; }
        if str_eq(ct_d, \"ladder::free_local_struct::inner::DeepStruct\") { ok += 2; }
        ok
    }
    let mut ok = inner();
    if str_eq(rt, \"ladder::free_local_struct::LocalStruct\") { ok += 4; }
    if str_eq(ct, \"ladder::free_local_struct::LocalStruct\") { ok += 8; }
    ok
}

impl Collider {
    fn spike(&self) -> u8 {
        struct InSpike;
        let rt = type_name::<InSpike>();
        let ct: &str = const { type_name::<InSpike>() };
        let mut ok = 0u8;
        if str_eq(rt, \"ladder::Collider::spike::InSpike\") { ok += 16; }
        if str_eq(ct, \"ladder::Collider::spike::InSpike\") { ok += 32; }
        ok
    }
}

impl Tr for Collider {
    fn tr_m(&self) -> u8 {
        struct InTr;
        let rt = type_name::<InTr>();
        if str_eq(rt, \"<ladder::Collider as ladder::Tr>::tr_m::InTr\") { 64 } else { 0 }
    }
}

fn with_local_impl() -> u8 {
    struct Local;
    impl Local {
        fn lm() -> u8 {
            fn _lmf() {}
            let rt = type_name_of_val(&_lmf);
            if str_eq(rt, \"ladder::with_local_impl::Local::lm::_lmf\") { 128 } else { 0 }
        }
    }
    Local::lm()
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let a = free_local_struct();
    let b = Collider.spike();
    let c = Collider.tr_m();
    let d = with_local_impl();
    (a + b + c + d) as i32
}
",
    );
    assert_eq!(got, Ok(255), "local structs in free fn, nested local fn, inherent impl, trait impl, and local impl must match rustc");
}

#[test]
fn non_adt_inherent_impl_and_ctors_runtime_and_const() {
    let got = run_local_no_core(
        "
trait DynTr {}
struct ImplDyn;
impl DynTr for ImplDyn {}

impl dyn DynTr {
    fn dm(&self) -> u8 {
        fn _df() {}
        let rt = type_name_of_val(&_df);
        let ct: &str = const { type_name_of_val(&_df) };
        let mut ok = 0u8;
        if str_eq(rt, \"<dyn ladder::DynTr>::dm::_df\") { ok += 1; }
        if str_eq(ct, \"<dyn ladder::DynTr>::dm::_df\") { ok += 2; }
        ok
    }
}

struct TupleS(u8);
enum EnumE {
    Var(u8),
}

#[unsafe(no_mangle)]
pub extern \"C\" fn main() -> i32 {
    let d: &dyn DynTr = &ImplDyn;
    let mut ok = d.dm();
    let s_ctor = type_name_of_val(&TupleS);
    let e_ctor = type_name_of_val(&EnumE::Var);
    if str_eq(s_ctor, \"ladder::TupleS::{{constructor}}\") { ok += 4; }
    if str_eq(e_ctor, \"ladder::EnumE::Var::{{constructor}}\") { ok += 8; }
    ok as i32
}
",
    );
    assert_eq!(got, Ok(15), "dyn inherent impl and tuple struct/variant constructors must match rustc");
}

#[test]
fn real_core_any_type_name_and_type_name_of_val_for_local_and_impl_items() {
    use blaze_ladder::front2::program::Front2Program;
    use blaze_ladder::seam::FrontEnd;

    let heap = blaze_own::testing::heap();
    let alloc = heap.alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let main = r#"
#![feature(const_type_name)]
#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    loop {}
}

mod sub {
    fn eq(a: &str, b: &str) -> bool {
        let ab = a.as_bytes();
        let bb = b.as_bytes();
        if ab.len() != bb.len() {
            return false;
        }
        let mut i = 0;
        while i < ab.len() {
            if ab[i] != bb[i] {
                return false;
            }
            i += 1;
        }
        true
    }

    pub struct Collider;
    pub trait Tr {
        fn tr_m(&self) -> u8;
    }

    pub fn outer() -> u8 {
        fn inner() -> u8 {
            fn deep() {}
            let mut ok = 0u8;
            if eq(core::any::type_name_of_val(&deep), "antithesis_trie_shapes::sub::outer::inner::deep") { ok += 1; }
            let ct: &str = const { core::any::type_name_of_val(&deep) };
            if eq(ct, "antithesis_trie_shapes::sub::outer::inner::deep") { ok += 2; }
            ok
        }
        struct LocalStruct;
        let mut ok = inner();
        if eq(core::any::type_name_of_val(&inner), "antithesis_trie_shapes::sub::outer::inner") { ok += 4; }
        if eq(core::any::type_name::<LocalStruct>(), "antithesis_trie_shapes::sub::outer::LocalStruct") { ok += 8; }
        ok
    }

    impl Collider {
        pub const fn const_spike_fn() -> &'static str {
            fn _f() {}
            core::any::type_name_of_val(&_f)
        }

        pub const fn const_spike_struct() -> &'static str {
            struct InConstSpike;
            core::any::type_name::<InConstSpike>()
        }

        pub fn spike(&self) -> u8 {
            fn _f() {}
            struct InSpike;
            let mut ok = 0u8;
            if eq(core::any::type_name_of_val(&_f), "antithesis_trie_shapes::sub::Collider::spike::_f") { ok += 16; }
            let ct: &str = const { core::any::type_name_of_val(&_f) };
            if eq(ct, "antithesis_trie_shapes::sub::Collider::spike::_f") { ok += 32; }
            if eq(core::any::type_name::<InSpike>(), "antithesis_trie_shapes::sub::Collider::spike::InSpike") { ok += 64; }
            ok
        }
    }

    impl Tr for Collider {
        fn tr_m(&self) -> u8 {
            fn _tf() {}
            if eq(core::any::type_name_of_val(&_tf), "<antithesis_trie_shapes::sub::Collider as antithesis_trie_shapes::sub::Tr>::tr_m::_tf") { 128 } else { 0 }
        }
    }
}

pub const CONST_LOCAL_FN: &str = {
    fn _c() {}
    core::any::type_name_of_val(&_c)
};

pub const CONST_LOCAL_STRUCT: &str = {
    struct LocalInConst;
    core::any::type_name::<LocalInConst>()
};

pub const CONST_INHERENT_LOCAL_FN: &str = sub::Collider::const_spike_fn();
pub const CONST_INHERENT_LOCAL_STRUCT: &str = sub::Collider::const_spike_struct();
pub const CONST_INHERENT_METHOD: &str = core::any::type_name_of_val(&sub::Collider::spike);

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    use sub::Tr;
    let total = sub::outer() + sub::Collider.spike() + sub::Collider.tr_m();
    if total == 255 { 0 } else { total as i32 }
}
"#;
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/antithesis_trie_shapes/src/main.rs".to_string(), main.to_string())];
    let edition = edition::PackageEdition::of_manifest("2024").expect("2024 edition");
    let user = UserCrate {
        name: "antithesis_trie_shapes",
        dir: "/antithesis_trie_shapes/",
        root: "/antithesis_trie_shapes/src/main.rs",
        files: &files,
        edition,
        panic: blaze_build::Panic::Unwind,
    };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("inputs: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: main }];

    // 1. Evaluate `const` items using real `core::any::type_name` and `core::any::type_name_of_val`
    //    via `ConstsWorld`.
    {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let mut front = Front2::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
        let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
        let mut buckets = None;
        let prepared = Prepares::new_in(blaze_own::testing::bump());
        let root = names.root(0);
        let lib = |name: &'static str| {
            names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, alloc)
                .ok()
                .and_then(|f| f.whole(1))
                .map(|d| d.key)
                .unwrap_or_else(|| panic!("`{name}` from crate root"))
        };
        let cases = [
            ("CONST_LOCAL_FN", "antithesis_trie_shapes::CONST_LOCAL_FN::_c"),
            ("CONST_LOCAL_STRUCT", "antithesis_trie_shapes::CONST_LOCAL_STRUCT::LocalInConst"),
            ("CONST_INHERENT_LOCAL_FN", "antithesis_trie_shapes::sub::Collider::const_spike_fn::_f"),
            ("CONST_INHERENT_LOCAL_STRUCT", "antithesis_trie_shapes::sub::Collider::const_spike_struct::InConstSpike"),
            ("CONST_INHERENT_METHOD", "antithesis_trie_shapes::sub::Collider::spike"),
        ];
        let mut keys = blaze_own::Vec::new_in(alloc);
        for (name, _) in cases {
            keys.push(front.keys.item(lib(name)).unwrap_or_else(|_| panic!("{name}'s key")));
        }
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let mut consts = Consts::default();
        for ((name, want), &key) in cases.iter().zip(keys.iter()) {
            let rec = consts.value(&mut records, prepared, key).unwrap_or_else(|e| panic!("{name}: refused: {e}"));
            let root = rec.data.allocs.first().map(|a| a.bytes.as_slice()).unwrap_or(&[]);
            let len = root.get(8..16).map(|b| u64::from_le_bytes(b.try_into().expect("eight bytes")));
            let text = rec
                .data
                .allocs
                .iter()
                .skip(1)
                .find_map(|a| std::str::from_utf8(a.bytes.as_slice()).ok().filter(|t| Some(t.len() as u64) == len))
                .unwrap_or_else(|| panic!("{name}: record is not a &str: {rec:?}"));
            assert_eq!(text, *want, "{name}");
        }
    }

    // 2. Verify `Front2Program::type_names()` (used by `FrontAdts` and `code_reads::Recorded`)
    //    returns the same paths and builds + runs the full program with real `core::any`.
    {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("frame");
        let mut program = Front2Program::new(&names, &frame, &sources, alloc).unwrap_or_else(|e| panic!("Front2Program: {e}"));
        let tn = program.type_names().expect("Front2Program implements TypeNames");
        let mut main_path = blaze_own::String::new_in(alloc);
        let entry = program.entry().expect("entry");
        let main_key = program.function(entry).key;
        assert!(tn.item_path(main_key, &mut |s| main_path.push_str(s)));
        assert_eq!(main_path.as_str(), "antithesis_trie_shapes::main");

        let built = blaze_ladder::run::build_with(&mut program, blaze_build::OverflowChecks::On, alloc)
            .unwrap_or_else(|e| panic!("build_with failed: {e}"));
        let status = blaze_ladder::run::run_built(built, None, heap.shared()).unwrap_or_else(|e| panic!("run_built failed: {e}"));
        assert_eq!(status, 0, "real core::any::type_name and type_name_of_val must pass at runtime");
    }
}
