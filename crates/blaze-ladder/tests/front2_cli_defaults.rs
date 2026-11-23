//! A struct with defaulted type parameters named bare in a body's value path, on front2 with
//! the real core: tempo's census 14 shape ("25 diag E0308", the first
//! `zone_node::cli::<impl ZoneCli>::try_parse_from at /package/1334/src/cli.rs:128`: at `cli`,
//! found `Cli<EthereumChainSpecParser, NoArgs, DefaultRpcModuleValidator, NoSubCmd>`, expected
//! `Cli<ZoneChainSpecParser, ZoneArgs, DefaultRpcModuleValidator, ZoneSubcommand>`), reduced.
//! tempo writes `Cli::from_arg_matches(&matches).map(|cli| Self::Node { cli: Box::new(cli), .. })`
//! with `type ZoneNodeCli = Cli<ZoneChainSpecParser, ZoneArgs, ..>` in the variant's field and
//! reth's `pub struct Cli<C = EthereumChainSpecParser, Ext = NoArgs, Rpc = .., SubCmd = NoSubCmd>`
//! (crates/ethereum/cli/src/interface.rs:34), which also has an inherent `impl Cli { fn
//! parse_args() .. }` at the defaults (line 57) and a derived trait impl
//! `impl<C, Ext, Rpc, SubCmd> FromArgMatches for Cli<C, Ext, Rpc, SubCmd>`.
//!
//! The rules (the Reference, "Generic parameters" and "Paths", "Paths in expressions"): a
//! default fills an omitted argument in a TYPE path (`fn f(x: Cli)` is `Cli<D1, D2>`); in an
//! EXPRESSION path a segment written with NO arguments leaves every one to inference
//! (`Cli::parse(n)` is `<Cli<_, _> as Parse>::parse`), and one written with SOME takes the
//! defaults for the rest (`Cli::<Z1>::parse(n)` is `Cli<Z1, D2>`, as `Vec::<u8>::new()` is
//! `Vec<u8, Global>`). An inherent impl at the defaults (`impl Cli { .. }`) gives only its own
//! names; a name it lacks is no reason to pick its self type.
//!
//! This file is a PROBE as much as a test: each body's outcome is printed whole (the checker's
//! found and expected types for a mismatch), so a failure names where the defaults came in.
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.

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

const CLI: &str = "fn main() {}
struct D1;
struct D2;
struct Z1;
struct Z2;
trait Mk { fn mk() -> Self; }
impl Mk for D1 { fn mk() -> Self { D1 } }
impl Mk for D2 { fn mk() -> Self { D2 } }
impl Mk for Z1 { fn mk() -> Self { Z1 } }
impl Mk for Z2 { fn mk() -> Self { Z2 } }
trait Parse: Sized { fn parse(n: u8) -> Option<Self>; }
struct Cli<C = D1, E = D2> { c: C, e: E }
impl Cli {
    fn plain() -> Self { Cli { c: D1, e: D2 } }
}
impl<C, E> Cli<C, E> {
    fn new(c: C, e: E) -> Self { Cli { c: c, e: e } }
}
impl<C: Mk, E: Mk> Parse for Cli<C, E> {
    fn parse(n: u8) -> Option<Self> { if n == 0 { None } else { Some(Cli { c: C::mk(), e: E::mk() }) } }
}
type ZoneCli = Cli<Z1, Z2>;
enum Outer { Node { cli: ZoneCli, n: u8 }, Dev }
fn tempo_shape(n: u8) -> Option<Outer> { Cli::parse(n).map(|cli| Outer::Node { cli: cli, n: n }) }
fn trait_fn_bare(n: u8) -> Option<ZoneCli> { Cli::parse(n) }
fn inherent_generic_bare() -> ZoneCli { Cli::new(Z1, Z2) }
fn type_path_defaults(x: Cli) -> Cli<D1, D2> { x }
fn some_written(n: u8) -> Option<Cli<Z1, D2>> { Cli::<Z1>::parse(n) }
fn inherent_at_defaults() -> Cli<D1, D2> { Cli::plain() }
fn bad_inherent_at_defaults() -> ZoneCli { Cli::plain() }
fn bad_some_written(n: u8) -> Option<ZoneCli> { Cli::<Z1>::parse(n) }
fn bad_type_path(x: Cli) -> ZoneCli { x }
";

