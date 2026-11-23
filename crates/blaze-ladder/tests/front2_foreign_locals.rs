//! An `extern` block declared inside a body, through the real front end with the installed
//! toolchain (`$HOME/.blaze/v2/toolchain`, as tests/front2_locals.rs): the fns and statics the
//! block declares are names of the block it is written in, typed by their declarations.
//!
//! The Reference (the pinned nightly's copy, share/doc/rust/html/reference, read 2026-11-16):
//! "External blocks": [items.extern.intro] "External blocks provide declarations of items that
//! are not defined in the current crate and are the basis of Rust's foreign function
//! interface." [items.extern.namespace] "The external block defines its functions and statics
//! in the value namespace of the module or block where it is located." [items.extern.fn.body]
//! "Functions within external blocks are declared in the same way as other Rust functions, with
//! the exception that they must not have a body and are instead terminated by a semicolon."
//! [items.extern.fn.safety] "A function declared in an extern block is implicitly unsafe unless
//! the safe function qualifier is present." [items.extern.static] "Statics within external
//! blocks are declared in the same way as statics outside of external blocks, except that they
//! do not have an expression initializing their value."
//! "Statements", "Item declarations": [statement.item.intro] "An item declaration statement has
//! a syntactic form identical to an item declaration within a module." [statement.item.scope]
//! "Declaring an item within a statement block restricts its scope to the block containing the
//! statement."
//!
//! The pinned core's `panic_fmt` (library/core/src/panicking.rs) is the case that asked:
//! ```text
//! pub const fn panic_fmt(fmt: fmt::Arguments<'_>) -> ! {
//!     ..
//!     unsafe extern "Rust" {
//!         #[lang = "panic_impl"]
//!         fn panic_impl(pi: &PanicInfo<'_>) -> !;
//!     }
//!     let pi = PanicInfo::new(&fmt, Location::caller(), true, false);
//!     unsafe { panic_impl(&pi) }
//! }
//! ```
//! It is checked here by its path, and `ext_never` is its shape.
//!
//! The lines this test prints when it passes (failures, if any, come first):
//! ```text
//! ext_call: clean
//! ext_before: clean
//! ext_never: clean
//! ext_arg_wrong: clean (refused with Mismatch)
//! ext_ret_wrong: clean (refused with Mismatch)
//! ext_static_wrong: clean (refused with Mismatch)
//! ext_outside: clean (refused with UnresolvedPath)
//! core::panicking::panic_fmt: clean
//! ```
//! A fix that only made the name resolve (to a hole of no type) fails `ext_arg_wrong`,
//! `ext_ret_wrong` and `ext_static_wrong`: each then checks clean. A fix that put the block's
//! names in the module fails `ext_outside`.

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

const MAIN: &str = "fn main(cx: &Cx, io: Io) -> Result<(), Error> {
    Ok(())
}
fn ext_call() -> u8 {
    unsafe extern \"C\" { fn ext(x: u8) -> u8; static COUNT: u32; }
    let n: u8 = unsafe { ext(1) };
    let c: u32 = unsafe { COUNT };
    let _ = c;
    n
}
fn ext_before() -> u8 { let n: u8 = unsafe { ext(1) }; unsafe extern \"C\" { fn ext(x: u8) -> u8; } n }
fn ext_never(pi: &u8) -> ! {
    unsafe extern \"Rust\" {
        #[link_name = \"its_symbol\"]
        fn never(pi: &u8) -> !;
    }
    unsafe { never(pi) }
}
fn ext_arg_wrong() -> u8 { unsafe extern \"C\" { fn ext(x: u8) -> u8; } unsafe { ext(true) } }
fn ext_ret_wrong() -> bool { unsafe extern \"C\" { fn ext(x: u8) -> u8; } unsafe { ext(1) } }
fn ext_static_wrong() -> bool { unsafe extern \"C\" { static COUNT: u32; } unsafe { COUNT } }
fn ext_outside() -> u8 { unsafe { ext(1) } }
";

/// The bodies that must check clean, each with the rule it proves.
const CLEAN: &[(&str, &str)] = &[
    ("ext_call", "a block's extern block declares its fn (the call is a `u8`) and its static (a `u32`) in the block"),
    ("ext_before", "a block's foreign fn is in scope before its declaration"),
    ("ext_never", "core's `panic_fmt` shape: an attributed foreign fn of `extern \"Rust\"` returning `!`"),
];

/// The bodies that must be refused, with the finding each must have.
const REFUSED: &[(&str, &str)] = &[
    ("ext_arg_wrong", "Mismatch"),
    ("ext_ret_wrong", "Mismatch"),
    ("ext_static_wrong", "Mismatch"),
    ("ext_outside", "UnresolvedPath"),
];

/// The library fn whose body declares an extern block and calls its fn.
const LIBRARY: &[(&str, &[&str])] = &[("core", &["panicking", "panic_fmt"])];

#[test]
fn an_extern_block_in_a_body_declares_its_fns_and_statics() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/exts/src/main.rs".to_string(), MAIN.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "exts", dir: "/exts/", root: "/exts/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: MAIN }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let find = |krate: usize, segments: &[&str]| {
        names::path(&names, names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, Alloc::global())
            .ok()
            .and_then(|f| f.whole(segments.len()))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{segments:?}` from crate {krate}'s root"))
    };
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed = Vec::new();
    let mut report: Vec<String> = Vec::new();
    let mut cases: Vec<(String, blaze_id::ItemKey, &str, Option<&str>)> = Vec::new();
    for &(n, p) in CLEAN {
        cases.push((n.to_string(), find(0, &[n]), p, None));
    }
    for &(n, w) in REFUSED {
        cases.push((n.to_string(), find(0, &[n]), "must be refused", Some(w)));
    }
    for &(krate, path) in LIBRARY {
        let ix = crates.iter().position(|c| c.name == krate).unwrap_or_else(|| panic!("{krate} among the crates"));
        cases.push((format!("{krate}::{}", path.join("::")), find(ix, path), "a library fn whose body declares an extern block and calls its fn", None));
    }
    for (name, key, proves, want) in cases {
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            report.push(format!("{name}: no head: {e:?}"));
            failed.push(name);
            continue;
        }
        let body = match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => b,
            Err(e) => {
                report.push(format!("{name}: does not lower: {e}"));
                failed.push(name);
                continue;
            }
        };
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, &body, &scope), want) {
            (Ok(_), None) => report.push(format!("{name}: clean")),
            (Err(CheckFail::Diags { diags, .. }), Some(w)) if format!("{diags:?}").contains(w) => {
                report.push(format!("{name}: clean (refused with {w})"));
            }
            (Ok(_), Some(w)) => {
                report.push(format!("{name}: checked clean, but must be refused with {w}"));
                failed.push(name);
            }
            (Err(CheckFail::Diags { diags, .. }), _) => {
                report.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name);
            }
            (Err(other), _) => {
                report.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name);
            }
        }
    }
    // Failures first: the harness's runner shows the first lines only.
    report.sort_by_key(|l| l.contains(": clean"));
    for line in &report {
        println!("{line}");
    }
    let why: Vec<String> = report.iter().filter(|l| !l.contains(": clean")).map(|l| l.chars().take(300).collect()).collect();
    assert!(failed.is_empty(), "bodies that did not check as they must: {failed:?}\n{}", why.join("\n"));
}
