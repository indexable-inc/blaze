//! attr-order-1: an item's attributes are taken in written order, a derive among them (the
//! Reference, "Attributes" and "Procedural macros"; rustc's `take_first_attr`): a derive runs on
//! the item as it stands when it is reached, and the first attribute macro after it takes the
//! item with every attribute written after it, a later `#[derive]` among them unexpanded.
//!
//! tempo's test-mode check (2026-11-17): `error[unresolved] crates/primitives/src/transaction/
//! tt_signature.rs:133:18 no `add_arbitrary_tests` in `reth_codecs``. `PrimitiveSignature` is
//! `#[derive(.., serde::Serialize, ..)] #[serde(..)] #[cfg_attr(all(test, feature =
//! "reth-codec"), reth_codecs::add_arbitrary_tests(..))] #[cfg_attr(.., derive(arbitrary::
//! Arbitrary))]`: a derive after the attribute macro. blaze sent all of the item's derives at
//! once with every name after the first (`after_derive_takers`), so the macro went as
//! `CallRole::AfterDeriveBeforeDerive` and the cells refused it `DeriveAfterAttrMacro`, which
//! blaze-cli printed as "no X in Y". Now the lowering sends one stage a fragment: the derives
//! before the next name, the names up to the next derive, the keep call, whose own fragment goes
//! on; a later derive is sent only there, or reaches the macro in its input.
//!
//! Each test resolves three crates: `user`, `codecs` (`pub use codecs_derive::*;`, reth-codecs'
//! re-export) and the proc-macro crate `codecs_derive`, whose runner counts what it ran and
//! what each attribute macro's input held.

use core::sync::atomic::{AtomicU32, Ordering};
use std::string::String;
use std::vec::Vec;

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenKind, TokenOrigin, TokenStream, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_mem::{Root, RootOpts};
use blaze_own::{AVec, Alloc};
use blaze_surface::{CallRole, Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{CrateInput, CrateNames, Names, SourceTexts, resolve_crates};
use crate::task::{AttrOrder, Serial};
use crate::{CallShape, ScopeError, Unresolved};

/// The test's bump arena, made at its top; the names resolved borrow it.
struct TestRoot(blaze_own::Erased<blaze_mem::Frame>);

impl TestRoot {
    fn alloc(&self) -> Alloc<'_> {
        self.0.handle()
    }

    fn bump(&self) -> blaze_own::Bump<'_> {
        self.0.bump()
    }
}

fn root() -> TestRoot {
    TestRoot(blaze_own::Erased::new(blaze_mem::Frame::new(blaze_mem::Tag::Nameres).unwrap_or_else(|e| panic!("the test's frame: {e:?}"))))
}

/// reth-codecs-derive in miniature: the attribute macros `add_tests` (`add_arbitrary_tests`) and
/// `mark`, the derive `Arb` (arbitrary's), and `Ser`, a derive with the helper `serde`.
const DERIVE: &str = "#![no_core]
#[proc_macro_attribute]
pub fn add_tests(args: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_attribute]
pub fn mark(args: TokenStream, item: TokenStream) -> TokenStream { item }
#[proc_macro_derive(Arb)]
pub fn arb(item: TokenStream) -> TokenStream { item }
#[proc_macro_derive(Ser, attributes(serde))]
pub fn ser(item: TokenStream) -> TokenStream { item }
";

/// reth-codecs: the derive crate's macros re-exported by a glob.
const CODECS: &str = "#![no_core]
pub use codecs_derive::*;
";

/// The user crate's cfg: a test build with the feature `x` (tempo's `reth-codec`).
const USER_CFG: &[(&str, Option<&str>)] = &[("feature", Some("x")), ("test", None)];

const RUNNER: CrateItemKey = CrateItemKey(ContentId([11; 32]));

/// One row: the user crate's text, the order the lowering sends a derived item's attributes in,
/// what each attribute macro returns, and what the runner saw.
struct World {
    user: &'static str,
    order: AttrOrder,
    tests_out: &'static str,
    mark_out: &'static str,
    tests: AtomicU32,
    mark: AtomicU32,
    arb: AtomicU32,
    ser: AtomicU32,
    /// `add_tests` runs whose input held `Arb` (a derive after the macro, unexpanded).
    tests_saw_arb: AtomicU32,
    /// `add_tests` runs whose input held `Ser` (a derive before the macro, which must have run).
    tests_saw_ser: AtomicU32,
    /// `mark` runs whose input held `Arb`.
    mark_saw_arb: AtomicU32,
}

