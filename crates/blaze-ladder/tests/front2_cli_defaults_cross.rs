//! The THIRD probe of tempo's census 14 E0308 (`zone_node::cli::<impl ZoneCli>::try_parse_from`
//! at `/package/1334/src/cli.rs:128`: at `cli`, found `Cli<EthereumChainSpecParser, NoArgs,
//! DefaultRpcModuleValidator, NoSubCmd>`, reth's four DEFAULTS, expected `Cli<ZoneChainSpecParser,
//! ZoneArgs, DefaultRpcModuleValidator, ZoneSubcommand>`). The one-crate probes
//! (tests/front2_cli_defaults.rs, tests/front2_cli_defaults_tempo.rs) PASS, so what tempo has and
//! they lack is that reth's `Cli`, its `impl Cli { .. }` at the defaults (reth
//! crates/ethereum/cli/src/interface.rs:57), its generic impls and the trait impl giving
//! `from_arg_matches` all live in ANOTHER crate, reached through a glob re-export
//! (`reth_ethereum::cli::{Cli}`, whose `cli` module is `pub use reth_ethereum_cli::*;`).
//!
//! One text `DEP` (reth's shape) and one `USER` (tempo's), checked in two layouts:
//! - CONTROL, one crate: `DEP` is the user crate's `mod dep { .. }` (the path `dep::cli::Cli`
//!   is the same text in both layouts);
//! - CROSS, two crates: `DEP` is the library crate `dep`, a dependency of the user crate.
//!
//! Every body in `CLEAN` must check with no diagnostic in both layouts, the body in `MISMATCH`
//! must be E0308 in both. The rule (the Reference, "Paths", "Paths in expressions", and
//! "Generic parameters"): a value path's segment written with no arguments leaves all of them
//! to inference; a parameter's default fills only a TYPE path (`impl Cli` is `impl Cli<EthSpec,
//! NoArgs, DefaultRpc, NoSub>`), so `impl Cli`'s fns are found only on `Cli` at its defaults
//! (the Reference, "Implementations", "Inherent implementations"), and `Cli::from_matches` is
//! the trait's fn with `Self = Cli<?C, ?E, ?R, ?S>`, the variables decided by the use. Which crate
//! an item is in changes none of this.
//!
//! Expected lines (test `cross_crate_value_path_args_are_inferred_not_defaulted`):
//! `CONTROL one crate: 8 bodies as they must be`
//! `CROSS two crates: 8 bodies as they must be`
//! then `test cross_crate_value_path_args_are_inferred_not_defaulted ... ok`.
//! Today, if the cause is cross-crate: FAIL with the CONTROL line printed and the message listing
//! each CROSS body's outcome (`tempo_boxed: diagnostics [(Mismatch, .., "found .. Cli<EthSpec,
//! NoArgs, DefaultRpc, NoSub> .. expected .. Cli<ZoneSpec, ZoneArgs, DefaultRpc, ZoneSub>")]`);
//! which bodies fail names the form at fault. If the CONTROL fails too, the cause is `Box`
//! (the one form the earlier probes left out: `cli: Box::new(cli)`, tempo's exact field).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`, as tests/front2_panics.rs.

use blaze_id::{ContentId, CrateItemKey};
use blaze_ladder::crates::{DirShelf, Inputs, Texts, Toolchain, UserCrate};
use blaze_ladder::front2::Front2;
use blaze_ladder::front2::body::lower_fn;
use blaze_ladder::front2::check::{BuildOrder, CheckFail, Records, check_body};
use blaze_ladder::front2::heads::{Store, Want};
use blaze_ladder::front2::spell::Spelling;
use blaze_ladder::refusal::SourceFile;
use blaze_lower::scoped::Prepares;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::resolve::{CrateInput, Names, SourceTexts, resolve_crates};
use blaze_scope::task::Serial;
use blaze_scope::names::{self, Written};
use blaze_surface::{Edition, Namespace, PathKind};
use blaze_unit_graph::{UnitKind, implicit};

