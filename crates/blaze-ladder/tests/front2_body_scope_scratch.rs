//! D189 step 4, the block-scopes differential's frame: one held body's work (the cell core, the
//! read list's finished-scope bytes) runs in a driver's scratch reset after the body
//! (blaze-scope body_scope.rs `body_scope_scratch`, task/fill.rs `Scratch::run`), so a pass
//! over every held body of a crate keeps the records and never bodies × modules. Tempo's
//! `core` (21 Nov 05:33): "DID NOT RUN: the `Bodies` solve panicked in its own frame
//! (4427022336 bytes committed of 4294967296)". The scratch test loads each body over its
//! crate's whole tree ([`ReachPlant::WholeTree`]) so a body's work grows with the modules; the
//! by-need load (`Cells::load_body`, the second test) is what makes it small.
//!
//! The world: crate `up` of [`MODULES`] modules and [`BODIES`] held bodies, each body
//! `{ struct Local; impl Tr for Local { .. } }` (the shape the differential solves). Every held
//! body's record is made twice, each pass in a fresh home frame: under the hook of no driver
//! (`Scratch::kept`: the CONTROL, rule 23, the cause present: every body's work kept in the
//! home frame) and under a scratch reset after each use.
//!
//! - The records are equal byte for byte under both (the scratch moves memory, never an
//!   answer).
//! - The home frame grows by the records alone under the scratch: its growth per body is
//!   under half the control's (printed, with the scratch's own peak: one body's work).
//! - The negative twin: a crate of [`FEW_MODULES`] modules, where a body's work is small, so
//!   the two homes are close; the gap is the modules' (it widens with them).
//!
//! Needs the installed toolchain under `$HOME/.blaze/v2/toolchain`.

use core::cell::RefCell;
use std::fmt::Write as _;

use blaze_ladder::crates::{DirShelf, Inputs, Toolchain, UserCrate};
use blaze_mem::{Frame, Tag};
use blaze_own::{AVec, Alloc, Erased};
use blaze_scope::body_scope::body_scope_scratch;
use blaze_scope::graph::{CrateId, Sysroot};
use blaze_scope::prepares::Prepares;
use blaze_scope::resolve::{BodyDeps, CrateInput, CrateNames, Names, ReachPlant, ReadTree, SourceTexts, resolve_crates};
use blaze_scope::surface::BlockScopes;
use blaze_scope::task::{Par, Scratch, Serial};

/// The modules of the large crate and of its twin.
const MODULES: usize = 256;
const FEW_MODULES: usize = 4;
/// The held bodies of each.
const BODIES: usize = 24;

/// The serial solve with D189's seam set to `Bodies` and a per-call expansion scratch.
struct Bodies {
    scratch: RefCell<Frame>,
}

impl Bodies {
    fn new() -> Self {
        Self { scratch: RefCell::new(Frame::new(Tag::Expand).expect("a call scratch frame")) }
    }
}

impl Par for Bodies {
    fn map<T, F: Fn(usize) -> T>(&self, n: usize, f: F, out: &mut AVec<'_, T>) {
        Serial.map(n, f, out);
    }

    fn block_scopes(&self) -> BlockScopes {
        BlockScopes::Bodies
    }

    fn call_scratch(&self, work: &mut dyn FnMut(Alloc<'_>)) -> Scratch {
        let mut frame = self.scratch.borrow_mut();
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        Scratch::Reset
    }
}

/// `up`'s text: `modules` modules, each with a glob of the root (so a body's lookups read
/// finished scopes), and `BODIES` held bodies.
fn text(modules: usize) -> String {
    let mut t = String::from("pub trait Tr { fn get(&self) -> u8; }\npub use m0::*;\n");
    for m in 0..modules {
        let _ = writeln!(t, "pub mod m{m} {{ pub use super::Tr; pub struct S{m}; pub fn g{m}() {{}} }}");
    }
    for b in 0..BODIES {
        let _ = writeln!(t, "pub fn f{b}() -> u8 {{ struct Local; impl Tr for Local {{ fn get(&self) -> u8 {{ {b} }} }} Local.get() }}");
    }
    t.push_str("pub fn main() {}\n");
    t
}

/// A home frame for one pass, kept for the test (its handle outlives the pass).
fn home() -> (&'static Frame, blaze_own::Bump<'static>) {
    let frame: &'static Frame = Box::leak(Box::new(Frame::new(Tag::Nameres).expect("a home frame")));
    let erased: &'static Erased<&'static Frame> = Box::leak(Box::new(Erased::new(frame)));
    (frame, erased.bump())
}

/// One pass over every held body of `up` (`modules` modules): each record's bytes, the home
/// frame's growth over the pass, and the scratch's peak (0 under the control).
struct Pass {
    records: Vec<Vec<u8>>,
    grown: usize,
    scratch_peak: usize,
}

/// `up` of `modules` modules resolved under `Bodies`, everything a body's solve takes, leaked
/// for the test's life.
struct Up {
    crates: &'static [CrateInput<'static>],
    inputs: &'static Inputs<'static>,
    c: &'static CrateNames<'static>,
    deps: &'static BodyDeps<'static, 'static>,
    tree: &'static ReadTree<'static>,
    prepares: &'static Prepares<'static>,
}

