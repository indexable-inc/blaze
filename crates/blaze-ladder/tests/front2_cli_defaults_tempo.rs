//! The second probe of tempo's census 14 E0308 (`zone_node::cli::<impl ZoneCli>::try_parse_from`
//! at `/package/1334/src/cli.rs:128`: at `cli`, found `Cli<EthereumChainSpecParser, NoArgs,
//! DefaultRpcModuleValidator, NoSubCmd>`, reth's four DEFAULTS, expected `Cli<ZoneChainSpecParser,
//! ZoneArgs, DefaultRpcModuleValidator, ZoneSubcommand>`). The first probe
//! (tests/front2_cli_defaults.rs, bare `Cli::parse(n)` with an unbounded impl) passes, so this one
//! adds what tempo has and it lacked, one form per body so the failing one names the cause:
//! - the trait impl is BOUNDED as clap's derive writes it (`impl<C: Spec, E: Ext + Show, R: Rpc,
//!   S: Sub + Show> FromMatches for Cli<C, E, R, S>`), each bound with several impls, the
//!   defaults' among them, so the bounds on inference variables are ambiguous until the use
//!   decides them;
//! - FOUR defaulted parameters, the third the same in found and expected (as tempo's `Rpc`);
//! - an inherent impl at the defaults (reth's `impl Cli { fn parse_args() .. }`) AND generic
//!   inherent impls, none naming the trait's fn;
//! - other traits in scope with fns of other names (clap's `CommandFactory`, `Args`);
//! - `Result::map` with a closure into an enum variant's field typed by an alias, through
//!   `Self::Node { .. }` in an impl of that enum (tempo's exact form, `Box` left out).
//!
//! The rule (the Reference, "Paths", "Paths in expressions", and "Generic parameters"): a value
//! path's segment written with no arguments leaves all of them to inference; defaults fill
//! only type paths (and a value path's segment written with some arguments).
//!
//! Every body's outcome, with the checker's found and expected types, is in the assert's
//! message, so a failure IS the probe output.
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

