//! Value paths tempo census 14 reported as "cannot find value" (E0425) in bodies rustc accepts
//! (piece e0425-1; wiring /Volumes/Projects/blaze-int-1112-notes/wiring/blaze-scope/e0425-1.md).
//!
//! Group 1 (FIXED by the wiring, blaze-scope task/lower.rs `Cfg::attr`): alloy-sol-macro
//! writes `#[rustfmt::skip] #[allow(clippy::all)] pub static BYTECODE: .. = ..;` in the
//! contract's module; tempo names it as `BYTECODE` from an impl of that module and as
//! `tempo_contracts::Permit2::BYTECODE` from another crate. An expansion's text spaces its
//! tokens (`rustfmt :: skip`), the tool's segment was not trimmed, so the tool attribute was
//! taken for an attribute macro and the static was never filed. A `macro_rules!` expansion's
//! text is spaced the same way, so the `made_mod!` below reproduces it with no proc macro. The
//! Reference, "Attributes", "Tool attributes": `rustfmt` and `clippy` are tools known to the
//! compiler; their attributes are inert.
//!
//! Group 4 (PROBE, `Trait::method` as a value): tempo's zone_node writes
//! `EthFilterApiServer::logs(&self.eth.filter, filter)` where the trait comes from jsonrpsee's
//! `#[rpc(server)]` through `pub use filter::EthFilterApiServer`. The Reference, "Paths",
//! "Path expressions" and "Associated items": "Associated functions .. can be called with a
//! path whose last segment is the function's name and whose prefix is the trait" (`Tr::m(&x)`),
//! and such a path is a value (`let f = Tr::m;`). The rows below are the plain forms (written
//! and through a `pub use` of a nested module): if they are clean, the census's false E0425 is
//! the trait's members as made by the proc macros (jsonrpsee then `#[async_trait]`), not
//! front2's `Trait::f` reading (front2/world_paths.rs `BodyWorld::path`, `Kind::Trait` arm).
//!
//! Group 3 (PROBE, printed only, not asserted): zone_checker's `decode_portal_event` defines
//! `macro_rules! ignored { .. => {{ decode_event::<$event>(log, $name, block)?; .. }} }` inside
//! the fn and calls it 19 times: 38 diagnostics = `log` and `block` per call. The Reference,
//! "Macros By Example", "Hygiene": "macro_rules! macros have mixed-site hygiene. This means
//! that loop labels, block labels, and local variables are looked up at the macro definition
//! site". blaze-lower keys a local by its name hashed with the token's local hygiene
//! (machine.rs `local_key`, blaze-expand body.rs `local_hygiene`): a template's `log` gets a
//! nonzero hygiene, the parameter `log` 0, so the def site's locals are never seen. Its fix is
//! blaze-lower's, not in this wiring.
//!
//! What it prints when it passes: `test e0425_value_paths ... ok`; in the notes, one line per
//! body: `made::inner: clean`, `outer: clean`, `outer_const: clean`, `tr_value: clean`,
//! `tr_value_reexported: clean`, `missing: diagnostics [.. UnresolvedPath ..]`,
//! `missing_method: diagnostics [.. UnresolvedPath ..]`, `PROBE local_macro_param: ..`.
//! Control: without the wiring's trim, `made::inner`, `outer` and `outer_const` say
//! `diagnostics [.. UnresolvedPath ..]` (or `made::inner` is not found from the root).
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_selfpaths.rs.

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main() {}
macro_rules! made_mod {
    () => {
        pub mod made {
            #[rustfmt::skip]
            #[allow(clippy::all)]
            pub static BYTECODE: u8 = 7;
            #[rustfmt::skip]
            pub const SIZE: u8 = 2;
            pub fn inner() -> u8 { BYTECODE + SIZE }
        }
    };
}
made_mod!();
fn outer() -> u8 { made::BYTECODE }
fn outer_const() -> u8 { made::SIZE }
fn missing() -> u8 { made::NOT_THERE }
trait Tr {
    fn m(&self) -> u8;
}
struct K;
impl Tr for K {
    fn m(&self) -> u8 { 1 }
}
fn tr_value() -> u8 {
    let x = K;
    let f = Tr::m;
    f(&x) + Tr::m(&x)
}
mod filter {
    pub trait TrServer {
        fn logs(&self) -> u8;
    }
    impl TrServer for super::K {
        fn logs(&self) -> u8 { 2 }
    }
}
mod api {
    pub use crate::filter::TrServer;
}
fn tr_value_reexported() -> u8 {
    let x = K;
    let f = api::TrServer::logs;
    f(&x) + api::TrServer::logs(&x)
}
fn missing_method() -> u8 {
    let x = K;
    Tr::not_there(&x)
}
fn local_macro_param(log: u8, block: u8) -> u8 {
    macro_rules! both {
        () => {{ log + block }};
    }
    both!()
}
";

