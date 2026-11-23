//! The crate list as a record (D114 rule 1, the coordinator's ruling 2026-11-16 21:1x): what a
//! reader of the world needs of each crate, written ONCE by the job that makes the world (the
//! `UnitGraph` task) and read IN PLACE by a search, so a body edit makes no world: no shelf
//! opened, no package placed, no list made.
//!
//! One row a crate, IN THE WORLD'S ORDER (row `r` is the world's crate `r`: the toolchain's
//! first, then by key), so a world made later from the same graph (a names, script or
//! proc-macro task's, blaze-cli names.rs `Names::made_or_make`) is indexed as the list is; a
//! by-key table finds a row (strictly by key: two crates under one key are refused when
//! written, rule 21). Positions are tree paths (`/<tree>/...`, the world's), never a path of
//! this machine (rules 25, 71). Nothing of a crate's TEXT is here: a fact of a root file is read
//! from that file's outline record by whoever needs it.
//!
//! THE PLANNERS' FACTS (version 6, engine.md item 2: under a standing graph no world is made and
//! no graph decoded): what the names, scripts and units stages plan from, which the world alone
//! does not know, its maker's answers ([`ListFacts`]): each row's `test` cfg and whether its
//! records are public (the lock's checksum), each build-script module (placement, subject, the
//! script's root file), each build-script run (its unit, side, name, version and the runs its
//! `links` closure reads) and each proc-macro crate's module subject. Every one is a function of
//! the graph section and the lock the `UnitGraph` record is keyed by (its source holds the lock's
//! bytes, the layout's reads and the cfgs), and the list is a section of that record, so its out
//! moves with any of them (rule 21).
//!
//! THE TREES. A row's paths are `/<tree>/..`; tree 0 is the checkout and tree `k` (1 and up)
//! is the world's `k`th pack ([`crate::Texts::packs`]). The tree table names each pack by
//! content, as the world knew it when the list was made ([`crate::PackOrigin`]: a toolchain
//! pack by its shelf digest, a fetched package's record by its store key), with the BLAKE3 of
//! its files' listing (`PackView::tree`), which its opener checks: so a reader with no world (a
//! body job, blaze-cli body_types.rs) opens the very packs the world read, in place, and serves
//! every dependency's file text. The files read corrected (D58) follow: `(tree, path in it,
//! corrected text)`, read in place of the pack's bytes as the world reads them.
//!
//! Form (little endian), checked whole once at [`CrateList::open`], then read without checks:
//! a 60-byte header (magic, version, rows `n`, deps `d`, heap length, sysroot core, alloc, std,
//! bstd as row indices or `NONE`, trees `t`, corrected files `c`, modules `m`, runs `u`,
//! producers `q`, macros `p`), `n` rows of [`ROW`] bytes, `d` dependency entries of [`DEP`]
//! bytes, `t` trees of [`TREE`] bytes, `c` corrected files of [`CORRECTED`] bytes, the by-key
//! table (`n` rows, 4 bytes each), `m` modules of [`MODULE`] bytes, `u` runs of [`RUN`] bytes,
//! `q` producers and `p` macros of [`SUBJECT`] bytes, then the string heap.

use blaze_id::{ContentId, CrateItemKey};
use blaze_own::{AVec, Alloc};
use blaze_scope::crate_env::{BinNames, EnvFacts, SeenRoot};
use blaze_scope::resolve::SourceTexts;
use blaze_unit_graph::PackageMetaVars;

use crate::{Origin, PackOrigin, Side, World};

const MAGIC: &[u8; 4] = b"BZCL";
/// 2: the tree table and the corrected files (a body job serves its dependencies' texts from
/// them); a version-1 list is refused by name at [`CrateList::open`]. 3: each row's
/// environment facts ([`CrateList::env_facts`]: a body's `env!` is answered from them). 4: the
/// package's `CARGO_PKG_*` metadata in each row ([`META_AT`]). 6: rows in the world's order
/// with a by-key table, the [`TEST`] and [`PUBLIC`] flags, and the planners' tables (modules,
/// runs, producers, macros); a version-5 list is refused by name, so its unit graph is made
/// again (D160). 7: each tree row's pack kind ([`TreeRow::kind`], the byte after the origin's),
/// so a reader of the list knows a tree's kind without mapping its pack; a version-6 list is
/// refused by name and made again. 8: each row's package binaries ([`BIN_EXES_AT`]: an
/// integration test's `CARGO_BIN_EXE_<name>`); a version-7 list is refused by name and made
/// again.
pub const VERSION: u16 = 8;
/// magic 4, version 2, pad 2, `n` 4, `d` 4, heap 4, sysroot 16, `t` 4, `c` 4, `m` 4, `u` 4,
/// `q` 4, `p` 4.
pub const HEADER_LEN: usize = 60;
/// A build-script module: row 4, placement 1 (its maker's byte, never 0), pad 3, subject 32,
/// the script's root file (package-relative) 8.
pub const MODULE: usize = 48;
/// A build-script run: subject 32, its unit 4, side 1, pad 3, the unit's package name 8 and
/// version 8, its producers' start 4 and count 4.
pub const RUN: usize = 64;
/// A producer (a `Script` run's subject) or a proc-macro crate's module subject.
pub const SUBJECT: usize = 32;
/// origin 1 ([`SHELF`] or [`SOURCE`]), the pack's kind 1 ([`pack_kind_byte`]), pad 2, its name
/// 32 (shelf digest or store key), its files' listing BLAKE3 32.
pub const TREE: usize = 68;

/// A pack kind's byte in a tree row (version 7): one per [`blaze_fetch::pack::Kind`], never 0.
fn pack_kind_byte(kind: blaze_fetch::pack::Kind) -> u8 {
    match kind {
        blaze_fetch::pack::Kind::Crate => 1,
        blaze_fetch::pack::Kind::Git => 2,
        blaze_fetch::pack::Kind::Tree => 3,
    }
}