/// reth's shape: the defaulted struct, `impl Cli` at the defaults, generic inherent impls, the
/// trait impls (clap's derive writes `impl<C: .., ..> FromArgMatches for Cli<C, Ext, Rpc,
/// SubCmd>`), and the glob re-export tempo names it through. Read both as a crate's root and as
/// a module's body: every path in it is `super::` or local.
const DEP: &str = "pub struct Matches;
pub struct Error;
pub trait Show {}
pub trait Spec { fn spec() -> Self; }
pub trait Ext { fn ext() -> Self; }
pub trait Rpc { fn rpc() -> Self; }
pub trait Sub { fn sub() -> Self; }
pub struct EthSpec;
pub struct NoArgs;
pub struct DefaultRpc;
pub struct NoSub;
impl Spec for EthSpec { fn spec() -> Self { EthSpec } }
impl Ext for NoArgs { fn ext() -> Self { NoArgs } }
impl Show for NoArgs {}
impl Rpc for DefaultRpc { fn rpc() -> Self { DefaultRpc } }
impl Sub for NoSub { fn sub() -> Self { NoSub } }
impl Show for NoSub {}
pub trait FromMatches: Sized { fn from_matches(m: &Matches) -> Result<Self, Error>; }
pub trait Factory { fn command() -> u8; }
pub trait ArgsLike: FromMatches { fn augment(n: u8) -> u8; }
pub trait Parser: FromMatches + Factory + Sized {
    fn parse() -> Self { match Self::from_matches(&Matches) { Ok(s) => s, Err(_) => loop {} } }
}
pub mod interface {
    use super::{ArgsLike, DefaultRpc, Error, EthSpec, Ext, Factory, FromMatches, Matches, NoArgs, NoSub, Parser, Rpc, Show, Spec, Sub};
    pub struct Cli<C: Spec = EthSpec, E: Ext + Show = NoArgs, R: Rpc = DefaultRpc, S: Sub + Show = NoSub> { pub c: C, pub e: E, pub r: R, pub s: S }
    impl Cli {
        pub fn parse_args() -> Self { Self::parse() }
        pub fn try_parse_args_from(n: u8) -> Result<Self, Error> { if n == 0 { Err(Error) } else { Self::from_matches(&Matches) } }
    }
    impl<C: Spec, E: Ext + Show, R: Rpc, S: Sub + Show> Cli<C, E, R, S> {
        pub fn as_node(&self) -> &C { &self.c }
    }
    impl<C: Spec, E: Ext + Show, R: Rpc, S: Sub + Show> FromMatches for Cli<C, E, R, S> {
        fn from_matches(m: &Matches) -> Result<Self, Error> { Ok(Cli { c: C::spec(), e: E::ext(), r: R::rpc(), s: S::sub() }) }
    }
    impl<C: Spec, E: Ext + Show, R: Rpc, S: Sub + Show> Factory for Cli<C, E, R, S> {
        fn command() -> u8 { 0 }
    }
    impl<C: Spec, E: Ext + Show, R: Rpc, S: Sub + Show> ArgsLike for Cli<C, E, R, S> {
        fn augment(n: u8) -> u8 { n }
    }
    impl<C: Spec, E: Ext + Show, R: Rpc, S: Sub + Show> Parser for Cli<C, E, R, S> {}
}
pub mod cli {
    pub use super::interface::*;
}
";