/// Each free fn by its path from the root; `Some(true)`: must check clean, `Some(false)`: must
/// be E0425 (`UnresolvedPath`), `None`: a probe, printed only.
const FREE: [(&[&str], Option<bool>, &str); 8] = [
    (&["made", "inner"], Some(true), "group 1: `BYTECODE` from its own module, a `#[rustfmt::skip]` static of an expansion"),
    (&["outer"], Some(true), "group 1: `made::BYTECODE` from outside its module"),
    (&["outer_const"], Some(true), "group 1: a `#[rustfmt::skip]` const of an expansion, from outside"),
    (&["tr_value"], Some(true), "group 4: `let f = Tr::m;` and `Tr::m(&x)`"),
    (&["tr_value_reexported"], Some(true), "group 4: the same through `pub use` of a nested module's trait"),
    (&["missing"], Some(false), "a name no module declares stays E0425"),
    (&["missing_method"], Some(false), "a trait's fn it does not declare stays E0425"),
    (&["local_macro_param"], None, "group 3 probe: a body-local macro's template naming the fn's parameters (blaze-lower hygiene)"),
];

#[test]
fn e0425_value_paths() {
    let alloc = blaze_own::testing::heap().alloc();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/vals/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "vals", dir: "/vals/", root: "/vals/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (path, want, proves) in FREE {
        let shown = path.join("::");
        let Some(key) = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: path }, alloc)
            .ok()
            .and_then(|f| f.whole(path.len()))
            .map(|d| d.key)
        else {
            notes.push(format!("{shown}: not found from the crate's root ({proves})"));
            if want.is_some() {
                failed.push(shown);
            }
            continue;
        };
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            notes.push(format!("{shown}: no head: {e:?}"));
            failed.push(shown);
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((shown, b, want, proves)),
            Err(e) => {
                notes.push(format!("{shown}: does not lower: {e}"));
                if want.is_some() {
                    failed.push(shown);
                }
            }
        }
    }
    for (name, body, want, proves) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        let said = match check_body(&mut records, body, &scope) {
            Ok(done) if done.diags.is_empty() => "clean".to_string(),
            Ok(done) => format!("Ok with {} diagnostics", done.diags.len()),
            Err(CheckFail::Diags { diags, .. }) => format!("diagnostics {diags:?}"),
            Err(other) => format!("{other:?}"),
        };
        let brief: String = said.chars().take(240).collect();
        match want {
            None => notes.push(format!("PROBE {name}: {brief} ({proves})")),
            Some(w) => {
                notes.push(format!("{name}: {brief}"));
                let ok = if *w { said == "clean" } else { said.contains("UnresolvedPath") };
                if !ok {
                    notes.push(format!("  wanted {}: {proves}", if *w { "clean" } else { "UnresolvedPath (E0425)" }));
                    failed.push(name.clone());
                }
            }
        }
    }
    println!("{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies that did not say what they must: {failed:?}\n{}", notes.join("\n"));
}