/// The bodies that must check clean, each with what it proves.
const CLEAN: [(&str, &str); 6] = [
    ("tempo_shape", "tempo's form: `Cli::parse(n).map(|cli| Outer::Node { cli, .. })`, `Cli`'s arguments inferred from the variant's field"),
    ("trait_fn_bare", "`Cli::parse(n)`: a trait fn through the bare struct path, arguments inferred from the return type"),
    ("inherent_generic_bare", "`Cli::new(Z1, Z2)`: the generic inherent impl through the bare path"),
    ("type_path_defaults", "`x: Cli` in a signature is `Cli<D1, D2>`: a type path's defaults"),
    ("some_written", "`Cli::<Z1>::parse(n)` is `Cli<Z1, D2>`: some written, the rest defaulted"),
    ("inherent_at_defaults", "`Cli::plain()` from `impl Cli` is `Cli<D1, D2>`"),
];

/// The bodies that must be a type mismatch (E0308). A fix that leaves every argument of a
/// value path to inference, or drops the defaults of a type path, passes them.
const MISMATCH: [(&str, &str); 3] = [
    ("bad_inherent_at_defaults", "`Cli::plain()` is `Cli<D1, D2>` (its impl's self type), never a `Cli<Z1, Z2>`"),
    ("bad_some_written", "`Cli::<Z1>::parse(n)` is `Cli<Z1, D2>`, never a `Cli<Z1, Z2>`"),
    ("bad_type_path", "`x: Cli` is `Cli<D1, D2>`, never a `Cli<Z1, Z2>`"),
];

/// Expected line: `test value_path_omitted_args_are_inferred_not_defaulted ... ok`, and on
/// stderr one `PROBE <body>: ..` line per body. Without the fix (the census's state) it fails
/// on `tempo_shape` (and, if the cause is the bare value path's filling, `trait_fn_bare`),
/// the PROBE line printing the mismatch's found `Cli<D1, D2>` and expected `Cli<Z1, Z2>`.
#[test]
fn value_path_omitted_args_are_inferred_not_defaulted() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/cli/src/main.rs".to_string(), CLI.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "cli", dir: "/cli/", root: "/cli/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: CLI }];
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let mut front = Front2::new(&names, &frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let heap = blaze_own::testing::heap();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (name, proves, clean) in CLEAN.iter().map(|&(n, p)| (n, p, true)).chain(MISMATCH.iter().map(|&(n, p)| (n, p, false))) {
        let key = names::path(&names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"));
        if let Err(e) = store.ensure(&names, &mut front.keys, Want::Fn(key)) {
            notes.push(format!("{name}: no head: {e:?}"));
            failed.push(name.to_string());
            continue;
        }
        match lower_fn(&names, prepared, &mut front.keys, &mut store, key) {
            Ok(b) => bodies.push((name, b, proves, clean)),
            Err(e) => {
                notes.push(format!("{name}: does not lower: {e}"));
                failed.push(name.to_string());
            }
        }
    }
    for (name, body, proves, clean) in &bodies {
        let view = body.lowered.view();
        let spell = Spelling::of(&view).expect("spelling");
        let scope = body.scope(&spell);
        let mut records = Records { names: &names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, body, &scope), *clean) {
            (Ok(done), true) if done.diags.is_empty() => notes.push(format!("{name}: clean")),
            (Ok(done), _) => {
                notes.push(format!("{name}: Ok with {} diagnostics; wanted {} ({proves})", done.diags.len(), if *clean { "none" } else { "a mismatch" }));
                failed.push(name.to_string());
            }
            // The mismatch is the program's own error, by the checker's code for E0308; the
            // diagnostic's text carries the found and expected types (check.rs `diag_about`).
            (Err(CheckFail::Diags { diags, .. }), false) if format!("{diags:?}").contains("Mismatch") => {
                notes.push(format!("{name}: a mismatch, as it must be: {diags:?}"));
            }
            (Err(CheckFail::Diags { diags, .. }), _) => {
                notes.push(format!("{name}: diagnostics {diags:?} ({proves})"));
                failed.push(name.to_string());
            }
            (Err(other), _) => {
                notes.push(format!("{name}: {other:?} ({proves})"));
                failed.push(name.to_string());
            }
        }
    }
    for n in &notes {
        eprintln!("PROBE {n}");
    }
    assert_eq!(bodies.len(), CLEAN.len() + MISMATCH.len(), "every body was lowered\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies through `Cli`'s defaulted parameters did not check as they must: {failed:?}\n{}", notes.join("\n"));
}