/// tempo's shape (crates/zones/node/src/cli.rs: `use reth_ethereum::cli::{Cli, ..}`, `use
/// clap::{.., Args, CommandFactory, FromArgMatches}`, its own parser, args and subcommand types
/// with the dependency's traits implemented, the alias `ZoneNodeCli`, the variant field
/// `Box<ZoneNodeCli>`).
const USER: &str = "use dep::cli::Cli;
use dep::{ArgsLike, DefaultRpc, Error, Ext, Factory, FromMatches, Matches, NoArgs, NoSub, Show, Spec, Sub, EthSpec};
fn main() {}
struct ZoneSpec;
struct ZoneArgs;
struct ZoneSub;
impl Spec for ZoneSpec { fn spec() -> Self { ZoneSpec } }
impl Ext for ZoneArgs { fn ext() -> Self { ZoneArgs } }
impl Show for ZoneArgs {}
impl Sub for ZoneSub { fn sub() -> Self { ZoneSub } }
impl Show for ZoneSub {}
type ZoneNodeCli = Cli<ZoneSpec, ZoneArgs, DefaultRpc, ZoneSub>;
enum ZoneCli { Node { cli: Box<ZoneNodeCli>, l1: u8 }, Dev(u8) }
enum Unboxed { Node { cli: ZoneNodeCli, l1: u8 } }
fn tempo_boxed(m: &Matches, l1: u8) -> Result<ZoneCli, Error> {
    Cli::from_matches(m).map(|cli| ZoneCli::Node { cli: Box::new(cli), l1: l1 })
}
fn tempo_unboxed(m: &Matches, l1: u8) -> Result<Unboxed, Error> {
    Cli::from_matches(m).map(|cli| Unboxed::Node { cli: cli, l1: l1 })
}
fn via_return(m: &Matches) -> Result<ZoneNodeCli, Error> { Cli::from_matches(m) }
fn via_annotation(m: &Matches) -> Result<ZoneNodeCli, Error> { let c: ZoneNodeCli = Cli::from_matches(m)?; Ok(c) }
fn alias_path(m: &Matches) -> Result<ZoneNodeCli, Error> { ZoneNodeCli::from_matches(m) }
fn defaults_inherent() -> Cli { Cli::parse_args() }
fn defaults_written() -> Cli<EthSpec, NoArgs, DefaultRpc, NoSub> { Cli::parse_args() }
fn bad_defaults_inherent() -> ZoneNodeCli { Cli::parse_args() }
";

/// The bodies that must check clean.
const CLEAN: [(&str, &str); 7] = [
    ("tempo_boxed", "tempo's form: `Cli::from_matches(m).map(|cli| ZoneCli::Node { cli: Box::new(cli), l1 })`"),
    ("tempo_unboxed", "the same into a field typed by the alias itself"),
    ("via_return", "`Cli::from_matches(m)` returned as `Result<ZoneNodeCli, Error>`"),
    ("via_annotation", "`let c: ZoneNodeCli = Cli::from_matches(m)?`"),
    ("alias_path", "`ZoneNodeCli::from_matches(m)`: the alias's path"),
    ("defaults_inherent", "`Cli::parse_args()` from `impl Cli` is `Cli` at its defaults"),
    ("defaults_written", "the same against the defaults written out"),
];

/// The bodies that must be E0308: `impl Cli`'s fn gives the defaults, never tempo's arguments.
const MISMATCH: [(&str, &str); 1] = [("bad_defaults_inherent", "`Cli::parse_args()` is `Cli<EthSpec, NoArgs, DefaultRpc, NoSub>`")];

/// The texts of the two-crate layout: the dependency's file for crate `dep`, the rest as
/// [`Inputs`] reads them (the program's files, the toolchain's packs).
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

