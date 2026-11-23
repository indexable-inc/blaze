//! PROBE prim-self-foreign-impl-1 (tempo census 5, 219 fns of num-bigint): a trait goal whose
//! Self is a PRIMITIVE and whose impl is written OUTSIDE core (`u64: Mul<BigUint>`, num-bigint
//! 0.4.8 biguint/division.rs `impl Rem<BigUint> for u64`) fails E0277; with an ADT Self
//! (`BigInt: Mul<u64>`) it passes.
//!
//! Three crates, on the RECORDS path (the one `blaze check` takes; as
//! tests/front2_records_side_scope.rs builds it): `core` (a small core: the sized family,
//! `ops::Mul`, and `prelude::rust_20xx`), `big` (`#![no_std]`, so its prelude is CORE's
//! `prelude::rust_2021`, a module of another crate, as every crate's but core's is), and `app`
//! (`#![no_std]`, the owner). The Reference, "Trait and lifetime bounds" / "Implementations",
//! orphan rules: "`impl<P1..=Pn> Trait<T1..=Tn> for T0` is valid if ... at least one of the
//! types `T0..=Tn` must be a local type": `impl Mul<Big> for u64` in `big` is valid (T1 = `Big`
//! is local), and `Mul::mul(3u64, Big)` is answered by it.
//!
//! Cases (each body must check clean):
//! - `prim_foreign`: `Mul::mul(3u64, Big)`, the impl `impl Mul<Big> for u64` in `big`: THE CLASS.
//! - `adt_foreign`: `Mul::mul(Small, Big)`, `impl Mul<Big> for Small` in `big`: control, ADT Self.
//! - `prim_root`: `Mul::mul(3u16, Mine)`, `impl Mul<Mine> for u16` in `app` itself: control, the
//!   impl in the owner's crate.
//!
//! The test prints, per case, one line per stage, then asserts every case clean:
//! `STAGE filed <case>: <bucket|unfiled(<NotFiled>)|NOWHERE>`: where the WRITER of `big`'s
//!   items record (blaze-scope items_record/buckets.rs `filing`) put the impl: in the bucket
//!   `of_trait(Mul, Head::Named(<self key>))` (`<self key>` = `blaze_scope::builtin_type("u64")`
//!   for a primitive, the walked key for an ADT), or the unfiled list with why. Read from the
//!   code: a primitive Self outside core walks past the module's own names to the PRELUDE
//!   layer (blaze-scope names.rs: prelude before builtin types), which is core's module, not
//!   held by the writer: `Why::NotRead { layer: Unheld::Prelude { builtin: Some(..) } }`, which
//!   `filing` files at the builtin (`Head::Named(builtin_type(..))`). Expected `bucket` for all
//!   three cases.
//! `STAGE walk <case>: <key matches|key DIFFERS|failed ..>`: the READER's walk of the self path
//!   in the impl's module gives the bucket key the question asks (`heads_of`: builtin_type for a
//!   primitive). Expected `key matches` for all three.
//! `STAGE candidate <case>: <yes|NO> (filed_from_rows <n>)`: front2 `Buckets::lazy` +
//!   `admit` + `admit_asked(Ask::Trait(Mul, shape))` + `trait_candidates` at `Homes::ANY`
//!   holds the impl. Expected `yes` for all three.
//! `STAGE check <case>: clean|NOT clean: diagnostics .., gaps .., refused ..`: the body's
//!   verdict (the solver's prefilter and the answer included). Expected `clean`.
//!
//! The first stage that says other than expected names where the class is lost. The solver's
//! prefilter (blaze-solve prefilter.rs) is covered by `check`: blaze-typeck infer/facts.rs
//! `fill_impl` puts the impl header's Self first (`terms.push(self_ty)`, then the trait's
//! arguments), so `fingerprint_impl` and `fingerprint_goal` (the goal's args, Self first) line
//! up position by position.

use std::collections::HashMap;

