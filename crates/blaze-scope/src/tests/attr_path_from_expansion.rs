//! RX6: an attribute call on an item a `macro_rules!` body wrote is found among the item's
//! attributes by its path's words (blaze-expand `same_path`), the same way as on an item of a
//! file's text.
//!
//! tempo, 2026-11-17, 27 calls in 8 files: `error[macro-call-failed] .. the attribute call of
//! `crate:: linkme:: distributed_slice` failed: it is not among the item's attributes as the
//! expander walks them`. A registration macro writes `#[$crate::linkme::distributed_slice(..)]
//! static X: .. = ..;` (linkme's README form behind a `$crate` path). The lowering reads the
//! expansion's rendered text, where `$crate` is written `crate` and every token spaced; the item
//! walk reads the expansion's tokens, where the transcriber's `$crate` is the one identifier
//! `$crate`. The walk compared the two as text with the spaces taken out (resolve.rs
//! `same_path`, now `same_spelling`, the planted bug): `crate::..` is not `$crate::..`, and the
//! call was refused with `ExpandNote::AttrNotFound`.
//!
//! The Reference, "Macros By Example", "Hygiene": `$crate` names the crate defining the macro,
//! wherever its tokens end up; "Attribute macros": the macro receives the item without the
//! attribute and with the other attributes.

use core::sync::atomic::{AtomicU32, Ordering};

use blaze_expand::{HostAsk, HostError, HostWhy, MacroDefinition, ProcCall, ProcKind, Produced, Reads, TokenKind, TokenOrigin, Transparency, lex};
use blaze_id::{ContentId, CrateItemKey};
use blaze_mem::{Root, RootOpts};
use blaze_own::{AVec, Alloc};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{Implicit, Prelude, StdIs};

use super::entry::input;
use crate::graph::Sysroot;
use crate::names::Written;
use crate::resolve::{AttrPlant, CrateInput, ExpandNote, Names, NamesNote, SourceTexts, resolve_crates};
use crate::task::Serial;

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

/// The proc-macro crate: one attribute macro, `mark`.
const MACROS: &str = "#![no_core]
#[proc_macro_attribute]
pub fn mark(args: TokenStream, item: TokenStream) -> TokenStream { item }
";