/// Every listed body of crate 0 lowered and checked; the bodies that did not check as they
/// must, and one note per body.
fn outcomes<'n>(names: &'n Names<'n>, frame: &'n blaze_mem::Frame, user: &str) -> (Vec<String>, Vec<String>) {
    let sources = [SourceFile { path: "src/main.rs", text: user }];
    let mut front = Front2::new(names, frame, &sources, blaze_own::Alloc::global()).unwrap_or_else(|e| panic!("front2: {e}"));
    let mut store = Store::new(front.index.lang("sized"), front.frame, blaze_own::Alloc::global());
    let heap = blaze_own::testing::heap();
    let prepared = Prepares::new_in(blaze_own::testing::bump());
    let mut buckets = None;
    let mut failed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut bodies = Vec::new();
    for (name, proves, clean) in CLEAN.iter().map(|&(n, p)| (n, p, true)).chain(MISMATCH.iter().map(|&(n, p)| (n, p, false))) {
        let key = names::path(names, names.root(0), Namespace::Values, Written { kind: PathKind::Plain, segments: &[name] }, heap.alloc())
            .ok()
            .and_then(|f| f.whole(1))
            .map(|d| d.key)
            .unwrap_or_else(|| panic!("`{name}` from the crate's root"));
        if let Err(e) = store.ensure(names, &mut front.keys, Want::Fn(key)) {
            notes.push(format!("{name}: no head: {e:?}"));
            failed.push(name.to_string());
            continue;
        }
        match lower_fn(names, prepared, &mut front.keys, &mut store, key) {
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
        let mut records = Records { names, keys: &mut front.keys, index: &front.index, store: &mut store, buckets: &mut buckets, order: BuildOrder::AsAsked, consts: None };
        match (check_body(&mut records, body, &scope), *clean) {
            (Ok(done), true) if done.diags.is_empty() => notes.push(format!("{name}: clean")),
            (Ok(done), _) => {
                notes.push(format!("{name}: Ok with {} diagnostics; wanted {} ({proves})", done.diags.len(), if *clean { "none" } else { "a mismatch" }));
                failed.push(name.to_string());
            }
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
    if bodies.len() != CLEAN.len() + MISMATCH.len() && failed.is_empty() {
        failed.push(format!("{} of {} bodies lowered", bodies.len(), CLEAN.len() + MISMATCH.len()));
    }
    (failed, notes)
}

/// Expected: `CONTROL one crate: 8 bodies as they must be`, `CROSS two crates: 8 bodies as they
/// must be`, then `test cross_crate_value_path_args_are_inferred_not_defaulted ... ok`.
#[test]
fn cross_crate_value_path_args_are_inferred_not_defaulted() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let edition = edition::PackageEdition::of_manifest("2024").expect("a known edition");
    let total = CLEAN.len() + MISMATCH.len();

    // CONTROL: `DEP` as the module `dep` of the one crate.
    let one = format!("{USER}\nmod dep {{\n{DEP}\n}}\n");
    let one_files = [("/zone/src/main.rs".to_string(), one.clone())];
    let one_user = UserCrate { name: "zone", dir: "/zone/", root: "/zone/src/main.rs", files: &one_files, edition, panic: blaze_build::Panic::Unwind };
    let one_inputs = Inputs::new(&one_user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let one_crates = one_inputs.crates();
    let one_names = one_inputs.resolve(&one_crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (one crate): {e}"));
    let one_frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let (one_failed, one_notes) = outcomes(&one_names, &one_frame, &one);
    assert!(one_failed.is_empty(), "CONTROL one crate: bodies did not check as they must: {one_failed:?}\n{}", one_notes.join("\n"));
    println!("CONTROL one crate: {total} bodies as they must be");

    // CROSS: `DEP` as the library crate `dep`, the user crate's dependency.
    let user_files = [("/zone/src/main.rs".to_string(), USER.to_string())];
    let dep_files = [("/dep/src/lib.rs".to_string(), DEP.to_string())];
    let user = UserCrate { name: "zone", dir: "/zone/", root: "/zone/src/main.rs", files: &user_files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let mut crates: Vec<CrateInput<'_>> = inputs.crates();
    // The sysroot `Inputs::resolve` gives, by the units' library names (the crate's index is
    // its `CrateId`).
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(i as u32));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    assert!(sysroot.core.is_some() && sysroot.std.is_some(), "the toolchain at {shelf} has `core` and `std` units");
    let dep = crates.len();
    let user_deps: &'static [(&'static str, usize)] = Box::leak(vec![("dep", dep)].into_boxed_slice());
    let lib = implicit(edition::PackageEdition { syntax: edition::Edition::Edition2021, blaze: None }, &UnitKind::Lib);
    let cfg = crates[0].cfg;
    crates[0].deps = user_deps;
    crates.push(CrateInput {
        name: "dep",
        key: CrateItemKey(ContentId::of("blaze-ladder cli defaults cross test crate", b"dep")),
        edition: Edition::E2021,
        implicit: lib,
        root: "/dep/src/lib.rs",
        shown_from: "/dep/",
        features: &[],
        deps: &[],
        cfg,
        script: None,
    });
    let texts = WithDep { base: &inputs.texts, dep, files: &dep_files };
    let names = resolve_crates(&crates, sysroot, &texts, &Serial, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates (two crates): {e}"));
    let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the command\x27s frame");
    let (failed, notes) = outcomes(&names, &frame, USER);
    assert!(
        failed.is_empty(),
        "CROSS two crates: bodies through the dependency's `Cli` did not check as they must (the one-crate CONTROL did): {failed:?}\n{}",
        notes.join("\n")
    );
    println!("CROSS two crates: {total} bodies as they must be");
}