/// A tree row's pack kind byte read back; none for a byte no kind writes.
fn pack_kind_of(byte: u8) -> Option<blaze_fetch::pack::Kind> {
    match byte {
        1 => Some(blaze_fetch::pack::Kind::Crate),
        2 => Some(blaze_fetch::pack::Kind::Git),
        3 => Some(blaze_fetch::pack::Kind::Tree),
        _ => None,
    }
}
/// tree 4, path 8, text 8.
pub const CORRECTED: usize = 20;
/// A tree whose pack is on the toolchain's shelf ([`PackOrigin::Shelf`]).
const SHELF: u8 = 1;
/// A tree whose pack is a record of the sources store ([`PackOrigin::Source`]).
const SOURCE: u8 = 2;
/// key 32, name 8, root 8, shown_from 8, deps 8, side 1, flags 1, script kind 1, pad 1, script
/// subject 32, then the crate's environment facts (blaze-scope `EnvFacts`, from its
/// `CrateEnv::facts`): crate name 8, package name 8, package version 8, binary name 8, the
/// member's dir 8, env flags 1 ([`ENV_PACKAGE`] ..), pad 3, then the package's metadata: 8
/// strings of 8 in `PackageMetaVars::NAMES`' order (all `""` without [`ENV_META`]), then the
/// package's binaries 8 (blaze-scope `BinNames`, joined by `\n`; `""` for a crate not a test or
/// bench).
pub const ROW: usize = 216;
/// Where a row's flags byte is ([`TEST`], [`PUBLIC`] ..): after the key, the five ranges and the
/// side.
pub const FLAGS_AT: usize = 65;
/// Where a row's environment facts start.
const ENV_AT: usize = 100;
/// Where a row's package metadata starts.
const META_AT: usize = 144;
/// Where a row's package binaries are (after the 8 metadata strings).
const BIN_EXES_AT: usize = META_AT + 64;

/// The crate has a package (name and version at 108, 116); a toolchain crate has none.
const ENV_PACKAGE: u8 = 1;
/// A binary: its name at 124.
const ENV_BIN: u8 = 2;
/// Its package has a build script.
const ENV_BUILD_SCRIPT: u8 = 4;
/// An integration test or a bench.
const ENV_TEST_OR_BENCH: u8 = 8;
/// It sees its workspace member's dir (at 132; D96 `SeenRoot::Member`), else `/package`.
const ENV_MEMBER: u8 = 16;
/// Its package's metadata is known (at [`META_AT`]); without it, the `CARGO_PKG_*` metadata
/// names are unknown.
const ENV_META: u8 = 32;
const ENV_FLAGS: u8 = ENV_PACKAGE | ENV_BIN | ENV_BUILD_SCRIPT | ENV_TEST_OR_BENCH | ENV_MEMBER | ENV_META;
/// name 8, row 4.
pub const DEP: usize = 12;
/// No row (a sysroot crate the world has not).
pub const NONE: u32 = u32::MAX;

const PROC_MACRO: u8 = 1;
const TOOLCHAIN: u8 = 2;
const MEMBER: u8 = 4;
const REFUSED: u8 = 8;
/// Its cfg holds `test` with no value (the world crate's `CrateInput::cfg`).
pub const TEST: u8 = 16;
/// Its records may hold nothing of a project ([`ListFacts::public`]).
pub const PUBLIC: u8 = 32;
const FLAGS: u8 = PROC_MACRO | TOOLCHAIN | MEMBER | REFUSED | TEST | PUBLIC;

/// A build-script module as its maker answers it ([`ListFacts::module`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Module<'s> {
    /// The run's placement, the maker's own byte (never 0).
    pub ran_as: u8,
    pub subject: [u8; 32],
    /// The build script's root file, package-relative.
    pub root: &'s str,
}

/// A build-script run as its maker answers it ([`ListFacts::runs`]).
#[derive(Clone, Copy, Debug)]
pub struct Run<'s> {
    pub subject: [u8; 32],
    /// The build-script unit of the graph.
    pub unit: u32,
    pub side: Side,
    /// The unit's package name and version.
    pub name: &'s str,
    pub version: &'s str,
    /// The `Script` runs its `links` closure reads, by subject.
    pub producers: &'s [[u8; 32]],
}

/// What the list holds that the world alone does not know: the planners' facts, answered by the
/// list's maker from the graph and the lock the world was made from (module docs, "The planners'
/// facts").
pub trait ListFacts {
    /// The name of crate `at`'s `Script` part (its kind byte and 32-byte subject) when it has a
    /// build script whose run is a task.
    fn script(&self, at: usize) -> Option<[u8; 33]>;
    /// Whether crate `at`'s records may hold nothing of a project.
    fn public(&self, at: usize) -> bool;
    /// The module crate `at`'s package's build script is looked up by.
    fn module(&self, at: usize) -> Option<Module<'_>>;
    /// Each run the crates read, in order of subject, each once.
    fn runs(&self, each: &mut dyn FnMut(Run<'_>));
    /// Each proc-macro crate's module subject, in order, each once.
    fn macros(&self, each: &mut dyn FnMut([u8; 32]));
}

/// The facts of a world with no graph beside it (the tests' worlds): its `Script` names from
/// `script_of`, nothing public, no module, run or macro.
struct ScriptsOnly<'s>(&'s dyn Fn(usize) -> Option<[u8; 33]>);

impl ListFacts for ScriptsOnly<'_> {
    fn script(&self, at: usize) -> Option<[u8; 33]> {
        (self.0)(at)
    }
    fn public(&self, _: usize) -> bool {
        false
    }
    fn module(&self, _: usize) -> Option<Module<'_>> {
        None
    }
    fn runs(&self, _: &mut dyn FnMut(Run<'_>)) {}
    fn macros(&self, _: &mut dyn FnMut([u8; 32])) {}
}

/// A build-script module's entry, read in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleRow<'b> {
    /// The row whose package's build script it is.
    pub row: usize,
    pub ran_as: u8,
    pub subject: [u8; 32],
    pub root: &'b str,
}

/// A build-script run's entry, read in place.
#[derive(Clone, Copy, Debug)]
pub struct RunRow<'b> {
    pub subject: [u8; 32],
    pub unit: u32,
    pub side: Side,
    pub name: &'b str,
    pub version: &'b str,
    producers: &'b [u8],
}

impl<'b> RunRow<'b> {
    /// The `Script` runs its `links` closure reads, by subject, as written.
    pub fn producers(&self) -> impl Iterator<Item = [u8; 32]> + 'b {
        self.producers.chunks_exact(SUBJECT).map(subject_of)
    }
}

fn subject_of(b: &[u8]) -> [u8; 32] {
    let mut s = [0; 32];
    s.copy_from_slice(&b[..32]);
    s
}