impl World {
    fn new(user: &'static str, order: AttrOrder, tests_out: &'static str, mark_out: &'static str) -> World {
        let zero = || AtomicU32::new(0);
        World {
            user,
            order,
            tests_out,
            mark_out,
            tests: zero(),
            mark: zero(),
            arb: zero(),
            ser: zero(),
            tests_saw_arb: zero(),
            tests_saw_ser: zero(),
            mark_saw_arb: zero(),
        }
    }

    fn get(n: &AtomicU32) -> u32 {
        n.load(Ordering::Relaxed)
    }
}

/// Whether `stream` holds the identifier `name`.
fn has_ident(stream: &TokenStream<'_>, name: &str) -> bool {
    stream.tokens().iter().any(|t| matches!(t.kind, TokenKind::Ident { .. }) && stream.text_of(t) == name)
}

impl<'a> SourceTexts<'a> for World {
    fn read(&self, krate: usize, path: &str) -> Option<&'a str> {
        match (krate, path) {
            (0, "/user/lib.rs") => Some(self.user),
            (1, "/codecs/lib.rs") => Some(CODECS),
            (2, "/codecs_derive/lib.rs") => Some(DERIVE),
            _ => None,
        }
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == 2).then_some(RUNNER)
    }

    fn attr_order(&self) -> AttrOrder {
        self.order
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        let bump = |n: &AtomicU32, yes: bool| {
            if yes {
                n.fetch_add(1, Ordering::Relaxed);
            }
        };
        let text = match (call.kind, def.proc_name()) {
            (ProcKind::Derive, Some("Arb")) => {
                bump(&self.arb, true);
                ""
            }
            (ProcKind::Derive, Some("Ser")) => {
                bump(&self.ser, true);
                ""
            }
            (ProcKind::Attr, Some("add_tests")) => {
                bump(&self.tests, true);
                bump(&self.tests_saw_arb, has_ident(call.input, "Arb"));
                bump(&self.tests_saw_ser, has_ident(call.input, "Ser"));
                self.tests_out
            }
            (ProcKind::Attr, Some("mark")) => {
                bump(&self.mark, true);
                bump(&self.mark_saw_arb, has_ident(call.input, "Arb"));
                self.mark_out
            }
            _ => return Err(refused),
        };
        let tokens = lex(text, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

/// The three crates of `world` resolved into `root`.
fn solve<'r>(root: &'r TestRoot, world: &World) -> Names<'r> {
    let derive = input("codecs_derive", "/codecs_derive/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [
        CrateInput { cfg: USER_CFG, ..input("user", "/user/lib.rs", &[("codecs", 1)]) },
        input("codecs", "/codecs/lib.rs", &[("codecs_derive", 2)]),
        CrateInput { implicit, ..derive },
    ];
    resolve_crates(&crates, Sysroot::default(), world, &Serial, root.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

/// The user root's macro errors, as (path, why), sorted.
fn macro_errors(names: &CrateNames<'_>) -> Vec<(String, Unresolved)> {
    let errors = &names.done.get(&names.root).expect("the root is done").scope.errors;
    let mut out: Vec<(String, Unresolved)> = errors
        .iter()
        .filter_map(|e| match e {
            ScopeError::Macro { path, why, .. } => Some((path.segments.join("::"), *why)),
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Asserts the user root has no macro error.
fn no_macro_errors(names: &Names<'_>) {
    let errors = macro_errors(names.krate(0));
    assert!(errors.is_empty(), "no macro error: {errors:?}; notes {:?}", names.notes(0));
}

/// The roles of the user root's attribute calls of `path`, in the order sent.
fn roles(names: &Names<'_>, path: &str) -> Vec<CallRole> {
    let user = names.krate(0);
    let decls = &user.decls.get(&user.root).expect("the root is lowered").1;
    decls.calls.iter().filter(|c| c.kind == CallShape::Attr && c.path.segments.join("::") == path).map(|c| c.role).collect()
}

/// Whether the type `name` is declared in the user root, the lookup's scratch in `alloc`.
fn declared(names: &Names<'_>, name: &str, alloc: Alloc<'_>) -> bool {
    names.item_at(names.root(0), Namespace::Types, Written { kind: PathKind::Plain, segments: &[name] }, alloc).is_ok()
}

/// The brief's miniature: `add_tests` in a compound `cfg_attr`, then `derive(Arb)` in another.
const MINIATURE: &str = "#![no_core]
#[cfg_attr(all(test, feature = \"x\"), codecs::add_tests(a, b))]
#[cfg_attr(any(test, feature = \"arb\"), derive(codecs::Arb))]
pub enum E { A }
";

/// Rule: under `cfg(test)` and feature `x`, `codecs::add_tests` is the item's first attribute,
/// so it is the item's one call (`CallRole::Call`), queued before any derive, its input still
/// carrying `#[derive(codecs::Arb)]`; `Arb` runs once, in the macro's output.
///
/// Expected: "1 passed". No macro error; `add_tests` ran once with `Arb` in its input; `Arb`
/// ran once; `e_tests` is declared. (This shape resolved before attr-order-1 too: no derive
/// precedes the macro, so the item never went as after-derive calls. The failing shape is
/// `tempo_shape_with_a_derive_on_each_side`.)
#[test]
fn an_attribute_macro_in_a_cfg_attr_takes_the_item_with_its_later_derive() {
    let root = root();
    let world = World::new(MINIATURE, AttrOrder::Written, "#[derive(codecs::Arb)] pub enum E { A } pub mod e_tests {}", "");
    let names = solve(&root, &world);
    no_macro_errors(&names);
    assert_eq!(roles(&names, "codecs::add_tests"), [CallRole::Call], "the macro is the item's one call");
    assert_eq!((World::get(&world.tests), World::get(&world.tests_saw_arb)), (1, 1), "`add_tests` ran once, `derive(Arb)` in its input");
    assert_eq!(World::get(&world.arb), 1, "`Arb` ran once, in the macro's output, never on the item as written");
    assert!(declared(&names, "e_tests", root.alloc()), "the macro's output is declared");
}

/// `PrimitiveSignature`'s shape: a derive with a helper, the helper, the macro in a multi-line
/// compound `cfg_attr`, then a derive.
const TEMPO: &str = "#![no_core]
#[derive(codecs::Ser)]
#[serde(x)]
#[cfg_attr(
    all(test, feature = \"x\"),
    codecs::add_tests(a, b)
)]
#[derive(codecs::Arb)]
pub struct P { f: u8 }
";

/// What `add_tests` gives back for [`TEMPO`]: the item as its input holds it (`Ser`'s derive
/// gone, the helper and the later derive kept), and a test module.
const TEMPO_OUT: &str = "#[serde(x)] #[derive(codecs::Arb)] pub struct P { f: u8 } pub mod p_tests {}";

/// Rule: `Ser` runs first, on the item as written; then `add_tests` takes the item as an
/// after-derive call (`CallRole::AfterDerive`, never `AfterDeriveBeforeDerive`) with `serde(x)`
/// and `derive(Arb)` still on it; in its output `serde` is `Ser`'s helper through the parent
/// expansion, inert, and `Arb` runs there.
///
/// Expected: "1 passed". No macro error; `Ser` ran once; `add_tests` ran once with `Arb` and
/// without `Ser` in its input; `Arb` ran once; `P` and `p_tests` are declared. The control is
/// `the_old_order_refuses_the_tempo_shape`.
#[test]
fn tempo_shape_with_a_derive_on_each_side() {
    let root = root();
    let world = World::new(TEMPO, AttrOrder::Written, TEMPO_OUT, "");
    let names = solve(&root, &world);
    no_macro_errors(&names);
    assert_eq!(roles(&names, "codecs::add_tests"), [CallRole::AfterDerive], "the macro goes after `Ser`, with no derive sent after it");
    assert_eq!(World::get(&world.ser), 1, "`Ser` ran once");
    assert_eq!(
        (World::get(&world.tests), World::get(&world.tests_saw_arb), World::get(&world.tests_saw_ser)),
        (1, 1, 0),
        "`add_tests` ran once, `derive(Arb)` in its input and `derive(Ser)` not"
    );
    assert_eq!(World::get(&world.arb), 1, "`Arb` ran once, in the macro's output");
    assert!(declared(&names, "P", root.alloc()), "`P` is declared");
    assert!(declared(&names, "p_tests", root.alloc()), "the macro's test module is declared");
}

/// Control (rule 23): the same crate with the old order planted (`AttrOrder::DerivesAtOnce`,
/// every derive sent at once with the names after the first) refuses the macro as tempo's
/// check did, and the macro never runs.
///
/// Expected: "1 passed": the errors are exactly `[("codecs::add_tests", DeriveAfterAttrMacro)]`.
#[test]
fn the_old_order_refuses_the_tempo_shape() {
    let root = root();
    let world = World::new(TEMPO, AttrOrder::DerivesAtOnce, TEMPO_OUT, "");
    let names = solve(&root, &world);
    let user = names.krate(0);
    assert_eq!(roles(&names, "codecs::add_tests"), [CallRole::AfterDeriveBeforeDerive], "the planted order sends the later derive with the macro");
    assert_eq!(macro_errors(user), [(String::from("codecs::add_tests"), Unresolved::DeriveAfterAttrMacro)], "notes {:?}", names.notes(0));
    assert_eq!(World::get(&world.tests), 0, "the refused macro never ran");
}

/// Rule: a derive, then an attribute macro: the derive runs on the item as written, then the
/// macro takes the item without the derive.
///
/// Expected: "1 passed". No macro error; `add_tests` is `AfterDerive`; `Arb` ran once (on the
/// item as written); `add_tests` ran once without `Arb` in its input; `d_tests` is declared.
#[test]
fn a_derive_then_an_attribute_macro() {
    let root = root();
    let user_text = "#![no_core]\n#[derive(codecs::Arb)]\n#[codecs::add_tests(a)]\npub struct D;\n";
    let world = World::new(user_text, AttrOrder::Written, "pub struct D; pub mod d_tests {}", "");
    let names = solve(&root, &world);
    no_macro_errors(&names);
    assert_eq!(roles(&names, "codecs::add_tests"), [CallRole::AfterDerive]);
    assert_eq!(World::get(&world.arb), 1, "`Arb` ran once, on the item as written");
    assert_eq!((World::get(&world.tests), World::get(&world.tests_saw_arb)), (1, 0), "`add_tests` ran once, the derive cut from its input");
    assert!(declared(&names, "d_tests", root.alloc()));
}

/// Rule: two attribute macros between two derives: `Ser` runs, then the first macro takes the
/// item with the second and `derive(Arb)` on it; the second runs once, in the first's output,
/// and `Arb` once, in the second's.
///
/// Expected: "1 passed". No macro error; both macros are `AfterDerive` in the root (one stage);
/// `Ser` 1, `add_tests` 1 with `Arb` in its input, `mark` 1 (not 2: the root's `mark` call is
/// passed over, its item taken) with `Arb` in its input, `Arb` 1; `t_tests` and `t_marked` are
/// declared.
#[test]
fn two_attribute_macros_between_two_derives() {
    let root = root();
    let user_text = "#![no_core]
#[derive(codecs::Ser)]
#[codecs::add_tests(a)]
#[codecs::mark]
#[derive(codecs::Arb)]
pub struct T;
";
    let tests_out = "#[codecs::mark] #[derive(codecs::Arb)] pub struct T; pub mod t_tests {}";
    let mark_out = "#[derive(codecs::Arb)] pub struct T; pub mod t_marked {}";
    let world = World::new(user_text, AttrOrder::Written, tests_out, mark_out);
    let names = solve(&root, &world);
    no_macro_errors(&names);
    assert_eq!(roles(&names, "codecs::add_tests"), [CallRole::AfterDerive]);
    assert_eq!(roles(&names, "codecs::mark"), [CallRole::AfterDerive], "one stage: both names after `Ser`, before `Arb`");
    assert_eq!(World::get(&world.ser), 1);
    assert_eq!((World::get(&world.tests), World::get(&world.tests_saw_arb)), (1, 1));
    assert_eq!((World::get(&world.mark), World::get(&world.mark_saw_arb)), (1, 1), "`mark` ran once, in `add_tests`' output");
    assert_eq!(World::get(&world.arb), 1, "`Arb` ran once, in `mark`'s output");
    assert!(declared(&names, "t_tests", root.alloc()));
    assert!(declared(&names, "t_marked", root.alloc()));
}

/// Rule: one `cfg_attr` holding a derive and an attribute macro is replaced by both in order:
/// the derive runs first, then the macro takes the item with the later `derive(Arb)`.
///
/// Expected: "1 passed". No macro error; `add_tests` is `AfterDerive`; `Ser` 1; `add_tests` 1
/// with `Arb` and without `Ser` in its input; `Arb` 1; `c_tests` is declared.
#[test]
fn a_cfg_attr_holding_a_derive_and_an_attribute_macro() {
    let root = root();
    let user_text = "#![no_core]
#[cfg_attr(test, derive(codecs::Ser), codecs::add_tests(a))]
#[derive(codecs::Arb)]
pub struct C;
";
    let world = World::new(user_text, AttrOrder::Written, "#[derive(codecs::Arb)] pub struct C; pub mod c_tests {}", "");
    let names = solve(&root, &world);
    no_macro_errors(&names);
    assert_eq!(roles(&names, "codecs::add_tests"), [CallRole::AfterDerive]);
    assert_eq!(World::get(&world.ser), 1);
    assert_eq!(
        (World::get(&world.tests), World::get(&world.tests_saw_arb), World::get(&world.tests_saw_ser)),
        (1, 1, 0),
        "`add_tests` ran once, `derive(Arb)` in its input and `derive(Ser)` not"
    );
    assert_eq!(World::get(&world.arb), 1);
    assert!(declared(&names, "c_tests", root.alloc()));
}