fn up(modules: usize) -> Up {
    up_with(modules, 0)
}

/// `up` of `modules` inline modules and `file_mods` more modules each in its own file
/// (`mod f{i};`, `/up/src/f{i}.rs`).
fn up_with(modules: usize, file_mods: usize) -> Up {
    let alloc = blaze_own::testing::heap().alloc();
    let home_dir = std::env::var("HOME").expect("HOME is set");
    let shelf = format!("{home_dir}/.blaze/v2/toolchain");
    let tc: &'static Toolchain = Box::leak(Box::new(Toolchain::open(&mut DirShelf::new(shelf.clone())).unwrap_or_else(|e| panic!("the toolchain at {shelf}: {e}"))));
    let edition = edition::PackageEdition::of_manifest("2021").expect("a known edition");
    let mut root = text(modules);
    let mut list = Vec::new();
    for i in 0..file_mods {
        let _ = writeln!(root, "pub mod f{i};");
        list.push((format!("/up/src/f{i}.rs"), format!("pub fn h{i}() {{}}\n")));
    }
    list.insert(0, ("/up/src/main.rs".to_string(), root));
    let files: &'static [(String, String)] = Box::leak(list.into_boxed_slice());
    let up: &'static UserCrate<'static> = Box::leak(Box::new(UserCrate { name: "up", dir: "/up/", root: "/up/src/main.rs", files, edition, panic: blaze_build::Panic::Unwind }));
    let inputs: &'static Inputs<'static> = Box::leak(Box::new(Inputs::new(up, tc).unwrap_or_else(|e| panic!("the crates: {e}"))));
    let crates: &'static [CrateInput<'static>] = Vec::leak(inputs.crates());
    let at = |name: &str| crates.iter().position(|c| c.name == name).map(|i| CrateId(u32::try_from(i).expect("few crates")));
    let sysroot = Sysroot { core: at("core"), alloc: at("alloc"), std: at("std"), bstd: at("bstd") };
    let (_, bump) = home();
    let bodies = Bodies::new();
    let names: &'static Names<'static> = Box::leak(Box::new(resolve_crates(crates, sysroot, &inputs.texts, &bodies, bump).unwrap_or_else(|e| panic!("resolve_crates under Bodies: {e}"))));
    let c = names.crates()[0];
    assert_eq!(c.bodies.len(), BODIES, "every `f` is held under Bodies (its block writes an item)");
    let deps: &'static BodyDeps<'static, 'static> = Box::leak(Box::new(BodyDeps::of_names(names, 0, alloc)));
    let tree: &'static ReadTree<'static> = Box::leak(Box::new(ReadTree::of(c, bump).unwrap_or_else(|e| panic!("up's tree record: {e:?}"))));
    let prepares = Prepares::new_in(bump);
    Up { crates, inputs, c, deps, tree, prepares }
}

fn passes(modules: usize) -> (Pass, Pass) {
    let alloc = blaze_own::testing::heap().alloc();
    let Up { crates, inputs, c, deps, tree, prepares } = up(modules);

    // The whole-tree load: a body's work the size of its crate's tree, so the scratch's
    // reset is seen against the modules (the by-need load makes a body's work small whatever
    // the tree, `a_body_solve_makes_its_chain_and_reads_not_its_crate_s_tree`).
    let sources = WholeTree(&inputs.texts);
    let pass = |hook: blaze_scope::task::ScratchHook<'_>, scratch_peak: &dyn Fn() -> usize| -> Pass {
        let (frame, home) = home();
        let before = frame.used();
        let mut records = Vec::new();
        for held in c.bodies.iter() {
            let scope = body_scope_scratch(tree, deps, &crates[0], 0, *held, &sources, prepares, hook, home).unwrap_or_else(|e| panic!("body_scope: {e}"));
            records.push(scope.write(alloc).unwrap_or_else(|e| panic!("write: {e:?}")).to_vec());
        }
        Pass { records, grown: frame.used() - before, scratch_peak: scratch_peak() }
    };
    let kept = pass(&Scratch::kept, &|| 0);

    let scratch = RefCell::new(Frame::with_reserve(1 << 30, Tag::Expand).expect("a scratch frame"));
    let hook = |work: &mut dyn FnMut(Alloc<'_>)| {
        let mut frame = scratch.try_borrow_mut().unwrap_or_else(|_| panic!("the scratch was asked inside its own use"));
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        Scratch::Reset
    };
    let reset = pass(&hook, &|| scratch.borrow().peak());
    (kept, reset)
}

