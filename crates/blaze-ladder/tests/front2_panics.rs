//! `panic!`, `assert!` and `write!` bodies through front2 with the real core and the
//! `blaze-2026.11` edition: every program reaches `core::panicking::panic_bounds_check`, whose body
//! is `if cfg!(panic = "immediate-abort") { super::intrinsics::abort() } panic!("index out of
//! bounds: the len is {len} but the index is {index}")` (library/core/src/panicking.rs), so these
//! must check clean. Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_bounds.rs does.
//!
//! The Reference, "Never type": "Expressions of type `!` ... can be coerced into any other type";
//! "Block expressions": a block whose statements diverge has type `!` when it has no final
//! operand. A call of a function whose return type is `!` is such an expression whatever its
//! arguments are. The pinned core: `pub const fn panic_fmt(fmt: fmt::Arguments<'_>) -> !`
//! (core/src/panicking.rs); `panic!` on the 2021 edition and later is `panic_2021!`, which
//! expands to `$crate::panicking::panic_fmt($crate::const_format_args!(..))`, a format string
//! whose `{len}` captures the local `len` ("Format strings", RFC 2795 implicit named
//! arguments): an argument expression written by the format string, not by the user.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::{lower_fn, lower_member};
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::keys::{MemberKind, checker_of};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};
use blaze_typeck::{CheckError, Recorded};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn a(len: usize, index: usize) -> ! { panic!(\"the len is {len} but the index is {index}\") }
fn a2(len: usize, index: usize) -> ! { if false { core::intrinsics::abort_immediate() } panic!(\"the len is {len} but the index is {index}\") }
fn b(x: u8) -> u8 { match x { 0 => 1, _ => panic!(\"no\") } }
fn c() -> ! { panic!() }
fn d(x: u8) { assert!(x > 0, \"x is {x}\") }
fn e(s: &[u8]) -> u8 { s[0] }
struct W(u8);
impl core::fmt::Display for W {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { write!(f, \"{}\", self.0) }
}
struct V;
impl core::fmt::Display for V {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result { f.write_str(\"x\") }
}
";

/// The free bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 6] = [
    ("a", "`panic!` with two captured arguments as a `-> !` tail"),
    // The pinned core (library/core/src/panicking.rs line 266): `if cfg!(panic = "immediate-abort")
    // { super::intrinsics::abort_immediate() }`; it has no `intrinsics::abort` (an E0425 here).
    ("a2", "`panic_bounds_check`'s shape: an `if false { abort_immediate() }` statement, then `panic!`"),
    ("b", "`panic!(\"no\")` as a match arm of type `u8`"),
    ("c", "`panic!()` as a `-> !` tail"),
    ("d", "`assert!` with a captured argument"),
    ("e", "a slice index: the program then reaches `panic_bounds_check`"),
];

#[test]
fn panics_through_front2() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/pnc/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "pnc", dir: "/pnc/", root: "/pnc/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let core_ix = crates.iter().position(|c| c.name == "core").expect("core among the crates");
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
        let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut clean: Vec<String> = Vec::new();
    // Each failed body's finding, for the failure message: the runner shows only the panic.
    let mut why: Vec<String> = Vec::new();

    // The free fns of the fixture, then core's `panic_bounds_check` by its path.
    let mut targets: Vec<(String, blaze_id::ItemKey, &str)> = CLEAN.iter().map(|(n, p)| (n.to_string(), find(0, &[n]), *p)).collect();
    // Private to `core::panicking`: found by its `#[lang = "panic_bounds_check"]`, in core.
    let pbc = front.index.lang("panic_bounds_check").expect("core declares `panic_bounds_check`");
    assert_eq!(names.crate_of(pbc), Some(core_ix), "`panic_bounds_check` is core's");
    targets.push(("core::panicking::panic_bounds_check".into(), pbc, "the library fn every program reaches"));
    let mut bodies = Vec::new();
    for (name, key, proves) in targets {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            println!("{name}: no head: {e:?}");
            failed.push(name);
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((name, b, proves)),
            Err(e) => {
                println!("{name}: does not lower: {e}");
                failed.push(name);
            }
        }
    }
    // The two `Display::fmt` members.
    for (k, _, _) in names.items(0) {
        let Some(header) = names.item_text(k).and_then(|t| t.impl_header) else { continue };
        if store.ensure(&names, &mut front.keys, Want::Impl(k)).is_err() {
            println!("impl `{header}`: no head");
            failed.push(header.to_string());
            continue;
        }
        let Some(mk) = front.keys.filed_member(checker_of(k), MemberKind::Fn, "fmt") else { continue };
        match lower_member(&names, prepared, &mut front.keys, &mut store, mk) {
            Ok(b) => bodies.push((format!("<{header}>::fmt"), b, "a `Display::fmt` body")),
            Err(e) => {
                println!("{header}: fmt does not lower: {e}");
                failed.push(header.to_string());
            }
        }
    }
    for (name, body, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => clean.push(name.clone()),
            Ok(done) => {
                why.push(format!("{name}: Ok with {} diagnostics {:?} ({proves})", done.diags.len(), done.diags));
                failed.push(name.clone());
            }
            Err(CheckFail::Check { error: CheckError::UnknownInCleanBody { at: Recorded::ExprTy(x) }, .. }) => {
                // The untyped node: its kind and its source.
                let kind = view.expr(x);
                let text = view.expr_range(x).ok().and_then(|r| view.text_at(r).ok());
                why.push(format!("{name}: UnknownInCleanBody at {x:?}: {kind:?} `{text:?}` ({proves})"));
                failed.push(name.clone());
            }
            Err(CheckFail::Diags { diags, .. }) => {
                // Which segment a diagnostic's `name` hash is, among the expansion's words.
                let words = ["panic", "panic_fmt", "panicking", "const_format_args", "format_args", "panic_2021", "unreachable", "core", "crate"];
                let known: Vec<String> = words.iter().map(|w| format!("{w}={:?}", blaze_typeck::Name::of(w))).collect();
                why.push(format!("{name}: diagnostics {diags:?} ({proves}); names: {known:?}"));
                failed.push(name.clone());
            }
            Err(other) => {
                why.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name.clone());
            }
        }
        if failed.last() == Some(name) && std::env::var("FRONT2_PANICS_DUMP").is_ok() {
            let mut out = String::new();
            let _ = blaze_lower::dump::dump(&view, Alloc::global(), &mut out);
            println!("{name}'s lowered body:\n{out}");
        }
    }
    println!("clean: {clean:?}");
    assert_eq!(bodies.len() + failed.iter().filter(|f| !bodies.iter().any(|b| &b.0 == *f)).count(), CLEAN.len() + 3, "every target was taken");
    assert!(failed.is_empty(), "bodies that must check clean did not: {failed:?}\n{}", why.join("\n"));
}
