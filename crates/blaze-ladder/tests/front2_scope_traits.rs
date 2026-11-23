//! A trait in scope that declares no item of the called name is never made a candidate, so its
//! refused head cannot stop a method call it has nothing to do with (front2/member_names.rs);
//! and an impl filed under another self shape is never read for a goal (front2/buckets.rs: the
//! bucket key is the impl's self-type head as its header writes it).
//!
//! The evidence: the hello world's run left out `Vec::as_slice`, `len`, `push`, `pop`,
//! `capacity`, `try_reserve`, `into_boxed_slice`, `extend_from_slice`, `extend` (71 stubs and
//! 208 lines of the walk) with "check: Head: Item: Unsupported(\"a const parameter whose type is
//! not an integer, `char` or `bool` (`adt_const_params`)\" ..)": the refused head is core's
//! `pub unsafe trait TransmuteFrom<Src, const ASSUME: Assume = { Assume::NOTHING }>`
//! (library/core/src/mem/transmutability.rs:91), a trait in scope in alloc's vec module
//! (library/alloc/src/vec/mod.rs:108 `use core::mem::{.., TransmuteFrom};`), asked for an item
//! of every called method's name by method resolution (blaze-typeck rules/method.rs
//! `probe_method`), which made its head to read its members.
//!
//! The rule (the Reference, "Method call expressions"): the candidates are "T's inherent methods"
//! and "the methods provided by a visible trait implemented by T"; a trait provides the items
//! its body declares, so a trait in scope with no item named `tr` is no candidate of `x.tr()`.
//!
//! The crate: `far::Odd<const F: &'static str>` (a head the front end refuses today, as
//! `TransmuteFrom`'s) is imported at the root beside `Tr`; `far::R<const F: &'static str, T>`
//! has an impl of `Tr` (a head refused the same way) beside `impl Tr for u8`.
//!
//! The lines it prints (in its failure text; with all well the script prints
//! `test blaze-ladder: exit 0; 1 passed, 0 failed`):
//!
//! ```text
//! method: clean (wanted Clean)          `x.tr()`: `Odd` declares no `tr`; its head is never made
//! bound: clean (wanted Clean); impl heads made: 1     `u8: Tr` reads `impl Tr for u8` only
//! named: refused, holds "adt_const_params" (wanted Refused)    `x.odd()`: `Odd` could answer
//! r_goal: no head, "adt_const_params"   a signature naming `R<"a", u8>`: `R`'s head is refused
//! ```
//!
//! WITHOUT THE FIX (world_items.rs `assoc_item` making every trait in scope's head) `method`
//! fails: "method: Head { .. Item(.., Unsupported(\"a const parameter whose type is not an
//! integer, `char` or `bool` (`adt_const_params`)\" ..)) .. (wanted Clean)". The rows `bound`,
//! `named` and `r_goal` pass with and without it: `bound` pins that a goal reads only its self
//! shape's bucket (one impl head made, not two: the `R` impl's refused head never reached),
//! `named` that a trait that MAY answer is still made and its refusal is the body's by name
//! (never dropped), `r_goal` that `R` itself stays refused until a const parameter of type
//! `&'static str` has a head (stops-3 class B (c), walk-AJ P12). When that lands, `named` says
//! diagnostics (no impl of `Odd` for `u8`) and `r_goal` has a head: change those two rows then.
//!
//! A goal whose self type is not yet known reads no impl at all here: it is ambiguous before
//! any impl is read (blaze-solve term.rs `shape_of_kind`), and a parameter's goal reads only the
//! trait's blanket impls (buckets.rs `fits`), so no row asks for "both".
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_params.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx) -> Result<(), Error> {
    Ok(())
}
mod far {
    pub struct R<const F: &'static str, T>(pub T);
    pub trait Odd<const F: &'static str> {
        fn odd(&self) -> u8;
    }
}
use far::Odd;
trait Tr {
    fn tr(&self) -> u8;
}
impl Tr for u8 {
    fn tr(&self) -> u8 { *self }
}
impl<const F: &'static str, T> Tr for far::R<F, T> {
    fn tr(&self) -> u8 { 0 }
}
fn need<X: Tr>(x: X) -> u8 { x.tr() }
fn method(x: u8) -> u8 { x.tr() }
fn bound(x: u8) -> u8 { need(x) }
fn named(x: u8) -> u8 { x.odd() }
fn r_goal(r: far::R<\"a\", u8>) -> u8 { need(r) }
";

/// What a body must say.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Says {
    Clean,
    /// A program diagnostic holding this text (e.g. `NoMethod` once `Odd`'s head builds).
    Diags(&'static str),
}

/// The bodies, each with what it says and, for `Some(n)`, how many impl heads its check makes
/// (over buckets of its own, so nothing another body made is counted).
const BODIES: [(&str, Says, Option<u32>, &str); 3] = [
    ("bound", Says::Clean, Some(1), "`u8: Tr` reads the `u8` bucket of `Tr`: `impl Tr for u8` only, never `R`'s impl"),
    ("method", Says::Clean, None, "`x.tr()` with `Odd` in scope: `Odd` declares no `tr`, so its head is never read"),
    ("named", Says::Diags("NoMethod"), None, "`x.odd()`: `Odd` declares `odd`, so it is checked and reports no impl for `u8`"),
];

/// The fns whose head must be refused, each with a text its refusal must hold.
const NO_HEAD: [(&str, &str, &str); 1] = [("r_goal", "const argument of a reference", "`R<\"a\", u8>` in a signature: `&'static str` const arg value tree is refused by name")];

#[test]
fn scope_traits_and_buckets_read_only_what_may_answer() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/cen/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "cen", dir: "/cen/", root: "/cen/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let key_of = |name: &str| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"))
    };

    for (name, holds, proves) in NO_HEAD {
        let said = match store.ensure(&names, &mut front.keys, Want::Fn(key_of(name))) {
            Ok(_) => "built".to_string(),
            Err(e) => format!("{e:?}"),
        };
        let _ = proves;
        if said.contains(holds) {
            notes.push(format!("{name}: no head, {holds:?}"));
        } else {
            let brief: String = said.chars().take(240).collect();
            notes.push(format!("{name}: wanted a refusal holding {holds:?}: {brief}"));
            failed.push(name.to_string());
        }
    }

    let mut bodies = Vec::new();
    for (name, want, heads, proves) in BODIES {
        let key = key_of(name);
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            let brief: String = format!("{e:?}").chars().take(240).collect();
            notes.push(format!("{name}: no head: {brief}"));
            failed.push(name.to_string());
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((name, b, want, heads, proves)),
            Err(e) => {
                let brief: String = format!("{e}").chars().take(240).collect();
                notes.push(format!("{name}: does not lower: {brief}"));
                failed.push(name.to_string());
            }
        }
    }
    for (name, body, want, heads, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        // Buckets of this body's own: `lowered` counts the impl heads its questions made.
        let mut buckets = None;
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => {
                let codes: Vec<String> = done.diags.iter().map(|d| format!("{:?}", d.code)).collect();
                format!("diagnostics {codes:?}")
            }
            Err(CheckFail::Diags { diags, .. }) => {
                let codes: Vec<String> = diags.iter().map(|d| format!("{:?}", d.0)).collect();
                format!("diagnostics {codes:?}")
            }
            Err(other @ CheckFail::Head { .. }) => format!("refused {other:?}"),
            Err(other) => format!("{other:?}"),
        };
        let made = buckets.as_ref().map_or(0, |b| b.index().lowered);
        let ok = match want {
            Says::Clean => said == "clean",
            Says::Diags(holds) => said.starts_with("diagnostics") && said.contains(holds),
        };
        let counted = heads.is_none_or(|n| n == made);
        let _ = proves;
        let shown = &said[..said.floor_char_boundary(240.min(said.len()))];
        match heads {
            Some(_) => notes.push(format!("{name}: {shown} (wanted {want:?}); impl heads made: {made}")),
            None => notes.push(format!("{name}: {shown} (wanted {want:?})")),
        }
        if !ok || !counted {
            failed.push(name.to_string());
        }
    }
    assert_eq!(bodies.len(), BODIES.len(), "every body lowered\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "cases that did not say what is stated: {failed:?}\n{}", notes.join("\n"));
}