const TEMPO: &str = "fn main() {}
struct Matches;
struct Error;
trait Show {}
trait Spec { fn spec() -> Self; }
trait Ext { fn ext() -> Self; }
trait Rpc { fn rpc() -> Self; }
trait Sub { fn sub() -> Self; }
struct EthSpec; struct ZoneSpec; struct OtherSpec;
struct NoArgs; struct ZoneArgs; struct OtherArgs;
struct DefaultRpc; struct OtherRpc;
struct NoSub; struct ZoneSub; struct OtherSub;
impl Spec for EthSpec { fn spec() -> Self { EthSpec } }
impl Spec for ZoneSpec { fn spec() -> Self { ZoneSpec } }
impl Spec for OtherSpec { fn spec() -> Self { OtherSpec } }
impl Ext for NoArgs { fn ext() -> Self { NoArgs } }
impl Ext for ZoneArgs { fn ext() -> Self { ZoneArgs } }
impl Ext for OtherArgs { fn ext() -> Self { OtherArgs } }
impl Show for NoArgs {}
impl Show for ZoneArgs {}
impl Show for OtherArgs {}
impl Rpc for DefaultRpc { fn rpc() -> Self { DefaultRpc } }
impl Rpc for OtherRpc { fn rpc() -> Self { OtherRpc } }
impl Sub for NoSub { fn sub() -> Self { NoSub } }
impl Sub for ZoneSub { fn sub() -> Self { ZoneSub } }
impl Sub for OtherSub { fn sub() -> Self { OtherSub } }
impl Show for NoSub {}
impl Show for ZoneSub {}
impl Show for OtherSub {}
trait FromMatches: Sized { fn from_matches(m: &Matches) -> Result<Self, Error>; }
trait Factory { fn command() -> u8; }
trait ArgsLike { fn augment(n: u8) -> u8; }
struct Cli<C: Spec = EthSpec, E: Ext + Show = NoArgs, R: Rpc = DefaultRpc, S: Sub + Show = NoSub> { c: C, e: E, r: R, s: S }
impl Cli {
    fn parse_args() -> Self { Cli { c: EthSpec, e: NoArgs, r: DefaultRpc, s: NoSub } }
    fn try_parse_args_from(n: u8) -> Result<Self, Error> { if n == 0 { Err(Error) } else { Ok(Self::parse_args()) } }
}
impl<C: Spec, E: Ext + Show, R: Rpc, S: Sub + Show> Cli<C, E, R, S> {
    fn as_node(&self) -> &C { &self.c }
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
type ZoneNodeCli = Cli<ZoneSpec, ZoneArgs, DefaultRpc, ZoneSub>;
enum ZoneCli { Node { cli: ZoneNodeCli, l1: u8 }, Dev(u8) }
fn tempo_exact(m: &Matches, l1: u8) -> Result<ZoneCli, Error> {
    Cli::from_matches(m).map(|cli| ZoneCli::Node { cli: cli, l1: l1 })
}
fn via_let(m: &Matches) -> Result<ZoneCli, Error> {
    let r = Cli::from_matches(m);
    r.map(|cli| ZoneCli::Node { cli: cli, l1: 0 })
}
fn via_return(m: &Matches) -> Result<ZoneNodeCli, Error> { Cli::from_matches(m) }
fn via_annotation(m: &Matches) -> Result<ZoneNodeCli, Error> { let c: ZoneNodeCli = Cli::from_matches(m)?; Ok(c) }
fn alias_path(m: &Matches) -> Result<ZoneNodeCli, Error> { ZoneNodeCli::from_matches(m) }
fn defaults_inherent() -> Cli { Cli::parse_args() }
fn bad_defaults_inherent() -> ZoneNodeCli { Cli::parse_args() }
";

/// The bodies that must check clean (`<impl ZoneCli>::x` are found through the impl).
const CLEAN: [(&str, &str); 6] = [
    ("tempo_exact", "tempo's form: `Cli::from_matches(m).map(|cli| Self::Node { cli, l1 })`"),
    ("via_let", "the same through a `let` and a written variant path"),
    ("via_return", "`Cli::from_matches(m)` returned as `Result<ZoneNodeCli, Error>`"),
    ("via_annotation", "`let c: ZoneNodeCli = Cli::from_matches(m)?`"),
    ("alias_path", "`ZoneNodeCli::from_matches(m)`: the alias's path"),
    ("defaults_inherent", "`Cli::parse_args()` from `impl Cli` is `Cli` at its defaults"),
];

/// The bodies that must be E0308: `impl Cli`'s fn gives the defaults, never tempo's arguments.
const MISMATCH: [(&str, &str); 1] = [("bad_defaults_inherent", "`Cli::parse_args()` is `Cli<EthSpec, NoArgs, DefaultRpc, NoSub>`")];

/// Expected line after a fix: `test bounded_value_path_args_are_inferred_not_defaulted ... ok`.
/// Today, if this reduction reaches the cause, it FAILS with the message naming each body's
/// outcome (`tempo_exact: diagnostics [(Mismatch, .., "found .. Cli<EthSpec, NoArgs, DefaultRpc,
/// NoSub> .. expected .. Cli<ZoneSpec, ZoneArgs, DefaultRpc, ZoneSub> ..")]`); which of the six
/// clean bodies fail names the form at fault.
#[test]
fn bounded_value_path_args_are_inferred_not_defaulted() {
    let home = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home}/.blaze/v2/toolchain");
    let tc = Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"));
    let files = [("/clt/src/main.rs".to_string(), TEMPO.to_string())];
    let edition = edition::PackageEdition::of_manifest("blaze-2026.11").expect("a known edition");
    let user = UserCrate { name: "clt", dir: "/clt/", root: "/clt/src/main.rs", files: &files, edition, panic: blaze_build::Panic::Unwind };
    let inputs = Inputs::new(&user, &tc).unwrap_or_else(|e| panic!("the crates: {e}"));
    let crates = inputs.crates();
    let names = inputs.resolve(&crates, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("resolve_crates: {e}"));
    let sources = [SourceFile { path: "src/main.rs", text: TEMPO }];
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
    assert_eq!(bodies.len(), CLEAN.len() + MISMATCH.len(), "every body was lowered\n{}", notes.join("\n"));
    assert!(failed.is_empty(), "bodies through `Cli`'s defaulted parameters did not check as they must: {failed:?}\n{}", notes.join("\n"));
}
