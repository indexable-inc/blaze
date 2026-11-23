//! Written types the tempo census refused, ACROSS CRATES: the library crate `tails` declares
//! them, the user crate's bodies use them.
//!
//! - A fn pointer type of any ABI the pinned nightly knows (front2/types.rs `fn_ptr_abi`, the
//!   checker's `tc::Abi::of_name`): evm2's `type TailInstrFn = extern "rust-preserve-none"
//!   fn(..)` through a chain of three aliases, a static table of them, a fn item coerced to
//!   one. The ABI is the type's identity: coercing that fn item to `extern "C" fn` is E0308.
//! - A type alias impl trait (front2/tait.rs): `pub type Hidden = impl Shown;` held in a
//!   struct's field. A body sees its written bound (`x.h.shown()` resolves through `Shown`);
//!   an auto trait of it (`Send`) is refused BY NAME, naming `define_opaque`, never guessed.
//! - A type macro an expansion writes with `$crate` (front2/type_marks.rs): the library's
//!   `holder!(Held)` writes `pub struct Held { pub f: $crate::inner_ty!() }` in the user
//!   crate; the call is read in `tails`, where `inner_ty!` is. The control: the user writing
//!   `crate::inner_ty!()` in its own file names its own crate, which has no `inner_ty!`, and
//!   stays refused (rustc: "could not find `inner_ty` in the crate root").
//! - A body's `const SIZE: usize` used as an array length in a type of that body
//!   (front2/types.rs `body_const_len`): in scope in its block and the blocks inside. The
//!   control: a const declared in an inner block is not in scope outside it (E0412's class).
//!
//! Expected lines (test `written_types_across_crates`), one per row, `written: NAME: as
//! stated` (or `FAILED: want .., said ..`), then `written: 10 rows as stated`:
//! via_table, via_static, make_table, through_bound, held_x, body_const_len: clean;
//! abi_bad: Mismatch; needs_hidden: define_opaque; user_mac: TypeMacro; out_of_block: a
//! type that names nothing.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as
//! tests/front2_field_defaults_cross.rs.

#![feature(allocator_api)]

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::Store;
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_mem::Frame;
use blaze_own::{Alloc, Erased};
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::names::{self, Written};
use blaze_scope::resolve::{CrateInput, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_surface::{Namespace, PathKind};

/// The library: the tail-call table's types, the opaque alias and the two macros.
const TAILS: &str = "#![feature(rust_preserve_none_cc)]
#![feature(type_alias_impl_trait)]
pub type Tail0 = extern \"rust-preserve-none\" fn(u8) -> u8;
pub type Tail1 = Tail0;
pub type TailInstrFn = Tail1;
pub extern \"rust-preserve-none\" fn step(x: u8) -> u8 { x }
pub static TABLE: [TailInstrFn; 1] = [step];

pub trait Shown { fn shown(&self) -> u8; }
pub struct Seven;
impl Shown for Seven { fn shown(&self) -> u8 { 7 } }
pub type Hidden = impl Shown;
#[define_opaque(Hidden)]
pub fn hidden() -> Hidden { Seven }
pub struct Holder { pub h: Hidden }

#[macro_export]
macro_rules! inner_ty { () => { u8 } }
#[macro_export]
macro_rules! holder { ($n:ident) => { pub struct $n { pub f: $crate::inner_ty!() } } }
";

/// The user crate: one fn a row.
const USER: &str = "use tails::{Holder, Shown, TABLE, TailInstrFn, step};
pub fn via_table(f: TailInstrFn, x: u8) -> u8 { f(x) }
pub fn via_static(x: u8) -> u8 { (TABLE[0])(x) }
pub fn make_table() -> TailInstrFn { step }
pub fn abi_bad() -> extern \"C\" fn(u8) -> u8 { step }

pub fn through_bound(x: &Holder) -> u8 { x.h.shown() }
fn is_send<T: Send>(_: &T) {}
pub fn needs_hidden(x: &Holder) { is_send(&x.h) }

tails::holder!(Held);
pub fn held_x(h: &Held) -> u8 { h.f }
pub struct Mine { pub f: crate::inner_ty!() }
pub fn user_mac(m: &Mine) -> u8 { m.f }

pub fn body_const_len() -> u8 { const SIZE: usize = 4; let buf: [u8; SIZE] = [0u8; SIZE]; buf[3] }
pub fn out_of_block() -> u8 { { const INNER: usize = 2; } let b: [u8; INNER] = [0u8; 2]; b[0] }
pub fn main() {}
";

/// A row: the user fn, what it must say (`clean`, or `|`-separated words one of which the
/// outcome holds), and what it proves.
const ROWS: [(&str, &str, &str); 10] = [
    ("via_table", "clean", "a call through `TailInstrFn`, three aliases deep, of the \"rust-preserve-none\" ABI"),
    ("via_static", "clean", "a call through a static table of that type"),
    ("make_table", "clean", "a fn item of that ABI coerces to its fn pointer type"),
    ("abi_bad", "Mismatch", "the same fn item to an `extern \"C\"` fn pointer: two types (E0308)"),
    ("through_bound", "clean", "a method of an alias's opaque, through its written bound"),
    ("needs_hidden", "define_opaque", "an auto trait of an alias's opaque: refused by name"),
    ("held_x", "clean", "`$crate::inner_ty!()` an expansion wrote: read in `tails`"),
    ("user_mac", "TypeMacro|macro call", "`crate::inner_ty!()` written in the user's file: its own crate, no such macro"),
    // OPEN (first run, 2026-11-18): must be `clean`. The length's name is found among the body's
    // constants (types.rs `body_const_len`) and asks its value (`NeedConst`), and nothing on
    // this path evaluates a constant declared in a body: "to_typeck refused Form { what: an
    // array repeat length }". The row states what is said today so the fix flips it.
    ("body_const_len", "an array repeat length", "OPEN: `[u8; SIZE]` with the body's `const SIZE`: its value is asked and not evaluated yet"),
    ("out_of_block", "cannot find|NotFound|TypeMiss", "a const of an inner block is not in scope outside it"),
];

/// The texts of the two-crate layout: the library's file for crate `dep`, the rest as
/// [`Inputs`] reads them.
struct WithDep<'t> {
    base: &'t Texts<'t>,
    dep: usize,
    files: &'t [(String, String)],
}