/// What a reader of a crate graph asks: the world made fresh, or the list read in place. The
/// check scope's rule ([`crate::check_scope::check_scope_in`]) is written once over it (rule 78).
/// `'n`: what its names borrow (the world's, or the list's bytes).
pub trait CrateGraph<'n> {
    fn len(&self) -> usize;
    fn name(&self, at: usize) -> &'n str;
    fn side(&self, at: usize) -> Side;
    fn proc_macro(&self, at: usize) -> bool;
    fn toolchain(&self, at: usize) -> bool;
    fn refused(&self, at: usize) -> bool;
    /// The crates `at` depends on, by index, in its own order.
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_;
    /// `core`, `alloc`, Rust's `std`, bstd.
    fn sysroot(&self) -> [Option<usize>; 4];
}

impl<'w> CrateGraph<'w> for World<'w> {
    fn len(&self) -> usize {
        self.crates.len()
    }
    fn name(&self, at: usize) -> &'w str {
        self.crates[at].input.name
    }
    fn side(&self, at: usize) -> Side {
        self.crates[at].side
    }
    fn proc_macro(&self, at: usize) -> bool {
        self.crates[at].proc_macro
    }
    fn toolchain(&self, at: usize) -> bool {
        matches!(self.crates[at].origin, Origin::Toolchain(_))
    }
    fn refused(&self, at: usize) -> bool {
        self.refusal(at).is_some()
    }
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        self.crates[at].input.deps.iter().map(|(_, dep)| *dep)
    }
    fn sysroot(&self) -> [Option<usize>; 4] {
        let s = self.sysroot;
        [s.core, s.alloc, s.std, s.bstd].map(|id| id.map(|id| id.0 as usize))
    }
}

/// Why a list was not written or does not open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    /// Two crates of the world under one key (rule 21: the key misses an input).
    SameKey { first: usize, second: usize },
    /// A count or a length past what the form holds (`u32`).
    TooLarge { what: &'static str },
    /// The bytes are not a list of this form: what was wrong, and at which byte.
    Form { what: &'static str, at: usize },
    /// A list of another version of the form (one with no tree table is version 1).
    Version { found: u16 },
    /// The world's packs and their origins are not one to one (`Texts::origins`).
    Origins { packs: usize, origins: usize },
    /// The world holds no environment for its crate `at` (`Texts::crate_env`).
    NoEnv { at: usize },
}

impl core::fmt::Display for ListError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ListError::Version { found } => {
                write!(f, "a crate list of version {found}, not {VERSION} (crate_list.rs `VERSION`): written by another blaze, so its unit graph is made again")
            }
            ListError::Origins { packs, origins } => write!(f, "the world has {packs} packs and {origins} pack origins (blaze-world `Texts::origins`)"),
            ListError::SameKey { first, second } => write!(f, "crates {first} and {second} of the world have one key (rule 21: its key misses an input)"),
            ListError::TooLarge { what } => write!(f, "the crate list's {what} is past what its form holds"),
            ListError::Form { what, at } => write!(f, "not a crate list of this form: {what} (at byte {at})"),
            ListError::NoEnv { at } => write!(f, "crate {at} of the world has no environment (blaze-world `Texts::crate_env`)"),
        }
    }
}

fn u32_of(n: usize, what: &'static str) -> Result<u32, ListError> {
    u32::try_from(n).map_err(|_| ListError::TooLarge { what })
}

/// The list of a world with no graph beside it: `script_of(at)` is the name of crate `at`'s
/// `Script` part (its kind byte and 32-byte subject) when it has a build script whose run is a
/// task; nothing public, no module, run or macro ([`encode_with`] for a world's maker).
pub fn encode<'a>(world: &World<'_>, script_of: &dyn Fn(usize) -> Option<[u8; 33]>, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ListError> {
    encode_with(world, &ScriptsOnly(script_of), alloc)
}

/// Whether crate `at` of `world` has `test` among its cfg, with no value ([`TEST`]).
pub fn cfg_test(world: &World<'_>, at: usize) -> bool {
    world.crates[at].input.cfg.iter().any(|(atom, value)| *atom == "test" && value.is_none())
}