/// `up`'s texts with each body solve making its crate's whole module tree at its load
/// ([`ReachPlant::WholeTree`]): the CONTROL of the by-need load (rule 23, the cause put back).
struct WholeTree<'t>(&'t blaze_ladder::crates::Texts<'t>);

impl<'t> SourceTexts<'t> for WholeTree<'t> {
    fn read(&self, krate: usize, path: &str) -> Option<&'t str> {
        self.0.read(krate, path)
    }

    fn reach_plant(&self) -> ReachPlant {
        ReachPlant::WholeTree
    }
}

/// Every held body of `up` solved once: each body's "modules made for body solves"
/// (`BodySolved::made_outside`) and its record's bytes; `whole` puts the whole-tree load back.
fn made(up: &Up, whole: bool) -> (Vec<usize>, Vec<Vec<u8>>) {
    let alloc = blaze_own::testing::heap().alloc();
    let (_, bump) = home();
    let whole_tree = WholeTree(&up.inputs.texts);
    let mut made = Vec::new();
    let mut records = Vec::new();
    for held in up.c.bodies.iter() {
        let scope = if whole {
            body_scope_scratch(up.tree, up.deps, &up.crates[0], 0, *held, &whole_tree, up.prepares, &Scratch::kept, bump)
        } else {
            body_scope_scratch(up.tree, up.deps, &up.crates[0], 0, *held, &up.inputs.texts, up.prepares, &Scratch::kept, bump)
        }
        .unwrap_or_else(|e| panic!("body_scope: {e}"));
        made.push(scope.solved.made_outside);
        records.push(scope.write(alloc).unwrap_or_else(|e| panic!("write: {e:?}")).to_vec());
    }
    (made, records)
}

/// The modules made for body solves: a body's solve makes its module's chain (`up`'s root)
/// and what its lookups reach (the root's glob source `m0`), never its crate's tree
/// (blaze-scope cells/mod.rs `Cells::load_body`). Each body makes as many at [`MODULES`]
/// modules as at [`FEW_MODULES`], and its record is byte for byte the whole-tree load's.
/// CONTROL (rule 23): the whole-tree load put back makes every module of `up` for every
/// body, so its count per body grows with the modules. NEGATIVE TWIN: at [`FEW_MODULES`] the
/// two loads are near (the tree is small).
#[test]
fn a_body_solve_makes_its_chain_and_reads_not_its_crate_s_tree() {
    let many = up(MODULES);
    let few = up(FEW_MODULES);
    let (by_need, records) = made(&many, false);
    let (whole, whole_records) = made(&many, true);
    let (few_by_need, _) = made(&few, false);
    let (few_whole, _) = made(&few, true);
    let sum = |v: &[usize]| v.iter().sum::<usize>();
    println!(
        "{MODULES} modules: modules made for body solves: {}; bodies: {BODIES} (whole tree: {}); {FEW_MODULES} modules: {} (whole tree: {})",
        sum(&by_need),
        sum(&whole),
        sum(&few_by_need),
        sum(&few_whole)
    );
    assert_eq!(records, whole_records, "the by-need load moves work, never an answer");
    assert_eq!(by_need, few_by_need, "a body's modules made do not grow with its crate's modules");
    assert!(by_need.iter().all(|&n| (1..=4).contains(&n)), "each body makes its chain (the root) and the glob source it reads: {by_need:?}");
    // The control: the whole tree for every body, so the count follows the crate's modules.
    assert!(whole.iter().all(|&n| n > MODULES), "the whole-tree load makes every module of `up` for each body: {whole:?}");
    assert!(sum(&whole) - sum(&few_whole) >= BODIES * (MODULES - FEW_MODULES), "the control's count grows with the modules");
    // The twin: few modules, the two loads near.
    assert!(few_whole.iter().zip(few_by_need.iter()).all(|(&w, &b)| w <= b + FEW_MODULES + 1), "{few_whole:?} vs {few_by_need:?}");
}

/// The file modules of the crate of the files test.
const FILES: usize = 128;