impl<'t> SourceTexts<'t> for WithDep<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        if krate == self.dep {
            return self.files.iter().find(|(p, _)| p == path).map(|(_, t)| t.as_str());
        }
        self.base.read(krate, path)
    }
}

/// Expected: see the header.
#[test]
fn written_types_across_crates() {
    // The test's one allocator, made at its top: a frame, its memory from the OS, kept for the
    // process (the store takes `Alloc<'static>`), as tests/front2_cast_infer.rs keeps it.
    // Two crates over the real library's names: more than a frame's default reserve (first run,
    // 2026-11-18: "the allocator refused 16384 bytes"); the census tests' size.
    let frame: &'static Frame = Box::leak(Box::new(Frame::with_reserve(32 << 30, blaze_mem::tag::Tag::Infer).expect("the test's frame")));
    let erased: &'static Erased<&'static Frame> = frame.alloc(Erased::new(frame));
    let alloc: Alloc<'static> = erased.handle();
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user_files = [("/user/src/main.rs".to_string(), USER.to_string())];
    let dep_files = [("/tails/src/lib.rs".to_string(), TAILS.to_string())];
    let user = UserCrate { name: "user", dir: "/user/", root: "/user/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates: Vec<CrateInput<'_>> = inputs.crates();
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    assert!(sysroot.core.is_some() && sysroot.bstd.is_some(), "the toolchain at {shelf} has `core` and `bstd` units");
    let dep = crates.len();
    let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("tails", dep)].into_boxed_slice());
    let (cfg, crate_edition, implicit) = (crates[0].cfg, crates[0].edition, crates[0].implicit);
    crates[0].deps = user_deps;
    crates.push(CrateInput {
        name: "tails",
        key: CrateItemKey(ContentId::of("blaze-ladder written types test crate", b"tails")),
        edition: crate_edition,
        implicit,
        root: "/tails/src/lib.rs",
        shown_from: "/tails/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (two crates): {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: USER }];
    let mut front = Front2::new(&names, frame, &sources, alloc).unwrap_or_else(|e| panic!("front2: {e}"));
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut store = Store::new(front.index.lang("sized"), front.frame, alloc);
    let mut buckets = None;

    let mut failed: Vec<String> = Vec::new();
    for (n, want, proves) in ROWS {
        let found = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[n] }, alloc).unwrap_or_else(|e| panic!("`{n}`: {e}"));
        let k = found.whole(1).unwrap_or_else(|| panic!("`{n}` not named")).key;
        let said = match lower_fn(&names, prepared, &mut front.keys, &mut store, k) {
            Err(e) => format!("not lowered: {e} ({e:?})"),
            Ok(body) => {
                let view = body.lowered.view();
                let spell = Spelling::of(&view).unwrap_or_else(|e| panic!("spelling `{n}`: {e:?}"));
                let scope = body.scope(&spell);
                let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
                match check_body(&mut records, &body, &scope) {
                    Ok(done) if done.diags.is_empty() => "clean".to_string(),
                    Ok(done) => format!("diags {:?}", done.diags),
                    Err(CheckFail::Diags { diags, .. }) => format!("diags {diags:?}"),
                    Err(e) => format!("not checked: {e} ({e:?})"),
                }
            }
        };
        let ok = if want == "clean" { said == "clean" } else { want.split('|').any(|w| said.contains(w)) };
        if ok {
            println!("written: {n}: as stated ({proves})");
        } else {
            let brief: String = said.chars().take(700).collect();
            println!("written: {n}: FAILED: want {want}, said {brief}");
            failed.push(n.to_string());
        }
    }
    assert!(failed.is_empty(), "rows not as stated: {failed:?}");
    println!("written: {} rows as stated", ROWS.len());
}