/// The list of `world`, its rows in the world's order, and the planners' facts `facts` (module
/// docs). Every table is written in an order of the world's or of a subject's, never of a map
/// or of this machine (rule 25): two makers of one world write equal bytes.
pub fn encode_with<'a>(world: &World<'_>, facts: &dyn ListFacts, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, ListError> {
    let crates = &world.crates;
    let n = crates.len();
    u32_of(n, "row count")?;
    // Row `r` is the world's crate `r`; the by-key table lists the rows sorted by key.
    let mut order: AVec<'_, usize> = AVec::with_capacity_in(n, alloc);
    order.extend(0..n);
    order.sort_unstable_by_key(|at| crates[*at].input.key.0.0);
    if let Some(pair) = order.windows(2).find(|pair| crates[pair[0]].input.key == crates[pair[1]].input.key) {
        return Err(ListError::SameKey { first: pair[0].min(pair[1]), second: pair[0].max(pair[1]) });
    }
    let mut row_of: AVec<'_, u32> = AVec::with_capacity_in(n, alloc);
    row_of.extend((0..n).map(|at| at as u32));
    let mut heap: AVec<'_, u8> = AVec::new_in(alloc);
    let mut rows: AVec<'_, u8> = AVec::with_capacity_in(n * ROW, alloc);
    let mut deps: AVec<'_, u8> = AVec::new_in(alloc);
    let mut d = 0usize;
    for at in 0..n {
        let c = &crates[at];
        rows.extend_from_slice(&c.input.key.0.0);
        rows.extend_from_slice(&text(c.input.name, &mut heap)?);
        rows.extend_from_slice(&text(c.input.root, &mut heap)?);
        rows.extend_from_slice(&text(c.input.shown_from, &mut heap)?);
        rows.extend_from_slice(&u32_of(d, "dependency count")?.to_le_bytes());
        rows.extend_from_slice(&u32_of(c.input.deps.len(), "dependency count")?.to_le_bytes());
        for (name, dep) in c.input.deps.iter() {
            deps.extend_from_slice(&text(name, &mut heap)?);
            deps.extend_from_slice(&row_of[*dep].to_le_bytes());
            d += 1;
        }
        rows.push(match c.side {
            Side::Target => 0,
            Side::Macro => 1,
        });
        let mut flags = 0;
        if c.proc_macro {
            flags |= PROC_MACRO;
        }
        if matches!(c.origin, Origin::Toolchain(_)) {
            flags |= TOOLCHAIN;
        }
        if c.member {
            flags |= MEMBER;
        }
        if world.refusal(at).is_some() {
            flags |= REFUSED;
        }
        if cfg_test(world, at) {
            flags |= TEST;
        }
        if facts.public(at) {
            flags |= PUBLIC;
        }
        rows.push(flags);
        match facts.script(at) {
            Some(name) => {
                // A kind byte of 0 would read as "no script": the name's kind is never 0 here.
                if name[0] == 0 {
                    return Err(ListError::Form { what: "a script part's kind byte is 0", at: rows.len() });
                }
                rows.push(name[0]);
                rows.push(0);
                rows.extend_from_slice(&name[1..]);
            }
            None => {
                rows.extend_from_slice(&[0; 34]);
            }
        }
        // The environment's facts, as the world made it (its `CrateEnv`, never joined with a
        // script's record: that is read by each reader as its own input).
        let env = world.texts.crate_env(at).ok_or(ListError::NoEnv { at })?.facts();
        let mut env_flags = 0;
        rows.extend_from_slice(&text(env.crate_name, &mut heap)?);
        let (package, version) = (c.package, c.version);
        if env.package.is_some() {
            env_flags |= ENV_PACKAGE;
        }
        rows.extend_from_slice(&text(package, &mut heap)?);
        rows.extend_from_slice(&text(version, &mut heap)?);
        if env.bin.is_some() {
            env_flags |= ENV_BIN;
        }
        rows.extend_from_slice(&text(env.bin.unwrap_or(""), &mut heap)?);
        let dir = match env.seen {
            SeenRoot::Package => "",
            SeenRoot::Member { dir } => {
                env_flags |= ENV_MEMBER;
                dir
            }
        };
        rows.extend_from_slice(&text(dir, &mut heap)?);
        if env.build_script {
            env_flags |= ENV_BUILD_SCRIPT;
        }
        if env.test_or_bench {
            env_flags |= ENV_TEST_OR_BENCH;
        }
        if env.meta.is_some() {
            env_flags |= ENV_META;
        }
        rows.extend_from_slice(&[env_flags, 0, 0, 0]);
        for value in env.meta.unwrap_or_default().values() {
            rows.extend_from_slice(&text(value, &mut heap)?);
        }
        rows.extend_from_slice(&text(env.bin_exes.joined(), &mut heap)?);
    }
    // The trees past the checkout, in the world's order (tree `k` is pack `k - 1`).
    let (packs, origins) = (world.texts.packs(), world.texts.origins());
    if packs.len() != origins.len() {
        return Err(ListError::Origins { packs: packs.len(), origins: origins.len() });
    }
    let mut trees: AVec<'_, u8> = AVec::with_capacity_in(packs.len() * TREE, alloc);
    for (pack, origin) in packs.iter().zip(origins.iter()) {
        let (kind, name) = match origin {
            PackOrigin::Shelf { digest } => (SHELF, *digest),
            PackOrigin::Source { key } => (SOURCE, key.0),
        };
        trees.extend_from_slice(&[kind, pack_kind_byte(pack.kind()), 0, 0]);
        trees.extend_from_slice(&name);
        trees.extend_from_slice(&pack.tree().0);
    }
    let corrected_files = world.texts.corrected();
    let mut corrected: AVec<'_, u8> = AVec::with_capacity_in(corrected_files.len() * CORRECTED, alloc);
    for (tree, path, text_of) in corrected_files.iter() {
        corrected.extend_from_slice(&tree.to_le_bytes());
        corrected.extend_from_slice(&text(path, &mut heap)?);
        corrected.extend_from_slice(&text(text_of, &mut heap)?);
    }
    let mut by_key: AVec<'_, u8> = AVec::with_capacity_in(n * 4, alloc);
    for at in order.iter() {
        by_key.extend_from_slice(&(*at as u32).to_le_bytes());
    }
    // The modules, by (subject, row): a module two crates' packages share is two entries.
    let mut found: AVec<'_, (usize, Module<'_>)> = AVec::new_in(alloc);
    for at in 0..n {
        if let Some(module) = facts.module(at) {
            if module.ran_as == 0 {
                return Err(ListError::Form { what: "a module's placement byte is 0", at });
            }
            found.push((at, module));
        }
    }
    found.sort_unstable_by(|a, b| (a.1.subject, a.0).cmp(&(b.1.subject, b.0)));
    let mut modules: AVec<'_, u8> = AVec::with_capacity_in(found.len() * MODULE, alloc);
    for (at, module) in found.iter() {
        modules.extend_from_slice(&(*at as u32).to_le_bytes());
        modules.extend_from_slice(&[module.ran_as, 0, 0, 0]);
        modules.extend_from_slice(&module.subject);
        modules.extend_from_slice(&text(module.root, &mut heap)?);
    }
    let mut runs: AVec<'_, u8> = AVec::new_in(alloc);
    let mut producers: AVec<'_, u8> = AVec::new_in(alloc);
    let mut last: Option<[u8; 32]> = None;
    let mut bad: Result<(), ListError> = Ok(());
    facts.runs(&mut |run| {
        if bad.is_err() {
            return;
        }
        if last.is_some_and(|last| last >= run.subject) {
            bad = Err(ListError::Form { what: "the runs are not strictly by subject", at: runs.len() });
            return;
        }
        last = Some(run.subject);
        let one = (|| -> Result<(), ListError> {
            runs.extend_from_slice(&run.subject);
            runs.extend_from_slice(&run.unit.to_le_bytes());
            runs.extend_from_slice(&[side_byte(run.side), 0, 0, 0]);
            runs.extend_from_slice(&text(run.name, &mut heap)?);
            runs.extend_from_slice(&text(run.version, &mut heap)?);
            runs.extend_from_slice(&u32_of(producers.len() / SUBJECT, "producer count")?.to_le_bytes());
            runs.extend_from_slice(&u32_of(run.producers.len(), "producer count")?.to_le_bytes());
            for producer in run.producers.iter() {
                producers.extend_from_slice(producer);
            }
            Ok(())
        })();
        bad = one;
    });
    bad?;
    let mut macros: AVec<'_, u8> = AVec::new_in(alloc);
    let mut last: Option<[u8; 32]> = None;
    let mut unsorted = false;
    facts.macros(&mut |subject| {
        unsorted |= last.is_some_and(|last| last >= subject);
        last = Some(subject);
        macros.extend_from_slice(&subject);
    });
    if unsorted {
        return Err(ListError::Form { what: "the macros are not strictly by subject", at: 0 });
    }
    let tables = by_key.len() + modules.len() + runs.len() + producers.len() + macros.len();
    let mut out: AVec<'a, u8> = AVec::with_capacity_in(HEADER_LEN + rows.len() + deps.len() + trees.len() + corrected.len() + tables + heap.len(), alloc);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&u32_of(n, "row count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(d, "dependency count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(heap.len(), "string heap")?.to_le_bytes());
    let s = world.sysroot;
    for id in [s.core, s.alloc, s.std, s.bstd] {
        out.extend_from_slice(&id.map_or(NONE, |id| row_of.get(id.0 as usize).copied().unwrap_or(NONE)).to_le_bytes());
    }
    out.extend_from_slice(&u32_of(packs.len(), "tree count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(corrected_files.len(), "corrected file count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(found.len(), "module count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(runs.len() / RUN, "run count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(producers.len() / SUBJECT, "producer count")?.to_le_bytes());
    out.extend_from_slice(&u32_of(macros.len() / SUBJECT, "macro count")?.to_le_bytes());
    out.extend_from_slice(&rows);
    out.extend_from_slice(&deps);
    out.extend_from_slice(&trees);
    out.extend_from_slice(&corrected);
    out.extend_from_slice(&by_key);
    out.extend_from_slice(&modules);
    out.extend_from_slice(&runs);
    out.extend_from_slice(&producers);
    out.extend_from_slice(&macros);
    out.extend_from_slice(&heap);
    Ok(out)
}

fn side_byte(side: Side) -> u8 {
    match side {
        Side::Target => 0,
        Side::Macro => 1,
    }
}

/// Where tree `k` (1 and up) of a list is kept, and its files' identity.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TreeRow {
    pub origin: PackOrigin,
    /// The pack's kind (`PackView::kind`), as the list's world mapped it: read from the row, so
    /// a reader that needs only the kind (engine_scope_reads.rs `ReadGrains::new`) maps no pack.
    pub kind: blaze_fetch::pack::Kind,
    /// The BLAKE3 of the pack's files' listing (`PackView::tree`): its opener checks the pack it
    /// mapped is this one.
    pub files: [u8; 32],
}