/// How much a pass over every held body of `up` grows its home frame, each body's cell work in
/// a scratch reset after it (so the home holds the solve's own tables, files among them, and
/// the records), and the records' bytes; `whole` puts the whole-tree load back.
fn home_growth(up: &Up, whole: bool) -> (usize, Vec<Vec<u8>>) {
    let alloc = blaze_own::testing::heap().alloc();
    let whole_tree = WholeTree(&up.inputs.texts);
    let scratch = RefCell::new(Frame::with_reserve(1 << 30, Tag::Expand).expect("a scratch frame"));
    let hook = |work: &mut dyn FnMut(Alloc<'_>)| {
        let mut frame = scratch.try_borrow_mut().unwrap_or_else(|_| panic!("the scratch was asked inside its own use"));
        frame.scope(|f| {
            let erased = Erased::new(f);
            work(erased.handle());
        });
        Scratch::Reset
    };
    let (frame, home) = home();
    let before = frame.used();
    let mut records = Vec::new();
    for held in up.c.bodies.iter() {
        let scope = if whole {
            body_scope_scratch(up.tree, up.deps, &up.crates[0], 0, *held, &whole_tree, up.prepares, &hook, home)
        } else {
            body_scope_scratch(up.tree, up.deps, &up.crates[0], 0, *held, &up.inputs.texts, up.prepares, &hook, home)
        }
        .unwrap_or_else(|e| panic!("body_scope: {e}"));
        records.push(scope.write(alloc).unwrap_or_else(|e| panic!("write: {e:?}")).to_vec());
    }
    (frame.used() - before, records)
}

/// D189 successor 2, `FileSet` by need: a body's solve finds its crate's files in place
/// (blaze-scope resolve.rs `FileSet::over`, the index made once per crate in `ReadTree::of`)
/// and copies none of them, so what it keeps per body does not grow with its crate's files.
/// The body's own work runs in a scratch reset after it, so the home holds what a solve keeps
/// (its file set among it): by need, the home's growth over all bodies stays under half a file
/// reference per file per body more than a crate of no file modules (the NEGATIVE TWIN).
/// CONTROL (rule 23): the whole-tree plant copies and indexes every file for each body, as
/// before; its growth rises by at least a file reference per file per body.
#[test]
fn a_body_solve_copies_no_file_of_its_crate() {
    let many = up_with(FEW_MODULES, FILES);
    let none = up_with(FEW_MODULES, 0);
    let (need_many, records) = home_growth(&many, false);
    let (whole_many, whole_records) = home_growth(&many, true);
    let (need_none, _) = home_growth(&none, false);
    let (whole_none, _) = home_growth(&none, true);
    println!("{FILES} file modules, {BODIES} bodies: home grew {need_many} by need, {whole_many} whole; no file modules: {need_none} by need, {whole_none} whole");
    assert_eq!(records, whole_records, "the files in place move memory, never an answer");
    let per_file_per_body = BODIES * FILES * core::mem::size_of::<usize>();
    assert!(
        need_many < need_none + per_file_per_body / 2,
        "by need the home does not grow with the crate's files: {need_many} at {FILES} files, {need_none} at none"
    );
    assert!(
        whole_many >= whole_none + per_file_per_body,
        "the control copies a reference per file per body: {whole_many} at {FILES} files, {whole_none} at none"
    );
}

#[test]
fn a_body_s_work_is_reset_after_it_and_the_records_are_equal() {
    let (kept, reset) = passes(MODULES);
    assert_eq!(kept.records, reset.records, "the scratch moves memory, never an answer");
    println!(
        "{MODULES} modules, {BODIES} bodies: home grew {} bytes kept (control), {} bytes with the scratch (its peak {}); per body {} vs {}",
        kept.grown,
        reset.grown,
        reset.scratch_peak,
        kept.grown / BODIES,
        reset.grown / BODIES
    );
    assert!(reset.scratch_peak > 0, "the work ran in the scratch");
    assert!(reset.grown * 2 < kept.grown, "a body's work is not kept in the home frame: {} vs {}", reset.grown, kept.grown);

    // The twin: few modules, little work per body: the homes are close, so the gap above is
    // the module tree's.
    let (few_kept, few_reset) = passes(FEW_MODULES);
    assert_eq!(few_kept.records, few_reset.records);
    println!("{FEW_MODULES} modules: home grew {} kept, {} with the scratch", few_kept.grown, few_reset.grown);
    assert!(
        kept.grown - reset.grown > 4 * (few_kept.grown - few_reset.grown),
        "the work the scratch takes grows with the modules: {} at {MODULES}, {} at {FEW_MODULES}",
        kept.grown - reset.grown,
        few_kept.grown - few_reset.grown
    );
}
