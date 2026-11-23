//! A value path through a generic associated type with a const parameter,
//! `<S as Pick>::Rot::<N>::default()`: the shape of tokio's `join!`/`try_join!` (tokio 1.53.3
//! macros/join.rs: `<$rotator_select as $crate::macros::support::RotatorSelect>::Rotator::<{
//! $($total)* }>::default()`, the trait `pub trait RotatorSelect { type Rotator<const COUNT:
//! u32>: Default; }`). Through the real front end with the installed toolchain
//! (`$HOME/.blaze/v2/toolchain`, as tests/front2_macro_block_mods.rs).
//!
//! The Reference, "Associated Items", "Associated Types": "Associated types may include generic
//! parameters and where clauses; these are often referred to as generic associated types";
//! "Paths", "Qualified paths". The arguments on `Rot`'s segment are the associated type's own,
//! after the trait's: `<Normal as Pick>::Rot::<3>` is `Rot<3>` by the impl.
//!
//! Both the literal and the anonymous const take the declared type `u32`. The impl
//! substitutes their values into `Rot<N>`, so changing either count changes the type.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::consts::Consts;
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::lower_made::{LowerTarget, lower_made};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = r#"
fn main() {}
pub trait Pick {
    type Rot<const N: u32>: Default;
}
pub struct Rot<const N: u32> {
    next: u32,
}
impl<const N: u32> Default for Rot<N> {
    fn default() -> Self {
        Rot { next: N }
    }
}
pub struct Normal;
impl Pick for Normal {
    type Rot<const N: u32> = Rot<N>;
}
fn literal() -> u32 { let r = <Normal as Pick>::Rot::<3>::default(); r.next }
fn braced() -> u32 { let r: Rot<2> = <Normal as Pick>::Rot::<{ 0 + 1 + 1 }>::default(); r.next }
fn bad_count() -> u32 { let r: Rot<3> = <Normal as Pick>::Rot::<2>::default(); r.next }
fn bad_braced() -> u32 { let r: Rot<3> = <Normal as Pick>::Rot::<{ 0 + 1 + 1 }>::default(); r.next }
"#;

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("literal", "a literal own argument reaches the impl's value: `Rot<3>`"),
    ("braced", "a braced own argument, as tokio's `Rotator::<{ 0 + 1 + 1 }>` writes it"),
];

enum Outcome {
    Clean,
    Diags(Vec<&'static str>),
    Refused(String),
}

fn run(cases: &[&'static str]) -> Vec<(&'static str, Outcome)> {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/w8gat/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "w8gat", dir: "/w8gat/", root: "/w8gat/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    // A test fixture's allocator, as tests/front2_macro_block_mods.rs takes it.
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command's frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let heap = blaze_own::testing::heap();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut consts = Consts::default();
    let mut out = Vec::new();
    for &name in cases {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|path| path.whole(1))
            .map(|def| def.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"));
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: Some((&mut consts, prepared)) };
        let body = match lower_made(&mut records, prepared, LowerTarget::Free(key)) {
            Ok(b) => b,
            Err(e) => {
                out.push((name, Outcome::Refused(format!("does not lower: {e}"))));
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("body spelling");
        let scope = body.scope(&spell);
        let outcome = match check_body(&mut records, &body, &scope) {
            Ok(done) if done.diags.is_empty() => Outcome::Clean,
            Ok(done) => Outcome::Diags(done.diags.iter().map(|d| d.code.code()).collect()),
            Err(CheckFail::Diags { diags, .. }) => Outcome::Diags(diags.iter().map(|(code, _, _)| code.code()).collect()),
            Err(other) => Outcome::Refused(format!("{other:?}")),
        };
        out.push((name, outcome));
    }
    out
}

/// Both spellings of two yield `Rot<2>`. A fresh inference variable would allow
/// the assignment to `Rot<3>` in the corresponding control.
#[test]
fn a_gat_const_argument_on_a_value_paths_segment_reaches_the_impl() {
    let mut names: Vec<&'static str> = CLEAN.iter().map(|(n, _)| *n).collect();
    names.extend(["bad_count", "bad_braced"]);
    let mut failed = Vec::new();
    for (name, outcome) in run(&names) {
        let proves = CLEAN.iter().find(|(n, _)| *n == name).map(|(_, p)| *p);
        match (proves, outcome) {
            (Some(_), Outcome::Clean) => println!("{name}: clean"),
            (Some(proves), Outcome::Diags(d)) => failed.push(format!("{name}: diagnostics {d:?} ({proves})")),
            (Some(proves), Outcome::Refused(r)) => failed.push(format!("{name}: refused {r} ({proves})")),
            (None, Outcome::Diags(d)) if d.contains(&"E0308") => println!("{name}: E0308"),
            (None, Outcome::Diags(d)) => failed.push(format!("{name}: expected E0308, got {d:?}")),
            (None, Outcome::Clean) => failed.push(format!("{name}: checked clean, so the own argument `2` did not reach the impl")),
            (None, Outcome::Refused(r)) => failed.push(format!("{name}: refused, not a mismatch: {r}")),
        }
    }
    let shown: Vec<String> = failed.iter().map(|l| l.chars().take(400).collect()).collect();
    assert!(failed.is_empty(), "{}", shown.join("\n"));
}