use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_ladder::front2::buckets::{Ask, Buckets, Homes};
use blaze_ladder::front2::keys::{Keys, checker_of};
use blaze_ladder::front2::project::{BodyRef, ProjectWorld};
use blaze_ladder::front2::source::{Load, LoadedExpansion, Log, NamesSource, Records};
use blaze_own::Alloc;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::items_record::buckets::{BucketKey, Head};
use blaze_scope::names::Written;
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, aligned, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::{items_record, names_record};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_typeck::{Shape, UintTy};
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
}
pub mod ops {
    #[lang = \"mul\"]
    pub trait Mul<Rhs = Self> {
        type Output;
        fn mul(self, rhs: Rhs) -> Self::Output;
    }
}
pub mod prelude {
    pub mod v1 {
        pub use crate::marker::Sized;
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

const BIG: &str = "#![no_std]
use core::ops::Mul;
pub struct Big;
pub struct Small;
impl Mul<Big> for u64 {
    type Output = Big;
    fn mul(self, rhs: Big) -> Big {
        rhs
    }
}
impl Mul<Big> for Small {
    type Output = Big;
    fn mul(self, rhs: Big) -> Big {
        rhs
    }
}
";

const APP: &str = "#![no_std]
use core::ops::Mul;
use big::{Big, Small};
pub struct Mine;
impl Mul<Mine> for u16 {
    type Output = Mine;
    fn mul(self, rhs: Mine) -> Mine {
        rhs
    }
}
pub fn prim_foreign() -> Big {
    Mul::mul(3u64, Big)
}
pub fn adt_foreign() -> Big {
    Mul::mul(Small, Big)
}
pub fn prim_root() -> Mine {
    Mul::mul(3u16, Mine)
}
";

/// World indexes: 0 `app` (the owner), 1 `core`, 2 `big`.
const APP_AT: usize = 0;
const CORE_AT: usize = 1;
const BIG_AT: usize = 2;

struct Texts;

impl SourceTexts<'static> for Texts {
    fn read(&self, krate: usize, path: &str) -> Option<&'static str> {
        match (krate, path) {
            (APP_AT, "/app/lib.rs") => Some(APP),
            (CORE_AT, "/core/lib.rs") => Some(CORE),
            (BIG_AT, "/big/lib.rs") => Some(BIG),
            _ => None,
        }
    }
}

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

fn sysroot() -> Sysroot {
    Sysroot { core: Some(CrateId(CORE_AT as u32)), alloc: None, std: None, bstd: None }
}

fn input(name: &'static str, root: &'static str, deps: &'static [(&'static str, usize)]) -> CrateInput<'static> {
    CrateInput {
        name,
        key: CrateItemKey(ContentId::of("blaze-ladder prim self foreign impl probe crate", name.as_bytes())),
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
    let crates = [
        input("app", "/app/lib.rs", &[("core", CORE_AT), ("big", BIG_AT)]),
        input("core", "/core/lib.rs", &[]),
        input("big", "/big/lib.rs", &[("core", CORE_AT)]),
    ];
    let names = resolve_crates(&crates, sysroot(), &Texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    Box::leak(Box::new(names))
}

struct Kept {
    names: Vec<&'static [u8]>,
    items: Vec<&'static [u8]>,
    files: Vec<HashMap<String, &'static str>>,
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

    /// No texts, so no origins.
    fn expansion(&self, _krate: usize, _call: ItemKey) -> Option<LoadedExpansion<'static>> {
        None
    }
}

