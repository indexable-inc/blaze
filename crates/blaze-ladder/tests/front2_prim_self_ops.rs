//! Binary operators whose impl has a PRIMITIVE self type and is written outside core (tempo's
//! census: num-bigint 0.4.8, `LEFT 51 .. cannot calculate the remainder`, `LEFT 36/18 .. cannot
//! subtract`): `impl Rem<BigUint> for u64` (biguint/division.rs) and, by its macros
//! (macros.rs `forward_scalar_val_ref_binop_to_val_val`), `impl Rem<BigUint> for &u64 { fn
//! rem(self, other: BigUint) -> BigUint { Rem::rem(*self, other) } }`. The Reference,
//! "Implementations", orphan rules: `impl Rem<Big> for u64` is valid in the crate of `Big` (`T1`
//! is local), so the goal `u64: Rem<Big>` in that crate is answered by it.
//!
//! Two crates on the RECORDS path (as tests/front2_prim_self_foreign_impl.rs builds them): `core`
//! (the sized family, `Clone`, `ops::{Rem, Sub}`, the preludes) and `big` (`#![no_std]`, the
//! program's crate, so its own bodies are checked). Every body of `big` must check clean but
//! the control `missing`, `Rem::rem(3u32, Big)`, for which no impl is written: E0277.

#![feature(allocator_api)]

use std::alloc::Global;
use std::collections::HashMap;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::program::Front2Program;
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_scope::names::Written;
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

const CORE: &str = "#![no_core]
#![feature(no_core, lang_items)]
pub mod marker {
    #[lang = \"pointee_sized\"]
    pub trait PointeeSized {}
    #[lang = \"meta_sized\"]
    pub trait MetaSized: PointeeSized {}
    #[lang = \"sized\"]
    pub trait Sized: MetaSized {}
    #[lang = \"copy\"]
    pub trait Copy: crate::clone::Clone {}
    impl Copy for u64 {}
    impl Copy for u32 {}
    impl Copy for u16 {}
}
pub mod clone {
    #[lang = \"clone\"]
    pub trait Clone: crate::marker::Sized {
        fn clone(&self) -> Self;
    }
    impl Clone for u64 {
        fn clone(&self) -> u64 {
            *self
        }
    }
    impl Clone for u32 {
        fn clone(&self) -> u32 {
            *self
        }
    }
    impl Clone for u16 {
        fn clone(&self) -> u16 {
            *self
        }
    }
}
pub mod ops {
    #[lang = \"rem\"]
    pub trait Rem<Rhs = Self> {
        type Output;
        fn rem(self, rhs: Rhs) -> Self::Output;
    }
    #[lang = \"sub\"]
    pub trait Sub<Rhs = Self> {
        type Output;
        fn sub(self, rhs: Rhs) -> Self::Output;
    }
}
pub mod prelude {
    pub mod v1 {
        pub use crate::clone::Clone;
        pub use crate::marker::{Copy, Sized};
    }
    pub mod rust_2015 {
        pub use super::v1::*;
    }
    pub mod rust_2018 {
        pub use super::v1::*;
    }
    pub mod rust_2021 {
        pub use super::v1::*;
    }
    pub mod rust_2024 {
        pub use super::v1::*;
    }
}
";

/// num-bigint's shapes: the by-value impl of a primitive self written by a macro with a `$t:ty`
/// self (as `impl_scalar!`-style macros do) and by hand, the by-reference impls forwarding to
/// it by a macro (`forward_scalar_val_ref_binop_to_val_val`), and the subtract shape whose
/// right-hand side is a reference (`impl Sub<&BigInt> for &u16`).
const BIG: &str = "#![no_std]
use core::ops::{Rem, Sub};
pub struct Big;
impl Clone for Big {
    fn clone(&self) -> Big {
        Big
    }
}
macro_rules! rem_by_value {
    ($t:ty) => {
        impl Rem<Big> for $t {
            type Output = Big;
            fn rem(self, other: Big) -> Big {
                other
            }
        }
    };
}
macro_rules! forward_ref_self {
    (impl $imp:ident<$scalar:ty> for $res:ty, $method:ident) => {
        impl $imp<$res> for &$scalar {
            type Output = $res;
            fn $method(self, other: $res) -> $res {
                $imp::$method(*self, other)
            }
        }
    };
}
macro_rules! forward_ref_ref {
    (impl $imp:ident<$scalar:ty> for $res:ty, $method:ident) => {
        impl $imp<&$res> for &$scalar {
            type Output = $res;
            fn $method(self, other: &$res) -> $res {
                $imp::$method(*self, other.clone())
            }
        }
    };
}
rem_by_value!(u64);
forward_ref_self!(impl Rem<u64> for Big, rem);
impl Sub<Big> for u16 {
    type Output = Big;
    fn sub(self, other: Big) -> Big {
        other
    }
}
forward_ref_self!(impl Sub<u16> for Big, sub);
forward_ref_ref!(impl Sub<u16> for Big, sub);
pub fn rem_value() -> Big {
    Rem::rem(3u64, Big)
}
pub fn rem_operator() -> Big {
    3u64 % Big
}
pub fn rem_by_ref(x: &u64) -> Big {
    x % Big
}
pub fn sub_ref_ref(x: &u16, y: &Big) -> Big {
    x - y
}
pub fn missing() -> Big {
    Rem::rem(3u32, Big)
}
";