/// `s` into `heap`: its (offset, length), each a `u32`.
fn text(s: &str, heap: &mut AVec<'_, u8>) -> Result<[u8; 8], ListError> {
    let off = u32_of(heap.len(), "string heap")?;
    let len = u32_of(s.len(), "string length")?;
    heap.extend_from_slice(s.as_bytes());
    let mut b = [0; 8];
    b[..4].copy_from_slice(&off.to_le_bytes());
    b[4..].copy_from_slice(&len.to_le_bytes());
    Ok(b)
}

/// A list read in place: every range checked once, at [`CrateList::open`].
#[derive(Clone, Copy)]
pub struct CrateList<'b> {
    rows: &'b [u8],
    deps: &'b [u8],
    trees: &'b [u8],
    corrected: &'b [u8],
    by_key: &'b [u8],
    modules: &'b [u8],
    runs: &'b [u8],
    producers: &'b [u8],
    macros: &'b [u8],
    heap: &'b str,
    n: usize,
    sysroot: [u32; 4],
}

/// The first of `0..len` for which `before` is false (`before` true on a prefix).
fn partition(len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) { lo = mid + 1 } else { hi = mid }
    }
    lo
}

fn le32(b: &[u8], at: usize) -> u32 {
    let mut w = [0; 4];
    w.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(w)
}