/// The three crates' records in world order, as `blaze check`'s body job gives them.
fn records() -> &'static Records<'static> {
    let names = names();
    let g = a();
    let mut kept = Kept { names: Vec::new(), items: Vec::new(), files: Vec::new() };
    let mut crate_names: Vec<&'static str> = Vec::new();
    let mut keys: Vec<CrateItemKey> = Vec::new();
    for c in names.crates().iter() {
        kept.names.push(aligned(&names_record::encode(c, g).unwrap_or_else(|e| panic!("the names record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.items.push(aligned(&items_record::encode(c, g).unwrap_or_else(|e| panic!("the items record of `{}`: {e:?}", c.name)), blaze_own::testing::bump()));
        kept.files.push(c.files.iter().map(|f| (f.package.as_str().to_string(), f.text)).collect());
        crate_names.push(c.name);
        keys.push(c.key);
    }
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the records reader's frame")));
    let log: &'static Log<'static> = Box::leak(Box::new(Log::new(Vec::leak(crate_names), g)));
    let load: &'static Kept = Box::leak(Box::new(kept));
    Box::leak(Box::new(Records::new(Vec::leak(keys), load, log, frame, g)))
}

/// One case: the body's name, the impl's crate, its self path as written, the goal's shape
/// (`None`: the ADT's, found by the walk).
struct Case {
    body: &'static str,
    krate: usize,
    self_path: &'static str,
    prim: Option<UintTy>,
}

const CASES: [Case; 3] = [
    Case { body: "prim_foreign", krate: BIG_AT, self_path: "u64", prim: Some(UintTy::U64) },
    Case { body: "adt_foreign", krate: BIG_AT, self_path: "Small", prim: None },
    Case { body: "prim_root", krate: APP_AT, self_path: "u16", prim: Some(UintTy::U16) },
];

#[test]
fn a_primitive_self_impl_outside_core_answers_its_goal() {
    let records = records();
    let g = a();
    let frame: &'static blaze_mem::Frame = Box::leak(Box::new(blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the world's frame")));
    let mut world = ProjectWorld::from_records(records, frame, &[APP_AT, CORE_AT], g).unwrap_or_else(|e| panic!("the world did not start: {e}"));
    let mul = world.front.index.lang("mul").expect("the lang table has `mul`");
    let mut wrong: Vec<String> = Vec::new();
    for case in CASES.iter() {
        // The impl's row: its key and module, from the crate's impl table.
        let mut row = None;
        NamesSource::each_impl_row(records, case.krate, &mut |r| {
            if r.self_path == Some(case.self_path) && r.trait_path.is_some() {
                row = Some(r);
            }
        });
        let row = row.unwrap_or_else(|| panic!("{}: no impl row with self `{}` in crate {}", case.body, case.self_path, case.krate));

        // STAGE walk: the reader's walk of the self path, against the bucket key the question
        // asks (`heads_of`: `builtin_type` for a primitive; the ADT's library key).
        let segments = [case.self_path];
        let walked = NamesSource::walk(records, row.module, Namespace::Types, Written { kind: PathKind::Plain, segments: &segments[..] });
        let (self_key, walk_line) = match (walked, case.prim) {
            (Ok(r), Some(_)) => {
                let want = blaze_scope::builtin_type(case.self_path).expect("a builtin name").key;
                (want, if r.def.key == want && r.consumed == 1 { "key matches".to_string() } else { format!("key DIFFERS: walked {:?} consumed {}", r.def, r.consumed) })
            }
            (Ok(r), None) => (r.def.key, "key matches".to_string()),
            (Err(e), Some(_)) => (blaze_scope::builtin_type(case.self_path).expect("a builtin name").key, format!("failed {e:?}")),
            (Err(e), None) => panic!("{}: the ADT `{}` did not walk: {e:?}", case.body, case.self_path),
        };
        println!("STAGE walk {}: {walk_line}", case.body);
        if walk_line != "key matches" {
            wrong.push(format!("walk {}", case.body));
        }

        // STAGE filed: where the writer of the impl's crate's items record filed it.
        let mut in_bucket = false;
        NamesSource::bucket(records, case.krate, &BucketKey::of_trait(mul, Head::Named(self_key)), &mut |it| in_bucket |= it.key == row.key);
        let mut unfiled = None;
        NamesSource::unfiled(records, case.krate, &mut |r, why| {
            if r.key == row.key {
                unfiled = Some(why);
            }
        });
        let filed = match (in_bucket, unfiled) {
            (true, None) => "bucket".to_string(),
            (false, Some(why)) => format!("unfiled({why:?})"),
            (true, Some(why)) => format!("BOTH bucket and unfiled({why:?})"),
            (false, None) => "NOWHERE".to_string(),
        };
        println!("STAGE filed {}: {filed}", case.body);

        // STAGE candidate: front2's reader files the crate (bucket or unfiled list) and the
        // question's candidates hold the impl.
        let shape = match case.prim {
            Some(u) => Shape::Uint(u),
            None => Shape::Adt(checker_of(self_key)),
        };
        let mut b = Buckets::lazy(records);
        b.admit(records, Homes::ANY);
        let keys = Keys::new();
        b.admit_asked(records, &keys, Homes::ANY, Ask::Trait(checker_of(mul), shape)).unwrap_or_else(|e| panic!("admit `Mul` at {shape:?}: {e}"));
        let candidates = b.trait_candidates(checker_of(mul), shape, Homes::ANY);
        let held = candidates.contains(&row.key);
        println!("STAGE candidate {}: {} (filed_from_rows {}, {} candidates)", case.body, if held { "yes" } else { "NO" }, b.index().filed_from_rows, candidates.len());
        if !held {
            wrong.push(format!("candidate {}", case.body));
        }
    }

    // STAGE check: each case's body verdict over the world.
    let bodies: Vec<_> = world.bodies(g).iter().map(|(b, _)| *b).collect();
    for case in CASES.iter() {
        let body = bodies
            .iter()
            .copied()
            .find(|b| matches!(b, BodyRef::Free(k) if NamesSource::name_of(records, *k) == Some(case.body)))
            .unwrap_or_else(|| panic!("no body `{}`", case.body));
        let v = world.check(body, g);
        if v.is_clean() {
            println!("STAGE check {}: clean", case.body);
        } else {
            let gaps: Vec<String> = v.gaps.iter().map(|gap| format!("{gap}")).collect();
            println!("STAGE check {}: NOT clean: diagnostics {:?}, gaps {gaps:?}, refused {:?}", case.body, v.diagnostics, v.refused.as_ref().map(|r| r.text()));
            wrong.push(format!("check {}", case.body));
        }
    }
    let errors: Vec<String> = records.errors().iter().map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "the records filed {errors:?}");
    assert!(wrong.is_empty(), "stages not as expected: {wrong:?}");
}