/// World indexes: 0 `big` (the program), 1 `core`.
const BIG_AT: usize = 0;
const CORE_AT: usize = 1;

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (BIG_AT, "/big/lib.rs") => Some(BIG),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder prim self ops crate", name.as_bytes())),
        edition: Edition::E2021,
        implicit: Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: false, entry: false },
        root,
        shown_from: "/",
        features: &[],
        deps,
        cfg: &[],
        script: None,
    }
}

fn names() -> &'static Names<'static> {
    let crates = [input("big", "/big/lib.rs", &[("core", CORE_AT)]), input("core", "/core/lib.rs", &[])];
    let sysroot = Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None };
    let names = resolve_crates(&crates, sysroot, &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
    expansions: Vec<HashMap<ItemKey, &'static str>>,
    origins: Vec<HashMap<ItemKey, &'static [u8]>>,
}

/// The origin column an expansion's text keeps, as a loader serves it (as tests/d106_records.rs
/// `made_origins`): empty for a text that keeps none by design.
fn made_origins(call: ItemKey, origins: &Result<&[u8], blaze_scope::expansion_origins::OriginsError>) -> &'static [u8] {
    use blaze_scope::expansion_origins::OriginsError;
    match origins {
        Ok(bytes) => Box::leak(bytes.to_vec().into_boxed_slice()),
        Err(OriginsError::Rewritten { .. } | OriginsError::Unplaced { .. }) => &[],
        Err(e) => panic!("the origins of expansion {call:x}: {e:?}"),
    }
}

impl Load<'static> for Kept {
    fn names(&self, krate: usize) -> Option<&'static [u8]> {
        self.names.get(krate).copied()
    }

    fn items(&self, krate: usize) -> Option<&'static [u8]> {
        self.items.get(krate).copied()
    }

    fn file(&self, krate: usize, path: blaze_scope::package_path::PackagePath<'_>) -> Option<&'static str> {
        self.files.get(krate)?.get(path.as_str()).copied()
    }

    fn expansion(&self, krate: usize, call: ItemKey) -> Option<LoadedExpansion<'static>> {
        let text = self.expansions.get(krate)?.get(&call).copied()?;
        // The fixture keeps the origins in the joined form: its two parts, cut where the column
        // starts (empty: the text keeps none). None kept for the call: none served.
        let origins = self.origins.get(krate).and_then(|by_call| by_call.get(&call)).copied().map(|bytes| {
            if bytes.is_empty() {
                (&[][..], &[][..])
            } else {
                blaze_scope::expansion_origins::split(bytes).unwrap_or_else(|e| panic!("the fixture's origins of {call:?}: {e}"))
            }
        });
        Some(LoadedExpansion { text, origins })
    }
}

fn records() -> &'static Records<'static> {
    let names = names();
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new(), expansions: Vec::new(), origins: Vec::new() };
    let mut crate_names: Vec<&'static str> = Vec::new();
    let mut keys: Vec<CrateItemKey> = Vec::new();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        kept.expansions.push(c.expansions.iter().filter_map(|(call, e)| e.made.map(|m| (*call, &*Box::leak(m.text.to_string().into_boxed_str())))).collect());
        kept.origins.push(c.expansions.iter().filter_map(|(call, e)| e.made.as_ref().map(|m| (*call, made_origins(*call, &m.origins)))).collect());
        crate_names.push(c.name);
        keys.push(c.key);
    }
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(crate_names), g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    Box::leak(Box::new(Records::new(Vec::leak(keys), load, log, frame, g)))
}