impl<'b> CrateList<'b> {
    /// The list in `bytes`, checked whole: the header, the lengths, every string range in the
    /// heap and on a character boundary, every dependency's row, the by-key table strictly by
    /// key over every row once, each module's row, each run's producers, the runs and macros
    /// strictly by subject and the modules by (subject, row).
    pub fn open(bytes: &'b [u8]) -> Result<CrateList<'b>, ListError> {
        let form = |what: &'static str, at: usize| ListError::Form { what, at };
        if bytes.len() < 6 || &bytes[..4] != MAGIC {
            return Err(form("no magic", 0));
        }
        let found = u16::from_le_bytes([bytes[4], bytes[5]]);
        if found != VERSION {
            return Err(ListError::Version { found });
        }
        if bytes.len() < HEADER_LEN {
            return Err(form("shorter than its header", 0));
        }
        let n = le32(bytes, 8) as usize;
        let d = le32(bytes, 12) as usize;
        let h = le32(bytes, 16) as usize;
        let t = le32(bytes, 36) as usize;
        let c = le32(bytes, 40) as usize;
        let m = le32(bytes, 44) as usize;
        let u = le32(bytes, 48) as usize;
        let q = le32(bytes, 52) as usize;
        let p = le32(bytes, 56) as usize;
        let after = |start: usize, count: usize, size: usize, at: usize| {
            count.checked_mul(size).and_then(|len| start.checked_add(len)).ok_or(form("a table's length overflows", at))
        };
        let rows_end = after(HEADER_LEN, n, ROW, 8)?;
        let deps_end = after(rows_end, d, DEP, 12)?;
        let trees_end = after(deps_end, t, TREE, 36)?;
        let corrected_end = after(trees_end, c, CORRECTED, 40)?;
        let by_key_end = after(corrected_end, n, 4, 8)?;
        let modules_end = after(by_key_end, m, MODULE, 44)?;
        let runs_end = after(modules_end, u, RUN, 48)?;
        let producers_end = after(runs_end, q, SUBJECT, 52)?;
        let macros_end = after(producers_end, p, SUBJECT, 56)?;
        if macros_end.checked_add(h) != Some(bytes.len()) {
            return Err(form("the lengths do not add up to the bytes", 16));
        }
        let heap = core::str::from_utf8(&bytes[macros_end..]).map_err(|_| form("the string heap is not UTF-8", macros_end))?;
        let mut sysroot = [NONE; 4];
        for (i, s) in sysroot.iter_mut().enumerate() {
            *s = le32(bytes, 20 + 4 * i);
            if *s != NONE && *s as usize >= n {
                return Err(form("a sysroot row past the rows", 20 + 4 * i));
            }
        }
        let list = CrateList {
            rows: &bytes[HEADER_LEN..rows_end],
            deps: &bytes[rows_end..deps_end],
            trees: &bytes[deps_end..trees_end],
            corrected: &bytes[trees_end..corrected_end],
            by_key: &bytes[corrected_end..by_key_end],
            modules: &bytes[by_key_end..modules_end],
            runs: &bytes[modules_end..runs_end],
            producers: &bytes[runs_end..producers_end],
            macros: &bytes[producers_end..macros_end],
            heap,
            n,
            sysroot,
        };
        let text_ok = |b: &[u8], at: usize| {
            let (off, len) = (le32(b, at) as usize, le32(b, at + 4) as usize);
            off.checked_add(len).is_some_and(|end| end <= heap.len() && heap.is_char_boundary(off) && heap.is_char_boundary(end))
        };
        for r in 0..n {
            let row = list.row(r);
            for at in [32, 40, 48] {
                if !text_ok(row, at) {
                    return Err(form("a row's string is outside the heap", HEADER_LEN + r * ROW + at));
                }
            }
            let (start, count) = (le32(row, 56) as usize, le32(row, 60) as usize);
            if start.checked_add(count).is_none_or(|end| end > d) {
                return Err(form("a row's dependencies are outside the table", HEADER_LEN + r * ROW + 56));
            }
            if row[64] > 1 {
                return Err(form("a side that is neither target nor macro", HEADER_LEN + r * ROW + 64));
            }
            for at in [ENV_AT, ENV_AT + 8, ENV_AT + 16, ENV_AT + 24, ENV_AT + 32].into_iter().chain((0..8).map(|i| META_AT + 8 * i)).chain([BIN_EXES_AT]) {
                if !text_ok(row, at) {
                    return Err(form("a row's environment string is outside the heap", HEADER_LEN + r * ROW + at));
                }
            }
            // The binaries are checked here once, so the facts read later need no check.
            if BinNames::new(list.text(row, BIN_EXES_AT)).is_none() {
                return Err(form("a row's package binaries are not sorted names, each once", HEADER_LEN + r * ROW + BIN_EXES_AT));
            }
            if row[ENV_AT + 40] & !ENV_FLAGS != 0 || row[ENV_AT + 41..ENV_AT + 44] != [0, 0, 0] {
                return Err(form("a row's environment flags name no fact", HEADER_LEN + r * ROW + ENV_AT + 40));
            }
            // The member's dir is checked here once, so the facts read later need no check.
            if row[ENV_AT + 40] & ENV_MEMBER != 0 && SeenRoot::member(list.text(row, ENV_AT + 32)).is_err() {
                return Err(form("a row's member dir is not a plain path under the workspace", HEADER_LEN + r * ROW + ENV_AT + 32));
            }
            if row[FLAGS_AT] & !FLAGS != 0 {
                return Err(form("a row's flags name no fact", HEADER_LEN + r * ROW + FLAGS_AT));
            }
        }
        // The by-key table: every entry a row, strictly by key, so each row once (n entries).
        for e in 0..n {
            let r = le32(list.by_key, e * 4) as usize;
            if r >= n {
                return Err(form("the by-key table names no row", corrected_end + e * 4));
            }
            if e > 0 && list.row(le32(list.by_key, (e - 1) * 4) as usize)[..32] >= list.row(r)[..32] {
                return Err(form("the by-key table is not strictly by key", corrected_end + e * 4));
            }
        }
        for e in 0..m {
            let module = &list.modules[e * MODULE..(e + 1) * MODULE];
            if le32(module, 0) as usize >= n || module[4] == 0 || module[5..8] != [0, 0, 0] || !text_ok(module, 40) {
                return Err(form("a module names no row, no placement or no string", by_key_end + e * MODULE));
            }
            if e > 0 {
                let before = &list.modules[(e - 1) * MODULE..e * MODULE];
                if (&before[8..40], le32(before, 0)) >= (&module[8..40], le32(module, 0)) {
                    return Err(form("the modules are not strictly by subject and row", by_key_end + e * MODULE));
                }
            }
        }
        for e in 0..u {
            let run = &list.runs[e * RUN..(e + 1) * RUN];
            let (start, count) = (le32(run, 56) as usize, le32(run, 60) as usize);
            if run[36] > 1 || run[37..40] != [0, 0, 0] || !text_ok(run, 40) || !text_ok(run, 48) || start.checked_add(count).is_none_or(|end| end > q) {
                return Err(form("a run names no side, no string or producers outside the table", modules_end + e * RUN));
            }
            if e > 0 && list.runs[(e - 1) * RUN..(e - 1) * RUN + 32] >= run[..32] {
                return Err(form("the runs are not strictly by subject", modules_end + e * RUN));
            }
        }
        for e in 1..p {
            if list.macros[(e - 1) * SUBJECT..e * SUBJECT] >= list.macros[e * SUBJECT..(e + 1) * SUBJECT] {
                return Err(form("the macros are not strictly by subject", producers_end + e * SUBJECT));
            }
        }
        for e in 0..d {
            let dep = &list.deps[e * DEP..(e + 1) * DEP];
            if !text_ok(dep, 0) || le32(dep, 8) as usize >= n {
                return Err(form("a dependency names no row or no string", rows_end + e * DEP));
            }
        }
        for k in 0..t {
            let tree = &list.trees[k * TREE..(k + 1) * TREE];
            if !matches!(tree[0], SHELF | SOURCE) || tree[2..4] != [0, 0] {
                return Err(form("a tree's kind is neither shelf nor source", deps_end + k * TREE));
            }
            if pack_kind_of(tree[1]).is_none() {
                return Err(form("a tree's pack kind is none of crate, git or tree", deps_end + k * TREE + 1));
            }
        }
        for e in 0..c {
            let file = &list.corrected[e * CORRECTED..(e + 1) * CORRECTED];
            let tree = le32(file, 0) as usize;
            if tree == 0 || tree > t {
                return Err(form("a corrected file names no tree of the table", trees_end + e * CORRECTED));
            }
            if !text_ok(file, 4) || !text_ok(file, 12) {
                return Err(form("a corrected file's string is outside the heap", trees_end + e * CORRECTED));
            }
        }
        Ok(list)
    }

    /// How many trees past the checkout the rows' paths name: tree `k` is `1..=trees()`.
    pub fn trees(&self) -> usize {
        self.trees.len() / TREE
    }

    /// Where tree `k` (1 and up) is kept, and its files' identity; none for the checkout (0) or
    /// past the table.
    pub fn tree(&self, k: usize) -> Option<TreeRow> {
        let at = k.checked_sub(1)?.checked_mul(TREE)?;
        let row = self.trees.get(at..at + TREE)?;
        let mut name = [0; 32];
        name.copy_from_slice(&row[4..36]);
        let mut files = [0; 32];
        files.copy_from_slice(&row[36..68]);
        let origin = match row[0] {
            SHELF => PackOrigin::Shelf { digest: name },
            _ => PackOrigin::Source { key: ContentId(name) },
        };
        // Checked once at [`CrateList::open`]: every row's kind byte is one `pack_kind_of` reads.
        let kind = pack_kind_of(row[1])?;
        Some(TreeRow { origin, kind, files })
    }

    /// The files read corrected (D58), in the world's order: `(tree, path in its pack, text)`,
    /// read in place of the pack's bytes (`crate::Texts::corrected`).
    pub fn corrected(&self) -> impl Iterator<Item = (u32, &'b str, &'b str)> + '_ {
        self.corrected.chunks_exact(CORRECTED).map(|file| (le32(file, 0), self.text(file, 4), self.text(file, 12)))
    }

