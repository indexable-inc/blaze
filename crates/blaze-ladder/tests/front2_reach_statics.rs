//! The reach walk follows what a reached static's or constant's initializer NAMES
//! (front2/reach/initializers.rs): `a` and `b`, named only by `static TABLE: [fn() -> i32; 2]
//! = [a, b];`, and `c`, named only by `const F: fn() -> i32 = c;`, are reached from `run`,
//! which calls them only through the table and the constant. Before the rule they never
//! entered the census, so the image's relocation to each had no instance (the rung
//! x0-static-03-fn-pointers: "points to fn instance .., which no code of the program reaches").
//!
//! The Reference, "Static items": "Static initializers are constant expressions evaluated at
//! compile time"; "Function item types": "When referred to, a function item, or the
//! constructor of a tuple-like struct or enum variant, yields a zero-sized value of its
//! function item type", coerced here to a fn pointer the program calls.
//!
//! Line it prints (and asserts): `reached by initializers: a Checked, b Checked, c Checked;
//! edge TABLE -> a`.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_clone.rs
//! does. NOT RUN by the writer (a write-only lane).

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::reach::{First, Reached, reach};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_own::Alloc;
use blaze_scope::names::{self, Written};
use blaze_surface::{Namespace, PathKind};

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}

fn a() -> i32 {
    20
}

fn b() -> i32 {
    15
}

fn c() -> i32 {
    7
}

static TABLE: [fn() -> i32; 2] = [a, b];

const F: fn() -> i32 = c;

fn run() -> i32 {
    (TABLE[0])() + (TABLE[1])() + F()
}
";

#[test]
fn initializers_reach_what_they_name() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/inits/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "inits", dir: "/inits/", root: "/inits/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let lib = |name: &'static str| {
        names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate root"))
    };
    let run = lib("run");
    let [a, b, c, table] = ["a", "b", "c", "TABLE"].map(|n| front.keys.item(lib(n)).unwrap_or_else(|e| panic!("{n}'s key: {e:?}")));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    store.ensure(&names, &mut front.keys, Want::Fn(run)).unwrap_or_else(|e| panic!("run's head: {e:?}"));
    let body = lower_fn(&names, prepared, &mut front.keys, &mut store, run).unwrap_or_else(|e| panic!("run lowers: {e}"));
    let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
    let view = body.lowered.view();
    let spell = Spelling::of(&view).expect("run's spelling");
    let scope = body.scope(&spell);
    let mut checked = check_body(&mut records, &body, &scope).unwrap_or_else(|e| panic!("run checks: {e:?}"));
    let census = reach(&mut records, prepared, body.owner, First::CheckedBody(&body, &mut checked), &scope);
    let outcome = |k| census.fns.iter().find(|(f, _)| *f == k).map(|(_, o)| o);
    let shown = |k| match outcome(k) {
        Some(Reached::Checked) => String::from("Checked"),
        Some(other) => format!("{other:?}"),
        None => String::from("not reached"),
    };
    let edge = census.edges.iter().any(|&(from, to)| from == table && to == a);
    let line = format!("reached by initializers: a {}, b {}, c {}; edge TABLE -> a{}", shown(a), shown(b), shown(c), if edge { "" } else { " missing" });
    eprintln!("{line}");
    assert_eq!(line, "reached by initializers: a Checked, b Checked, c Checked; edge TABLE -> a");
    // The initializer is no fn of the census: it is not built as code.
    assert!(outcome(table).is_none(), "TABLE is a static, never a fn of the census: {:?}", outcome(table));
}