#[test]
fn primitive_self_operator_impls_answer_their_goals() {
    let records = records();
    let g = a();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the world's frame")));
    let mut world = ProjectWorld::from_records(records, frame, &[BIG_AT, CORE_AT], g).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let bodies: Vec<_> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    let mut wrong: Vec<String> = Vec::new();
    let mut checked: Vec<String> = Vec::new();
    let mut control: Option<String> = None;
    for body in bodies.iter().copied() {
        let label = match body {
            BodyRef::Free(k) => NamesSource::name_of(records, k).unwrap_or("?").to_string(),
            BodyRef::Member { container, name, .. } => format!("{name} of impl {container:x}"),
        };
        let v = world.check(body, g);
        let line = if v.is_clean() {
            "clean".to_string()
        } else {
            let gaps: Vec<String> = v.gaps.iter().map(|gap| format!("{gap}")).collect();
            format!("NOT clean: diagnostics {:?}, gaps {gaps:?}, refused {:?}", v.diagnostics, v.refused.as_ref().map(|r| r.text()))
        };
        println!("BODY {label}: {line}");
        if label == "missing" {
            control = Some(format!("{:?}", v.diagnostics));
            continue;
        }
        if !v.is_clean() {
            wrong.push(format!("{label}: {line}"));
        }
        checked.push(label);
    }
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
    for name in ["rem_value", "rem_operator", "rem_by_ref", "sub_ref_ref"] {
        assert!(checked.iter().any(|c| c == name), "no body `{name}` was checked: {checked:?}");
    }
    // The six impl members (two clones are core's: not this crate's bodies): Big's clone, the
    // macro's `rem` by value, the hand-written `sub`, and the three forwarding members.
    let members = checked.iter().filter(|c| c.contains(" of impl ")).count();
    assert_eq!(members, 6, "the impl members checked: {checked:?}");
    assert!(wrong.is_empty(), "bodies not clean: {wrong:#?}");
    let control = control.expect("the control `missing` was checked");
    assert!(control.contains("E0277"), "`u32: Rem<Big>` has no impl and must stay E0277: {control}");
}

/// The census's path (blaze-real `tempo_build`: `Front2Program::survey_from` over the solved
/// names): `big` is a DEPENDENCY of the program's crate `app`, so its bodies are checked as the
/// walk reaches them from `app`'s roots, the forwarding impl members through the operators.
const APP: &str = "#![no_std]
pub fn go() {
    let _ = big::rem_value();
    let _ = big::rem_operator();
    let _ = big::rem_by_ref(&3);
    let _ = big::sub_ref_ref(&3, &big::Big);
}
pub fn go_missing() {
    let _ = big::missing();
}
";

/// World indexes of the survey: 0 `app` (the program), 1 `core`, 2 `big`.
const S_APP: usize = 0;
const S_CORE: usize = 1;
const S_BIG: usize = 2;

struct SurveyTexts;

impl SourceTexts<'static> for SurveyTexts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (S_APP, "/app/lib.rs") => Some(APP),
            (S_CORE, "/core/lib.rs") => Some(CORE),
            (S_BIG, "/big/lib.rs") => Some(BIG),
            _ => None,
        }
    }
}

fn survey_names() -> &'static Names<'static> {
    let crates = [
        input("app", "/app/lib.rs", &[("core", S_CORE), ("big", S_BIG)]),
        input("core", "/core/lib.rs", &[]),
        input("big", "/big/lib.rs", &[("core", S_CORE)]),
    ];
    let sysroot = Sysroot { core: Some(CrateId(S_CORE as u32)), alloc: None, std: None, bstd: None };
    let names = resolve_crates(&crates, sysroot, &SurveyTexts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

#[test]
fn primitive_self_operator_impls_answer_their_goals_in_a_dependency() {
    let names = survey_names();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the survey's frame")));
    let mut roots = Vec::new();
    for name in ["go", "go_missing"] {
        let segments = [name];
        let written = Written { kind: PathKind::Plain, segments: &segments[..] };
        let found = blaze_scope::names::path(names, names.root(S_APP), Namespace::Values, written, a()).unwrap_or_else(|e| panic!("`app::{name}`: {e:?}"));
        roots.push(found.whole(1).unwrap_or_else(|| panic!("`app::{name}` names no fn")).key);
    }
    let mut left_out = Vec::new_in(Global);
    let _program = Front2Program::survey_from(names, frame, &[], &roots, &mut left_out, None, a()).unwrap_or_else(|e| panic!("no survey: {e}"));
    let left: Vec<(String, String)> = left_out.iter().map(|(f, class)| (f.to_string(), class.to_string())).collect();
    for (f, class) in left.iter() {
        println!("LEFT {f}: {class}");
    }
    // Only the control is left out, by E0277.
    let others: Vec<&(String, String)> = left.iter().filter(|(f, _)| !f.ends_with("missing")).collect();
    assert!(others.is_empty(), "fns left out that check clean by Rust's rules: {others:#?}");
    let control: Vec<&(String, String)> = left.iter().filter(|(f, _)| f.ends_with("::missing") || f == "missing").collect();
    assert_eq!(control.len(), 1, "the control `big::missing` is left out once: {left:?}");
    assert!(control[0].1.contains("E0277"), "`u32: Rem<Big>` has no impl and must stay E0277: {:?}", control[0]);
}