    fn row(&self, r: usize) -> &'b [u8] {
        &self.rows[r * ROW..(r + 1) * ROW]
    }

    fn text(&self, b: &[u8], at: usize) -> &'b str {
        let (off, len) = (le32(b, at) as usize, le32(b, at + 4) as usize);
        &self.heap[off..off + len]
    }

    /// The row of the crate `key`: a search of the by-key table in the bytes, nothing allocated.
    pub fn find(&self, key: CrateItemKey) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.n);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let r = le32(self.by_key, mid * 4) as usize;
            match self.row(r)[..32].cmp(&key.0.0[..]) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => return Some(r),
            }
        }
        None
    }

    /// Its cfg holds `test` with no value ([`TEST`]).
    pub fn test(&self, r: usize) -> bool {
        self.row(r)[FLAGS_AT] & TEST != 0
    }
    /// Its records may hold nothing of a project ([`PUBLIC`]).
    pub fn public(&self, r: usize) -> bool {
        self.row(r)[FLAGS_AT] & PUBLIC != 0
    }
    /// Its package's name as the world holds it (`Crate::package`; `""` for none).
    pub fn package(&self, r: usize) -> &'b str {
        self.text(self.row(r), ENV_AT + 8)
    }

    fn module_at(&self, e: usize) -> ModuleRow<'b> {
        let module = &self.modules[e * MODULE..(e + 1) * MODULE];
        ModuleRow { row: le32(module, 0) as usize, ran_as: module[4], subject: subject_of(&module[8..40]), root: self.text(module, 40) }
    }
    /// Every build-script module, by (subject, row).
    pub fn modules(&self) -> impl Iterator<Item = ModuleRow<'b>> + '_ {
        (0..self.modules.len() / MODULE).map(|e| self.module_at(e))
    }
    /// The module row `r`'s package's build script is looked up by.
    pub fn module_of(&self, r: usize) -> Option<ModuleRow<'b>> {
        self.modules().find(|module| module.row == r)
    }
    /// The first module (by row) whose subject is `subject`.
    pub fn module(&self, subject: [u8; 32]) -> Option<ModuleRow<'b>> {
        let m = self.modules.len() / MODULE;
        let at = partition(m, |e| self.modules[e * MODULE + 8..e * MODULE + 40] < subject[..]);
        (at < m).then(|| self.module_at(at)).filter(|module| module.subject == subject)
    }

    fn run_at(&self, e: usize) -> RunRow<'b> {
        let run = &self.runs[e * RUN..(e + 1) * RUN];
        let (start, count) = (le32(run, 56) as usize, le32(run, 60) as usize);
        RunRow {
            subject: subject_of(run),
            unit: le32(run, 32),
            side: if run[36] == 1 { Side::Macro } else { Side::Target },
            name: self.text(run, 40),
            version: self.text(run, 48),
            producers: &self.producers[start * SUBJECT..(start + count) * SUBJECT],
        }
    }
    /// Every build-script run the crates read, by subject.
    pub fn runs(&self) -> impl Iterator<Item = RunRow<'b>> + '_ {
        (0..self.runs.len() / RUN).map(|e| self.run_at(e))
    }
    /// The run `subject`.
    pub fn run(&self, subject: [u8; 32]) -> Option<RunRow<'b>> {
        let u = self.runs.len() / RUN;
        let at = partition(u, |e| self.runs[e * RUN..e * RUN + 32] < subject[..]);
        (at < u).then(|| self.run_at(at)).filter(|run| run.subject == subject)
    }
    /// Every proc-macro crate's module subject, in order.
    pub fn macros(&self) -> impl Iterator<Item = [u8; 32]> + 'b {
        self.macros.chunks_exact(SUBJECT).map(subject_of)
    }

    pub fn key(&self, r: usize) -> CrateItemKey {
        let mut k = [0; 32];
        k.copy_from_slice(&self.row(r)[..32]);
        CrateItemKey(ContentId(k))
    }
    pub fn root(&self, r: usize) -> &'b str {
        self.text(self.row(r), 40)
    }
    pub fn shown_from(&self, r: usize) -> &'b str {
        self.text(self.row(r), 48)
    }
    pub fn version(&self, r: usize) -> &'b str {
        self.text(self.row(r), ENV_AT + 16)
    }
    /// Its root is in the checkout (tree 0): a workspace member's crate.
    pub fn member(&self, r: usize) -> bool {
        self.row(r)[FLAGS_AT] & MEMBER != 0
    }
    /// Each dependency: the name it calls it, and its row.
    pub fn deps_named(&self, r: usize) -> impl Iterator<Item = (&'b str, usize)> + '_ {
        let row = self.row(r);
        let (start, count) = (le32(row, 56) as usize, le32(row, 60) as usize);
        (start..start + count).map(move |e| {
            let dep = &self.deps[e * DEP..(e + 1) * DEP];
            (self.text(dep, 0), le32(dep, 8) as usize)
        })
    }
    /// The name of its `Script` part (kind byte, subject), when it has one.
    pub fn script(&self, r: usize) -> Option<[u8; 33]> {
        let row = self.row(r);
        if row[66] == 0 {
            return None;
        }
        let mut name = [0; 33];
        name[0] = row[66];
        name[1..].copy_from_slice(&row[68..100]);
        Some(name)
    }
    /// The facts of its environment as the world made it (blaze-scope `CrateEnv::facts`), its
    /// files read under `shown_from`: `CrateEnv::of` of them answers every `env!` as the
    /// world's environment of the crate does. No script record is joined (`script: None`): a
    /// reader with the crate's `Script` input passes its outputs in the facts it hands `of`.
    pub fn env_facts(&self, r: usize) -> EnvFacts<'b> {
        let row = self.row(r);
        let flags = row[ENV_AT + 40];
        EnvFacts {
            crate_name: self.text(row, ENV_AT),
            package: (flags & ENV_PACKAGE != 0).then(|| (self.text(row, ENV_AT + 8), self.text(row, ENV_AT + 16))),
            bin: (flags & ENV_BIN != 0).then(|| self.text(row, ENV_AT + 24)),
            // A member's dir was checked a plain path at `open`.
            seen: if flags & ENV_MEMBER != 0 { SeenRoot::Member { dir: self.text(row, ENV_AT + 32) } } else { SeenRoot::Package },
            read_folder: self.shown_from(r),
            build_script: flags & ENV_BUILD_SCRIPT != 0,
            script: None,
            test_or_bench: flags & ENV_TEST_OR_BENCH != 0,
            // Checked a `BinNames` at `open`: the `NONE` arm is never taken.
            bin_exes: BinNames::new(self.text(row, BIN_EXES_AT)).unwrap_or(BinNames::NONE),
            meta: (flags & ENV_META != 0).then(|| PackageMetaVars::from_values(core::array::from_fn(|i| self.text(row, META_AT + 8 * i)))),
        }
    }
}