/// The crate whose macros write `#[$crate::attrs::mark]` items (antithesis sdk's shape, its
/// `linkme` re-export here `attrs`), calling one of them itself (a), and the plain source
/// forms (e).
const SDK: &str = "#![no_core]
pub mod attrs { pub use macros::mark; }
#[macro_export]
macro_rules! register { ($n:ident) => { #[$crate::attrs::mark] pub static $n: u8 = 0; } }
#[macro_export]
macro_rules! register_doc_first { ($n:ident) => { #[doc = \"registered\"] #[$crate::attrs::mark] pub static $n: u8 = 0; } }
#[macro_export]
macro_rules! register_mark_first { ($n:ident) => { #[$crate::attrs::mark] #[doc = \"registered\"] pub static $n: u8 = 0; } }
#[macro_export]
macro_rules! register_in_const { ($n:ident) => { const _: () = { #[$crate::attrs::mark] static $n: u8 = 0; }; } }
register!(A);
#[attrs::mark]
pub static P: u8 = 0;
#[crate::attrs::mark]
pub static Q: u8 = 0;
";

/// Another crate calling the sdk's macros: `$crate` there names the sdk, not the caller (b, c, d).
const USER: &str = "#![no_core]
sdk::register!(B);
sdk::register_doc_first!(C);
sdk::register_mark_first!(D);
sdk::register_in_const!(E);
";

const USER_CRATE: usize = 0;
const SDK_CRATE: usize = 1;
const MACROS_CRATE: usize = 2;

const RUNNER: CrateItemKey = CrateItemKey(ContentId([11; 32]));

/// Per static `mark` is called on: its name, what `mark` returns for it, and whether its input
/// must still hold a `#[doc]` (the other attribute stays on the item).
const MARKED: [(&str, &str, bool); 7] = [
    ("A", "pub static A_MARKED: u8 = 0;", false),
    ("B", "pub static B_MARKED: u8 = 0;", false),
    ("C", "pub static C_MARKED: u8 = 0;", true),
    ("D", "pub static D_MARKED: u8 = 0;", true),
    ("E", "static E_MARKED: u8 = 0;", false),
    ("P", "pub static P_MARKED: u8 = 0;", false),
    ("Q", "pub static Q_MARKED: u8 = 0;", false),
];

/// The texts, the runner of `mark`, and what it saw: per [`MARKED`] row `k`, bit `k` of `ran`
/// when `mark` ran on that static, and bit `k` of `clean` when its input held no `mark` and,
/// where the row says so, the `#[doc]`.
struct World {
    plant: AttrPlant,
    ran: AtomicU32,
    clean: AtomicU32,
}

const FILES: [&[(&str, &str)]; 3] = [&[("/user/lib.rs", USER)], &[("/sdk/lib.rs", SDK)], &[("/macros/lib.rs", MACROS)]];

impl<'r> SourceTexts<'r> for World {
    fn read(&self, krate: usize, path: &str) -> Option<&'r str> {
        FILES.get(krate)?.iter().find(|(p, _)| *p == path).map(|(_, t)| *t)
    }

    fn proc_macro_key(&self, krate: usize) -> Option<CrateItemKey> {
        (krate == MACROS_CRATE).then_some(RUNNER)
    }

    fn attr_plant(&self) -> AttrPlant {
        self.plant
    }

    fn run_proc_macro<'b>(&self, _: usize, def: &MacroDefinition<'_>, call: ProcCall<'_>, alloc: Alloc<'b>) -> Result<Produced<'b>, HostError> {
        let refused = HostError { ask: HostAsk::ProcMacro, why: HostWhy::Refused };
        if !(call.kind == ProcKind::Attr && def.proc_name() == Some("mark")) {
            return Err(refused);
        }
        let item = call.input;
        let ident = |at: usize| matches!(item.tokens().get(at).map(|t| t.kind), Some(TokenKind::Ident { .. }));
        let words = || (0..item.len()).filter(|&at| ident(at)).map(|at| item.text_at(at));
        // The static's name: the identifier after `static` (and `mut`).
        let name = (0..item.len())
            .find(|&at| ident(at) && item.text_at(at) == "static")
            .and_then(|at| (at + 1..item.len()).find(|&n| ident(n) && item.text_at(n) != "mut"))
            .map(|at| item.text_at(at))
            .ok_or(refused)?;
        let (k, &(_, out, doc)) = MARKED.iter().enumerate().find(|(_, row)| row.0 == name).ok_or(refused)?;
        self.ran.fetch_or(1 << k, Ordering::Relaxed);
        if !words().any(|w| w == "mark") && (!doc || words().any(|w| w == "doc")) {
            self.clean.fetch_or(1 << k, Ordering::Relaxed);
        }
        let tokens = lex(out, Edition::E2021, alloc).map_err(|_| refused)?;
        let mut origins = AVec::new_in(alloc);
        origins.extend(tokens.tokens().iter().map(|_| TokenOrigin::Made(Transparency::Transparent)));
        Ok(Produced { tokens, origins, reads: Reads::new_in(alloc), diagnostics: AVec::new_in(alloc), text: AVec::new_in(alloc) })
    }
}

/// The three crates resolved under `plant` into `root`, and the world that ran `mark`.
fn solve(root: &TestRoot, plant: AttrPlant) -> (Names<'_>, World) {
    let world = World { plant, ran: AtomicU32::new(0), clean: AtomicU32::new(0) };
    let macros = input("macros", "/macros/lib.rs", &[]);
    let implicit = Implicit { std: StdIs::Rust, prelude: Prelude::Rust(edition::Edition::Edition2021), proc_macro: true, entry: false };
    let crates = [input("user", "/user/lib.rs", &[("sdk", SDK_CRATE)]), input("sdk", "/sdk/lib.rs", &[("macros", MACROS_CRATE)]), CrateInput { implicit, ..macros }];
    let names = resolve_crates(&crates, Sysroot::default(), &world, &Serial, root.bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    (names, world)
}

/// Whether `name` is declared in the value namespace of `krate`'s root, the lookup's scratch in
/// `alloc`.
fn declared(names: &Names<'_>, krate: usize, name: &str, alloc: Alloc<'_>) -> bool {
    names.item_at(names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, alloc).is_ok()
}

/// The names of the attribute calls of `krate` refused as not among their item's attributes.
fn not_found<'n, 'r>(names: &'n Names<'r>, krate: usize) -> impl Iterator<Item = &'r str> + 'n {
    names.notes(krate).iter().filter_map(|note| match note {
        NamesNote::Expand { note: ExpandNote::AttrNotFound { name }, .. } => Some(*name),
        _ => None,
    })
}

/// The bit of the [`MARKED`] row named `name`.
fn bit(name: &str) -> u32 {
    let k = MARKED.iter().position(|row| row.0 == name).unwrap_or_else(|| panic!("no row `{name}`"));
    1 << k
}

/// Rule (a): `register!(A)` called in the crate defining it writes `#[$crate::attrs::mark] pub
/// static A`; the call is found among the item's attributes and run, and its output replaces
/// the item: `A_MARKED` is declared in the sdk's root, `A` is not.
///
/// Expected: passes. Before RX6 the sdk's notes held `AttrNotFound { name: "crate:: attrs::
/// mark" }` and `A_MARKED` was never declared (the control is
/// [`the_planted_spelling_match_refuses_the_macro_written_attribute`]).
#[test]
fn a_dollar_crate_attribute_a_macro_writes_in_its_own_crate_is_found_and_run() {
    let root = root();
    let (names, world) = solve(&root, AttrPlant::None);
    assert_eq!(not_found(&names, SDK_CRATE).count(), 0, "no attribute call of the sdk is refused: notes {:?}", names.notes(SDK_CRATE));
    assert_ne!(world.ran.load(Ordering::Relaxed) & bit("A"), 0, "`mark` ran on `A`: notes {:?}", names.notes(SDK_CRATE));
    assert_ne!(world.clean.load(Ordering::Relaxed) & bit("A"), 0, "`mark`'s input is `A` without its attribute");
    assert!(declared(&names, SDK_CRATE, "A_MARKED", root.alloc()), "the output is declared: notes {:?}", names.notes(SDK_CRATE));
    assert!(!declared(&names, SDK_CRATE, "A", root.alloc()), "the output replaces the item");
}

/// Rule (b): the same macro called from another crate: `$crate` names the sdk (the defining
/// crate), so `$crate::attrs::mark` is the sdk's re-export of `mark`; the call is found and
/// run, and `B_MARKED` is declared in the calling crate's root in place of `B`.
#[test]
fn a_dollar_crate_attribute_written_for_another_crate_is_found_and_run() {
    let root = root();
    let (names, world) = solve(&root, AttrPlant::None);
    assert_eq!(not_found(&names, USER_CRATE).count(), 0, "no attribute call of the user is refused: notes {:?}", names.notes(USER_CRATE));
    assert_ne!(world.ran.load(Ordering::Relaxed) & bit("B"), 0, "`mark` ran on `B`: notes {:?}", names.notes(USER_CRATE));
    assert!(declared(&names, USER_CRATE, "B_MARKED", root.alloc()), "the output is declared where the call stands: notes {:?}", names.notes(USER_CRATE));
    assert!(!declared(&names, USER_CRATE, "B", root.alloc()), "the output replaces the item");
}

/// Rule (c): with a `#[doc = ..]` written before the macro-written `#[$crate::attrs::mark]`, and
/// after it, the call is the `mark` attribute: `mark` runs on `C` and on `D`, its input holds the
/// `#[doc]` and no `mark`, and both outputs are declared.
#[test]
fn the_macro_written_attribute_is_found_among_two_in_either_order() {
    let root = root();
    let (names, world) = solve(&root, AttrPlant::None);
    let (ran, clean) = (world.ran.load(Ordering::Relaxed), world.clean.load(Ordering::Relaxed));
    for name in ["C", "D"] {
        assert_ne!(ran & bit(name), 0, "`mark` ran on `{name}`: notes {:?}", names.notes(USER_CRATE));
        assert_ne!(clean & bit(name), 0, "`{name}`'s input keeps the `#[doc]` and loses `mark`");
    }
    assert!(declared(&names, USER_CRATE, "C_MARKED", root.alloc()), "`C`'s output is declared (doc first)");
    assert!(declared(&names, USER_CRATE, "D_MARKED", root.alloc()), "`D`'s output is declared (mark first)");
}

/// Rule (d): the macro writes the static inside `const _: () = { .. };`: the attribute call on
/// it is found and run too, and no call of the user crate is refused as not found.
///
/// UNSURE (RX6): an item in a `const` block's body may be the body layer's (blaze-lower) and
/// not this solve's; if `mark` never runs on `E` here, the walk is not the cause and this test
/// says so by its first assertion.
#[test]
#[ignore = "OPEN (2026-11-17): no attribute call is refused, but `mark` never ran on `E`: a static a macro writes inside `const _: () = { .. };` is the body layer's item (blaze-lower driver.rs resolves an attribute there by text plus a mark), not this resolve's: the next piece"]
fn the_macro_written_attribute_in_a_const_block_is_found_and_run() {
    let root = root();
    let (names, world) = solve(&root, AttrPlant::None);
    assert_eq!(not_found(&names, USER_CRATE).count(), 0, "no attribute call of the user is refused: notes {:?}", names.notes(USER_CRATE));
    assert_ne!(world.ran.load(Ordering::Relaxed) & bit("E"), 0, "`mark` ran on `E` in the const block: notes {:?}", names.notes(USER_CRATE));
    assert_ne!(world.clean.load(Ordering::Relaxed) & bit("E"), 0, "`E`'s input lost its attribute");
}

/// Rule (e): the plain source forms, `#[attrs::mark]` and `#[crate::attrs::mark]` written in the
/// sdk's file, are found and run as before.
#[test]
fn the_source_forms_are_found_and_run() {
    let root = root();
    let (names, world) = solve(&root, AttrPlant::None);
    let ran = world.ran.load(Ordering::Relaxed);
    for name in ["P", "Q"] {
        assert_ne!(ran & bit(name), 0, "`mark` ran on `{name}`: notes {:?}", names.notes(SDK_CRATE));
    }
    assert!(declared(&names, SDK_CRATE, "P_MARKED", root.alloc()), "`P`'s output is declared");
    assert!(declared(&names, SDK_CRATE, "Q_MARKED", root.alloc()), "`Q`'s output is declared");
}

/// Rule 23 (f): the planted switch restores the old match (the path's text, spaces taken out):
/// the attribute `register!(A)` wrote is refused with `AttrNotFound` naming
/// `crate:: attrs:: mark` (the lowering's rendered `crate`), `mark` never runs on `A`, and
/// `A_MARKED` is not declared. The plain source forms (e) are still found under the plant, so
/// the cause is the `$crate` the expansion wrote, not the walk.
#[test]
fn the_planted_spelling_match_refuses_the_macro_written_attribute() {
    let root = root();
    let (names, world) = solve(&root, AttrPlant::Spelling);
    let refused = not_found(&names, SDK_CRATE).find(|name| name.chars().filter(|c| !c.is_whitespace()).eq("crate::attrs::mark".chars()));
    assert!(refused.is_some(), "the plant refuses `crate::attrs::mark` as not among the item's attributes: notes {:?}", names.notes(SDK_CRATE));
    let ran = world.ran.load(Ordering::Relaxed);
    assert_eq!(ran & bit("A"), 0, "under the plant `mark` never runs on `A`");
    assert!(!declared(&names, SDK_CRATE, "A_MARKED", root.alloc()), "under the plant `A`'s output is not declared");
    assert_ne!(ran & bit("P"), 0, "control: the source form `#[attrs::mark]` is still found under the plant");
}
