//! A constant's body checked twice in one run reads the same answers (front2 body_reads.rs
//! `Digests::file`; rule 21), whatever was evaluated before it.
//!
//! The shape is jiff's and jiff-core's bounds (tempo's census, "LEFT 19 constant: .. checking
//! .. again read other answers than its first check: two read-list digests under one body",
//! e.g. `<impl DurationUnits>::to_span::LIMIT`): a trait `Bounds` with constants typed by its
//! own associated type, implemented for unit structs whose inherent impls mirror the trait's
//! constants under the same names, a trait impl's constant that reads `Self::MAX` (the
//! inherent constant: The Reference, "Paths", inherent items first), and body-local constants
//! that read the inherent ones.
//!
//! Each order evaluates the constants over ONE store with a fresh evaluator per constant, as
//! the walk and the program's later evaluations do: every body is checked again over a store
//! the earlier evaluations filled, and each check must file the digest the first filed.
//!
//! The cause it guards (blaze-typeck rules/writeback.rs): writeback selects the impl of each
//! projection over the body's reads, after the walk. `ImplsOf(Bounds, Param)` was a hole there
//! on a fresh store, answered by its stand-in and left out of the read list, while a store an
//! earlier evaluation's instance selection had filled answered it: 39 rows, then 40 (two
//! read-list digests under `<impl SpanYears>::MAX`). A hole in writeback is now a need of the
//! run. The control, that two different lists under one body are still refused, is
//! `body_reads::tests::reads_differ_names_the_rows`.
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_consts.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, Records};
use blaze_ladder::front2::consts::{Consts, int_of};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "pub trait Bounds {
    const MIN: Self::Primitive;
    const MAX: Self::Primitive;
    type Primitive;
}
pub struct Year(());
impl Bounds for Year {
    const MIN: Self::Primitive = -9999;
    const MAX: Self::Primitive = 9999;
    type Primitive = i16;
}
impl Year {
    pub const MIN: i16 = <Year as Bounds>::MIN;
    pub const MAX: i16 = <Year as Bounds>::MAX;
    pub const LEN: i128 = Self::MAX as i128 - Self::MIN as i128 + 1;
}
pub struct SpanYears(());
impl Bounds for SpanYears {
    const MIN: Self::Primitive = -Self::MAX;
    const MAX: Self::Primitive = (Year::LEN - 1) as i16;
    type Primitive = i16;
}
impl SpanYears {
    pub const MIN: i16 = <SpanYears as Bounds>::MIN;
    pub const MAX: i16 = <SpanYears as Bounds>::MAX;
}
pub const LIMIT: u64 = SpanYears::MAX as u64;
pub const FLOOR: i64 = SpanYears::MIN as i64;
pub const YEARS: i128 = Year::LEN;
fn main() {}
";

#[test]
fn a_constant_body_reads_the_same_in_any_order() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/bounds/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "bounds", dir: "/bounds/", root: "/bounds/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let root = names.root(0);
    let lib = |name: &'static str| {
        names::path(&names, root, Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let mut keys = Vec::new();
    for name in ["LIMIT", "FLOOR", "YEARS"] {
        keys.push((name, front.keys.item(lib(name)).expect("a constant's key")));
    }
    let want = |name: &str| -> u128 {
        match name {
            "LIMIT" => 19998,
            "FLOOR" => (-19998i64) as u64 as u128,
            _ => 19999,
        }
    };
    let orders: [[usize; 3]; 3] = [[0, 1, 2], [2, 1, 0], [1, 2, 0]];
    let mut failed = Vec::new();
    for order in orders {
        let mut store = Store::new(front.index.lang("sized"), front.frame, Alloc::global());
        let mut buckets = None;
        for at in order {
            let (name, key) = keys[at];
            let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
            let mut consts = Consts::default();
            let got = consts.value(&mut records, prepared, key).map(|r| int_of(r));
            println!("order {order:?}: {name}: {got:?}");
            match got {
                Ok(Some(v)) if v & u64::MAX as u128 == want(name) & u64::MAX as u128 => {}
                Ok(other) => failed.push(format!("order {order:?}: {name} = {other:?}, want {}", want(name))),
                Err(e) => failed.push(format!("order {order:?}: {name}: {e}")),
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    println!("const reads once: every constant evaluated in every order, one digest a body");
}