/// Whether two environments' facts are one (the script record aside): `read_folder` compared
/// without its trailing `/`, as `CrateEnv::of` keeps it.
fn same_facts(a: &EnvFacts<'_>, b: &EnvFacts<'_>) -> bool {
    a.crate_name == b.crate_name
        && a.package == b.package
        && a.bin == b.bin
        && a.seen == b.seen
        && a.read_folder.trim_end_matches('/') == b.read_folder.trim_end_matches('/')
        && a.build_script == b.build_script
        && a.test_or_bench == b.test_or_bench
        && a.bin_exes == b.bin_exes
        && a.meta == b.meta
}

impl<'b> CrateGraph<'b> for CrateList<'b> {
    fn len(&self) -> usize {
        self.n
    }
    fn name(&self, at: usize) -> &'b str {
        self.text(self.row(at), 32)
    }
    fn side(&self, at: usize) -> Side {
        if self.row(at)[64] == 1 { Side::Macro } else { Side::Target }
    }
    fn proc_macro(&self, at: usize) -> bool {
        self.row(at)[FLAGS_AT] & PROC_MACRO != 0
    }
    fn toolchain(&self, at: usize) -> bool {
        self.row(at)[FLAGS_AT] & TOOLCHAIN != 0
    }
    fn refused(&self, at: usize) -> bool {
        self.row(at)[FLAGS_AT] & REFUSED != 0
    }
    fn deps(&self, at: usize) -> impl Iterator<Item = usize> + '_ {
        self.deps_named(at).map(|(_, row)| row)
    }
    fn sysroot(&self) -> [Option<usize>; 4] {
        self.sysroot.map(|r| (r != NONE).then_some(r as usize))
    }
}

/// How many fields of `world`'s crates differ from `list`'s rows (each crate found by its key;
/// a crate with no row counts once): 0 when the list is the world's. The fields: name, root,
/// shown_from, side, proc_macro, toolchain, member, refused, the environment's facts, sysroot, each dependency's name and
/// key, each tree's origin and files, and each corrected file.
pub fn differences(world: &World<'_>, list: &CrateList<'_>) -> usize {
    differences_inner(world, list, true)
}

/// As [`differences`], skipping the pack/origin table comparison for worlds constructed lazily
/// via [`crate::world_from_list`] whose packs are opened on demand rather than eagerly mapped.
pub fn differences_without_packs(world: &World<'_>, list: &CrateList<'_>) -> usize {
    differences_inner(world, list, false)
}

fn differences_inner(world: &World<'_>, list: &CrateList<'_>, check_packs: bool) -> usize {
    let mut differ = 0;
    if list.len() != world.crates.len() {
        differ += 1;
    }
    let key_row = |at: usize| list.find(world.crates[at].input.key);
    for (at, c) in world.crates.iter().enumerate() {
        let Some(r) = key_row(at) else {
            differ += 1;
            continue;
        };
        // Row `r` is the world's crate `r` (version 6).
        differ += usize::from(r != at)
            + usize::from(list.test(r) != cfg_test(world, at))
            + usize::from(list.package(r) != c.package)
            + usize::from(list.name(r) != c.input.name)
            + usize::from(list.root(r) != c.input.root)
            + usize::from(list.shown_from(r) != c.input.shown_from)
            + usize::from(list.side(r) != c.side)
            + usize::from(list.proc_macro(r) != c.proc_macro)
            + usize::from(list.toolchain(r) != world.toolchain(at))
            + usize::from(list.member(r) != c.member)
            + usize::from(list.refused(r) != world.refused(at))
            + usize::from(!world.texts.crate_env(at).is_some_and(|env| same_facts(&env.facts(), &list.env_facts(r))));
        let mut deps = list.deps_named(r);
        for (name, dep) in c.input.deps.iter() {
            match deps.next() {
                Some((n, row)) if n == *name && list.key(row) == world.crates[*dep].input.key => {}
                _ => differ += 1,
            }
        }
        differ += deps.count();
    }
    let rows = list.sysroot();
    for (i, id) in CrateGraph::sysroot(world).iter().enumerate() {
        let want = id.map(|at| world.crates[at].input.key);
        if want != rows[i].map(|r| list.key(r)) {
            differ += 1;
        }
    }
    if check_packs {
        let (packs, origins) = (world.texts.packs(), world.texts.origins());
        differ += packs.len().abs_diff(list.trees());
        for (k, (pack, origin)) in packs.iter().zip(origins.iter()).enumerate() {
            if list.tree(k + 1) != Some(TreeRow { origin: *origin, kind: pack.kind(), files: pack.tree().0 }) {
                differ += 1;
            }
        }
    }
    let mut corrected = list.corrected();
    for (tree, path, text) in world.texts.corrected().iter() {
        if corrected.next() != Some((*tree, *path, *text)) {
            differ += 1;
        }
    }
    differ + corrected.count()
}
