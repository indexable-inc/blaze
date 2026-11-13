//! The daemon's front-end carry driver (docs/v2/plan/design/end-state.md, "The front end as
//! kernel steps", the order's row 3): one [`blaze_kernel::Session`] kept across the builds of
//! a checkout, holding blaze-front-items' steps (`parse`, `items`, `lines`, `modules`) over a
//! world of the checkout's pinned tree ([`Tree`]). Each build pins the view's root; the
//! difference from the last pin is the VFS's Merkle diff ([`blaze_vfs::Vfs::diff`], which descends only
//! where directory ids differ), so the change set is exact and costs `O(changed paths × depth)`,
//! never a reread of the tree. [`Carry::build`] turns it into the carry's [`Changed`], calls
//! [`Kernel::carry`], and asks the steps' roots again ([`Carry::build`]'s doc says which).
//!
//! A sans-IO core (rule 14): it reads the VFS's tree through a [`FileReader`] and the store it
//! is handed, holds no clock, opens no file and holds no view. The job loop hands it one
//! [`Pass`] at a build's end ([`Carry::run`]), run as a work task of its own off the loop
//! (`kinds::FRONT_CARRY`); the loop computes the pass's change set (the VFS's diff, which needs
//! both roots, held open by the loop's views) and puts the task's store writes ([`Overlay`]).
//!
//! What the world answers, read by read (rule 21: exactly what the steps read):
//! - `FILE` (`steps::reads::FILE`): a file's content id from the tree, gone where none; its
//!   text is the tree's own bytes, checked to be UTF-8 when a step reads it.
//! - `EXISTS`: whether the tree has a file at that path.
//! - `CFG`: whether a predicate holds for a crate, evaluated against the crate's
//!   [`CfgOptions`]. A crate's stable name holds the digest of its cfg ([`CrateRoot::new`]), so
//!   a name's cfg never changes: a feature flip is another crate (its `modules` asked anew; its
//!   files' `parse` and `items`, keyed by file and edition, are shared), and no `CFG` read
//!   ever differs between pins. The edition is in every file key (`Reading`), so an edition
//!   change is a new key too.
//!
//! The pin is the view's root id: every read above is a function of it and of the crates
//! known, whose cfgs never change under their names.
//!
//! A crate's cfg is the one its build ended with, every build script's `cargo:rustc-cfg`
//! applied, and its files are read with the syntax extensions its root turns on
//! ([`CrateRoot::extensions`]); both are in the crate's name, so either moving is another
//! crate. A symlink in the tree is refused when a step reads it, naming it. Crates outside the
//! checkout (registry dependencies, the standard library) are not in this world: they are
//! other roots and never change under a checkout's build. Each pass saves the session
//! ([`Saved`]); the daemon's journal marks the last durable one a checkout
//! (`serve::local::CarryMark`), and a restarted daemon opens it ([`Pass::Open`]).

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use blaze_cas::{Key as StoreKey, Kind as StoreKind, Mismatch, content_of};
use blaze_front_items::{
    Reading,
    modules::{MODULES, Module, Status},
    steps::{CrateKey, FileKey, Items, Lines, Modules, Parse, Source, reads},
    table::Table,
};
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{
    Carried, Changed, CodeId, Get, Kernel, KernelError, Pin, Put, Read, Seen, Session, World,
    WorldError,
};
use blaze_store_check::Setting;
use blaze_vfs::{Bytes, Change, ChangeKind, Mode, Reader as FileReader};
use cfg::{CfgExpr, CfgOptions};
use syntax::{Edition, Extensions, view::AstNode as _};

const PIN_DOMAIN: &str = "blaze v2 front carry pin 1";
const EXISTS_DOMAIN: &str = "blaze v2 front carry exists 1";
const CFG_DOMAIN: &str = "blaze v2 front carry cfg 1";
const CFG_NAME_DOMAIN: &str = "blaze v2 front carry crate cfg 1";
const VALUE_DOMAIN: &str = "blaze v2 front carry modules value 1";

/// The steps' code. TEMPORARY in its granularity, as the code generator's toolchain id is
/// (`blaze_codegen::kernel::code`): a version by hand, bumped with any change to
/// blaze-front-items' steps, until `fn_info(..).code_id` gives each step its own (rule 52).
const CODE: [(&str, u8); 4] = [("parse", 1), ("items", 1), ("lines", 1), ("modules", 1)];

fn code(step: usize) -> CodeId {
    let (name, version) = CODE[step];
    let mut hasher = Hasher::new("blaze v2 front carry step code 1");
    hasher.part(name.as_bytes()).part(&[version]);
    CodeId(hasher.finish())
}

/// The front end's kernel: the four steps of blaze-front-items over [`Tree`].
pub(crate) fn kernel() -> Result<Kernel<Tree>, KernelError> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Parse<Tree>>(code(0))?;
    kernel.register_ask::<Items<Tree>>(code(1))?;
    kernel.register_ask::<Lines<Tree>>(code(2))?;
    kernel.register_ask::<Modules<Tree>>(code(3))?;
    Ok(kernel)
}

/// A crate of the checkout as the carry asks for it: its stable name, its root file's path
/// in the pinned tree, its edition, its cfg and its syntax extensions.
#[derive(Clone, Debug)]
pub(crate) struct CrateRoot {
    /// What the crate is (its package, target and kinds) and the digest of its cfg and its
    /// extensions: equal names are equal cfgs, so a read of a crate's cfg never differs
    /// between pins, and equal readings of its files.
    pub(crate) name: String,
    pub(crate) root: String,
    pub(crate) edition: Edition,
    /// The cfg the build ended with: every build script's `cargo:rustc-cfg` applied.
    pub(crate) cfg: CfgOptions,
    /// The syntax extensions the crate's root turns on (`base_db::Crate::extensions`): every
    /// file of the crate is read with them.
    pub(crate) extensions: Extensions,
}

impl CrateRoot {
    /// The crate `what` (`package target kinds`), rooted at `root` (a path in the pinned
    /// tree), named by `what` and the digest of its cfg and extensions.
    pub(crate) fn new(
        what: &str,
        root: String,
        edition: Edition,
        cfg: CfgOptions,
        extensions: Extensions,
    ) -> CrateRoot {
        // alloc: once a crate a plan, its atoms in order (rule 25)
        let mut atoms: Vec<String> = cfg.atoms().map(ToString::to_string).collect();
        atoms.sort_unstable();
        let mut hasher = Hasher::new(CFG_NAME_DOMAIN);
        hasher.part(&[extensions.bits()]);
        for atom in &atoms {
            hasher.part(atom.as_bytes());
        }
        let digest = format!("{:x}", hasher.finish()); // alloc: once a crate a plan
        // alloc: the crate's stable name, once a crate a plan
        let name = format!("{what} cfg:{}", &digest[..32.min(digest.len())]);
        CrateRoot { name, root, edition, cfg, extensions }
    }

    fn key(&self) -> CrateKey {
        let reading = Reading { edition: self.edition, extensions: self.extensions };
        // alloc: the step's input, once a crate a build
        let root = FileKey { name: self.root.clone(), reading };
        CrateKey { krate: self.name.clone(), root } // clone: the step's input
    }
}

/// One file of the pinned tree: its content id and bytes (the tree's own, nothing copied),
/// or a symlink, which this world does not follow.
#[derive(Clone)]
enum Held {
    File(ContentId, Bytes),
    Link(ContentId),
}

/// The world: the checkout's pinned tree, every file of it held (a count of the tree's own
/// bytes each), and the cfg of every crate named so far.
pub(crate) struct Tree {
    root: ContentId,
    files: BTreeMap<Box<str>, Held>,
    /// Append only: a name's cfg never changes ([`CrateRoot::name`]), so a read of an old
    /// crate's cfg answers what it always did.
    crates: BTreeMap<Box<str>, CfgOptions>,
}

impl Tree {
    /// Every file under `root`, held: once, at a session's first build.
    fn load(reader: &FileReader, root: ContentId) -> Result<Tree, FrontError> {
        let pinned = reader.pin();
        let mut files = BTreeMap::new();
        // alloc: the directories still to list, once a seed
        let mut pending = vec![String::new()];
        while let Some(dir) = pending.pop() {
            let Some(names) = pinned.list(root, &dir) else {
                // alloc: an error
                return Err(FrontError::Tree(format!("the pinned root {root:x} has no directory `{dir}`")));
            };
            for (name, is_dir) in names {
                let path = if dir.is_empty() { name.to_owned() } else { format!("{dir}/{name}") }; // alloc: one path a file
                if is_dir {
                    pending.push(path);
                    continue;
                }
                let held = held(&pinned, root, &path)?;
                files.insert(path.into_boxed_str(), held);
            }
        }
        Ok(Tree { root, files, crates: BTreeMap::new() })
    }

    /// The tree moved to `root`: each path of `changes` read again from it.
    fn moved(&mut self, reader: &FileReader, root: ContentId, changes: &[Change]) -> Result<(), FrontError> {
        let pinned = reader.pin();
        for change in changes {
            match change.kind {
                ChangeKind::Removed => {
                    self.files.remove(change.path.as_str());
                }
                ChangeKind::Added | ChangeKind::Modified => {
                    let held = held(&pinned, root, &change.path)?;
                    // clone: one path a changed file
                    self.files.insert(change.path.clone().into_boxed_str(), held);
                }
            }
        }
        self.root = root;
        Ok(())
    }

    fn cfg_of(&self, read: Read<'_>) -> Result<bool, WorldError> {
        let key = core::str::from_utf8(read.key).map_err(|_| WorldError::new(read, "a cfg read whose key is not UTF-8"))?;
        let Some((krate, predicate)) = key.split_once('\0') else {
            return Err(WorldError::new(read, "a cfg read's key is a crate, a zero byte, a predicate"));
        };
        let Some(cfg) = self.crates.get(krate) else {
            // alloc: an error
            return Err(WorldError::new(read, format!("no crate `{krate}` in this checkout's builds")));
        };
        holds(cfg, predicate).map_err(|why| WorldError::new(read, why))
    }
}

fn held(pinned: &blaze_vfs::Pinned<'_>, root: ContentId, path: &str) -> Result<Held, FrontError> {
    match pinned.held(root, path) {
        Ok(Some((id, Mode::Symlink, _))) => Ok(Held::Link(id)),
        Ok(Some((id, _, bytes))) => Ok(Held::File(id, bytes)),
        // alloc: an error
        Ok(None) => Err(FrontError::Tree(format!("`{path}` is listed under the pinned root {root:x} and is not a file of it"))),
        // alloc: an error
        Err(corrupt) => Err(FrontError::Tree(format!("`{path}` of the pinned root {root:x} is corrupt in the image: {corrupt:?}"))),
    }
}

/// Whether the cfg `predicate` (its tokens joined by one space, as `modules(crate)` reads it)
/// holds under `cfg`: parsed as a `#[cfg(..)]` and checked by the front end's own cfg
/// ([`CfgExpr::parse_from_view`], [`CfgOptions::check`]).
fn holds(cfg: &CfgOptions, predicate: &str) -> Result<bool, String> {
    let text = format!("#[cfg({predicate})] fn f() {{}}"); // alloc: one predicate's text, on a modules run
    let parse = syntax::SourceFile::parse(&text, Edition::CURRENT);
    if !parse.errors().is_empty() {
        return Err(format!("`{predicate}` is not a cfg predicate")); // alloc: an error
    }
    let Some(view) = parse.root().descendants().find_map(syntax::view::CfgPredicate::cast) else {
        return Err(format!("`{predicate}` is not a cfg predicate")); // alloc: an error
    };
    cfg.check(&CfgExpr::parse_from_view(view)).ok_or_else(|| format!("`{predicate}` is not a valid cfg predicate")) // alloc: an error
}

impl World for Tree {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        #[cfg(test)]
        sim::read();
        let name = || core::str::from_utf8(read.key).map_err(|_| WorldError::new(read, "a path that is not UTF-8"));
        match read.kind {
            reads::FILE => Ok(match self.files.get(name()?) {
                Some(Held::File(id, _) | Held::Link(id)) => Seen::Is(*id),
                None => Seen::Gone,
            }),
            reads::EXISTS => Ok(match self.files.contains_key(name()?) {
                true => Seen::Is(ContentId::of(EXISTS_DOMAIN, b"there")),
                false => Seen::Gone,
            }),
            reads::CFG => Ok(Seen::Is(ContentId::of(CFG_DOMAIN, &[u8::from(self.cfg_of(read)?)]))),
            _ => Err(WorldError::new(read, "a kind of read the front end's world does not answer")),
        }
    }

    fn pin(&self) -> Pin {
        Pin(ContentId::of(PIN_DOMAIN, &self.root.0))
    }
}

impl Source for Tree {
    fn file(&self, name: &str) -> Result<Option<&str>, WorldError> {
        #[cfg(test)]
        sim::read();
        let read = Read { kind: reads::FILE, key: name.as_bytes() };
        match self.files.get(name) {
            None => Ok(None),
            Some(Held::File(_, bytes)) => core::str::from_utf8(bytes)
                .map(Some)
                .map_err(|_| WorldError::new(read, "a source file that is not UTF-8")),
            Some(Held::Link(_)) => Err(WorldError::new(read, "a symlink, which the front end's carry does not follow yet")),
        }
    }

    fn cfg(&self, krate: &str, predicate: &str) -> Result<bool, WorldError> {
        // alloc: the read's key, on a modules run
        let key = format!("{krate}\0{predicate}");
        self.cfg_of(Read { kind: reads::CFG, key: key.as_bytes() })
    }
}

/// Why a build's carry was not done, naming what.
#[derive(Debug)]
pub(crate) enum FrontError {
    /// The kernel refused (a step, a read, the store), naming the step and the read.
    Kernel(KernelError),
    /// The pinned tree: a root it does not have, a file the image holds corrupt.
    Tree(String),
    /// Two crates of one name with other roots or editions.
    Crates(String),
    /// A crate's `modules` value that is not a modules table.
    Modules(String),
}

impl From<KernelError> for FrontError {
    fn from(error: KernelError) -> FrontError {
        FrontError::Kernel(error)
    }
}

impl fmt::Display for FrontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrontError::Kernel(error) => write!(f, "the front end's carry: {error}"),
            FrontError::Tree(why) => write!(f, "the front end's carry: the pinned tree: {why}"),
            FrontError::Crates(why) => write!(f, "the front end's carry: the crates: {why}"),
            FrontError::Modules(why) => write!(f, "the front end's carry: a crate's modules: {why}"),
        }
    }
}

/// What one build's carry did: the span's fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Built {
    /// The session was made at this build: nothing was carried, every root was asked.
    pub(crate) first: bool,
    pub(crate) carried: Carried,
    /// The paths the two pins differ in.
    pub(crate) paths: usize,
    pub(crate) crates: usize,
    /// The files whose `items` and `lines` the session holds as roots after the build.
    pub(crate) files: usize,
    /// The roots of edited files asked again after the carry that ran rather than hit: the
    /// ones no step reads (`lines`), which the carry drops as `gone` instead of repairing.
    pub(crate) ran: usize,
}

/// What [`Carry::save`] gives and [`Carry::open`] takes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Saved {
    pub(crate) session: StoreKey,
    pub(crate) root: ContentId,
}

/// A file whose `items` and `lines` were asked: its name and reading, ordered by the name,
/// then the edition and extensions' bytes (rule 25: the asking order is the set's).
#[derive(Clone, PartialEq, Eq)]
struct Asked(String, Reading);

impl Asked {
    fn order(&self) -> (&str, u8, u8) {
        (&self.0, self.1.edition as u8, self.1.extensions.bits())
    }
}

impl Ord for Asked {
    fn cmp(&self, other: &Asked) -> std::cmp::Ordering {
        self.order().cmp(&other.order())
    }
}

impl PartialOrd for Asked {
    fn partial_cmp(&self, other: &Asked) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// One checkout's session and what it was asked: kept by the job loop across builds, lent to
/// the task that carries it ([`Carry::run`]).
pub(crate) struct Carry {
    kernel: Kernel<Tree>,
    session: Session,
    tree: Tree,
    /// Each crate's `modules` value's hash as last asked: an equal one asks none of its
    /// files again.
    modules: BTreeMap<String, ContentId>,
    /// The files whose `items` and `lines` were asked, by name and reading.
    asked: BTreeSet<Asked>,
}

/// What the loop hands the carry's task: the change set is the loop's, computed where both
/// roots are held open (`Vfs::diff`), so the task reads only the files that moved.
pub(crate) enum Pass {
    /// No session yet: one of `root`, every crate's roots asked.
    Seed { root: ContentId },
    /// The session the daemon's image kept, opened at its root, then carried to `root` over
    /// `changes` (the diff from `saved.root`; none when the roots are equal).
    Open { saved: Saved, root: ContentId, changes: Vec<Change> },
    /// The held session carried to `root` over `changes` (the diff from its pin's root).
    Move { root: ContentId, changes: Vec<Change> },
}

impl Pass {
    /// The root the pass ends at.
    pub(crate) fn root(&self) -> ContentId {
        match self {
            Pass::Seed { root } | Pass::Open { root, .. } | Pass::Move { root, .. } => *root,
        }
    }
}

impl Carry {
    /// One pass, on the carry's task: the session made, opened or carried (`state` is the held
    /// one, which a [`Pass::Move`] needs and the others ignore), then saved into `store`.
    /// What it gives: the session, what the pass did, and where it was saved.
    pub(crate) fn run<P: Put + ?Sized>(
        state: Option<Box<Carry>>,
        pass: Pass,
        reader: &FileReader,
        store: &mut P,
        crates: &[CrateRoot],
    ) -> Result<(Box<Carry>, Built, Saved), FrontError> {
        let (mut carry, built) = match pass {
            Pass::Seed { root } => {
                let (carry, built) = Carry::seed(reader, store, root, crates)?;
                (Box::new(carry), built) // alloc: once a session
            }
            Pass::Open { saved, root, changes } => {
                let mut carry = Box::new(Carry::open(reader, &*store, saved, crates)?); // alloc: once a session
                let built = carry.build(reader, store, root, &changes, crates)?;
                (carry, built)
            }
            Pass::Move { root, changes } => {
                let Some(mut carry) = state else {
                    return Err(FrontError::Tree(
                        "a carry to a new root was asked of a checkout that holds no session".to_owned(), // alloc: an error
                    ));
                };
                let built = carry.build(reader, store, root, &changes, crates)?;
                (carry, built)
            }
        };
        let saved = carry.save(store)?;
        Ok((carry, built, saved))
    }

    /// A session of `root`, every crate's roots asked: the first build of a checkout.
    pub(crate) fn seed<P: Put + ?Sized>(
        reader: &FileReader,
        store: &mut P,
        root: ContentId,
        crates: &[CrateRoot],
    ) -> Result<(Carry, Built), FrontError> {
        let tree = Tree::load(reader, root)?;
        let session = Session::new(&tree);
        let mut carry = Carry { kernel: kernel()?, session, tree, modules: BTreeMap::new(), asked: BTreeSet::new() };
        let crates = carry.know(crates)?;
        carry.ask(store, &crates)?;
        let built = Built { first: true, crates: crates.len(), files: carry.asked.len(), ..Built::default() };
        Ok((carry, built))
    }

    /// The session saved under `saved` ([`Carry::save`]), pinned at its root, which the VFS
    /// must still have. The crates' cfgs are known again from `crates`; every root is asked
    /// again at the next build (hits, but for what the carry reaches).
    pub(crate) fn open<G: Get + ?Sized>(
        reader: &FileReader,
        store: &G,
        saved: Saved,
        crates: &[CrateRoot],
    ) -> Result<Carry, FrontError> {
        let session = Session::open(store, saved.session)?;
        let tree = Tree::load(reader, saved.root)?;
        if session.pin() != tree.pin() {
            // alloc: an error
            return Err(FrontError::Tree(format!("the saved session is of another pin than the root {:x} it was saved with", saved.root)));
        }
        let mut carry = Carry { kernel: kernel()?, session, tree, modules: BTreeMap::new(), asked: BTreeSet::new() };
        carry.know(crates)?;
        Ok(carry)
    }

    /// Writes the session into `store` ([`Session::save`], incremental): what [`Carry::open`]
    /// takes.
    pub(crate) fn save<P: Put + ?Sized>(&mut self, store: &mut P) -> Result<Saved, FrontError> {
        Ok(Saved { session: self.session.save(store)?, root: self.tree.root })
    }

    /// The root the session is pinned at.
    pub(crate) fn root(&self) -> ContentId {
        self.tree.root
    }

    /// The build of `root` with `crates`: the session carried from its pin to `root`'s over
    /// `changes` (the VFS's diff between the two roots, which the caller computes where both
    /// are held), then the roots asked.
    ///
    /// The change set: each path the diff names differs as a `FILE` read, and as an
    /// `EXISTS` read where it came or went; nothing else of either kind differs, and no
    /// `CFG` read does ([`CrateRoot::name`]). Every kind is said, so a kind posted and left
    /// out is the kernel's refusal, never a silent stale entry.
    ///
    /// The roots asked after the carry, each a session hit unless the carry reached it or it
    /// is new: every crate's `modules` (`O(crates)` hits); a crate's files only when its
    /// `modules` value is other bytes than last asked, and then only the files not asked
    /// before; and each edited file's `items` and `lines` again. The carry repairs a row only
    /// where another step read it (`Session::asked`): a root no step reads, `lines(file)` and
    /// `modules(crate)` here, is dropped and counted `gone`, so asking it again is what keeps
    /// the session holding every root ([`Built::ran`]). A deleted file's roots are dropped
    /// and its name leaves the asked set.
    pub(crate) fn build<P: Put + ?Sized>(
        &mut self,
        reader: &FileReader,
        store: &mut P,
        root: ContentId,
        changes: &[Change],
        crates: &[CrateRoot],
    ) -> Result<Built, FrontError> {
        let crates = self.know(crates)?;
        let mut built = Built { crates: crates.len(), ..Built::default() };
        // alloc: one build's edited files, asked again after the carry
        let mut edited: Vec<Asked> = Vec::new();
        if root != self.tree.root {
            let from = self.tree.pin();
            self.tree.moved(reader, root, changes)?;
            let mut changed = Changed::between(from, self.tree.pin());
            changed.exact(reads::FILE);
            changed.exact(reads::EXISTS);
            changed.exact(reads::CFG);
            for change in changes {
                let key = change.path.as_bytes();
                changed.differs(Read { kind: reads::FILE, key });
                if change.kind != ChangeKind::Modified {
                    changed.differs(Read { kind: reads::EXISTS, key });
                }
                match change.kind {
                    ChangeKind::Removed => self.asked.retain(|Asked(name, _)| *name != change.path),
                    // clone: an edited file asked again
                    ChangeKind::Modified => edited.extend(self.asked.iter().filter(|Asked(name, _)| *name == change.path).cloned()),
                    ChangeKind::Added => {}
                }
            }
            built.paths = changes.len();
            built.carried =
                self.kernel.carry(&mut self.session, store, &self.tree, &changed, &Setting::OFF)?;
            if built.carried.dropped.is_some() {
                // The session holds nothing now: every root is asked again, as at a seed.
                self.modules.clear();
                self.asked.clear();
                edited.clear();
            }
        }
        for Asked(name, reading) in edited {
            let input = FileKey { name, reading };
            let items = self.kernel.get_in::<Items<Tree>, _>(&mut self.session, store, &self.tree, &input, &Setting::OFF)?.is_hit();
            let lines = self.kernel.get_in::<Lines<Tree>, _>(&mut self.session, store, &self.tree, &input, &Setting::OFF)?.is_hit();
            built.ran += usize::from(!items) + usize::from(!lines);
        }
        self.ask(store, &crates)?;
        built.files = self.asked.len();
        Ok(built)
    }

    /// The crates of this build, sorted by name, their cfgs known to the world. Equal names
    /// of equal roots and editions are one crate (a lib's check and build units); other roots
    /// under one name are refused naming both.
    fn know(&mut self, crates: &[CrateRoot]) -> Result<Vec<CrateKey>, FrontError> {
        // alloc: one build's crates, by name (rule 25)
        let mut by_name: BTreeMap<&str, &CrateRoot> = BTreeMap::new();
        for krate in crates {
            match by_name.get(krate.name.as_str()) {
                Some(kept) if kept.root != krate.root || kept.edition != krate.edition => {
                    // alloc: an error
                    return Err(FrontError::Crates(format!(
                        "two crates named `{}`, rooted at `{}` ({}) and at `{}` ({})",
                        krate.name,
                        kept.root,
                        kept.edition.number(),
                        krate.root,
                        krate.edition.number()
                    )));
                }
                Some(_) => {}
                None => {
                    by_name.insert(krate.name.as_str(), krate);
                }
            }
        }
        for krate in by_name.values() {
            if !self.tree.crates.contains_key(krate.name.as_str()) {
                // clone: once a crate the checkout's builds name
                self.tree.crates.insert(krate.name.clone().into_boxed_str(), krate.cfg.clone());
            }
        }
        Ok(by_name.values().map(|krate| krate.key()).collect())
    }

    /// Asks each crate's `modules`, and the `items` and `lines` of the files of each crate
    /// whose `modules` value moved that are not asked yet.
    fn ask<P: Put + ?Sized>(&mut self, store: &mut P, crates: &[CrateKey]) -> Result<(), FrontError> {
        // alloc: one crate's files at a time, reused
        let mut files: Vec<(String, Reading)> = Vec::new();
        for krate in crates {
            files.clear();
            {
                let got = self.kernel.get_in::<Modules<Tree>, _>(&mut self.session, store, &self.tree, krate, &Setting::OFF)?;
                let value = got.value();
                let hash = ContentId::of(VALUE_DOMAIN, value);
                if self.modules.get(&krate.krate) == Some(&hash) {
                    continue;
                }
                // clone: once a crate whose modules moved
                self.modules.insert(krate.krate.clone(), hash);
                let table = Table::read(MODULES, value).map_err(|error| FrontError::Modules(format!("`{}`: {error}", krate.krate)))?;
                for row in table.iter() {
                    let (_, bytes) = row.map_err(|error| FrontError::Modules(format!("`{}`: {error}", krate.krate)))?;
                    let module = Module::read(bytes).map_err(|why| FrontError::Modules(format!("`{}`: {why}", krate.krate)))?;
                    if module.status == Status::Found {
                        // alloc: one file's name, once it is new to the asked set
                        files.push((module.file.to_owned(), module.reading));
                    }
                }
            }
            for (name, reading) in files.drain(..) {
                let key = Asked(name, reading);
                if self.asked.contains(&key) {
                    continue;
                }
                let input = FileKey { name: key.0.clone(), reading }; // clone: the step's input
                self.kernel.get_in::<Items<Tree>, _>(&mut self.session, store, &self.tree, &input, &Setting::OFF)?;
                self.kernel.get_in::<Lines<Tree>, _>(&mut self.session, store, &self.tree, &input, &Setting::OFF)?;
                self.asked.insert(key);
            }
        }
        Ok(())
    }

    /// Every root's value as the session gives it now, by label: what a test compares with a
    /// fresh kernel's over the same tree.
    #[cfg(test)]
    pub(crate) fn values<P: Put + ?Sized>(
        &mut self,
        store: &mut P,
        crates: &[CrateRoot],
    ) -> Result<BTreeMap<String, Vec<u8>>, FrontError> {
        let mut out = BTreeMap::new();
        for krate in self.know(crates)? {
            let got = self.kernel.get_in::<Modules<Tree>, _>(&mut self.session, store, &self.tree, &krate, &Setting::OFF)?;
            out.insert(format!("modules {}", krate.krate), got.value().to_vec());
        }
        let asked: Vec<(String, Reading)> = self.asked.iter().map(|Asked(name, reading)| (name.clone(), *reading)).collect();
        for (name, reading) in asked {
            let input = FileKey { name: name.clone(), reading };
            let got = self.kernel.get_in::<Items<Tree>, _>(&mut self.session, store, &self.tree, &input, &Setting::OFF)?;
            out.insert(format!("items {name}"), got.value().to_vec());
            let got = self.kernel.get_in::<Lines<Tree>, _>(&mut self.session, store, &self.tree, &input, &Setting::OFF)?;
            out.insert(format!("lines {name}"), got.value().to_vec());
        }
        Ok(out)
    }

}

/// The store as the carry's task reads and writes it: its puts kept here and readable at
/// once, over `base` (on the task, the store's reader under the task's guard). A worker never
/// writes the store: the loop puts [`Overlay::into_puts`] when the task's value is back, as it
/// puts a compile's.
pub(crate) struct Overlay<'a, G: Get + ?Sized> {
    base: &'a G,
    puts: BTreeMap<StoreKey, (StoreKind, Vec<u8>)>,
}

impl<'a, G: Get + ?Sized> Overlay<'a, G> {
    pub(crate) fn new(base: &'a G) -> Overlay<'a, G> {
        Overlay { base, puts: BTreeMap::new() }
    }

    /// The puts, in key order (rule 25).
    pub(crate) fn into_puts(self) -> Vec<super::work::Put> {
        // alloc: once a pass, a put each the session saved or a step valued
        self.puts.into_iter().map(|(key, (kind, bytes))| super::work::Put { key, kind, bytes }).collect()
    }
}

impl<G: Get + ?Sized> Get for Overlay<'_, G> {
    fn get(&self, key: &StoreKey) -> Result<Option<&[u8]>, String> {
        match self.puts.get(key) {
            Some((_, bytes)) => Ok(Some(bytes)),
            None => self.base.get(key),
        }
    }

    fn has(&self, key: &StoreKey) -> Result<bool, String> {
        Ok(self.puts.contains_key(key) || self.base.has(key)?)
    }
}

impl<G: Get + ?Sized> Put for Overlay<'_, G> {
    fn put(&mut self, key: StoreKey, kind: StoreKind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        let kept = match self.puts.get(&key) {
            Some((_, kept)) => Some(kept.as_slice()),
            // A read that fails is put: the loop's put of it is the one that says what the
            // store holds.
            None => self.base.get(&key).ok().flatten(),
        };
        match kept {
            Some(kept) if *kept != *bytes => Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) }),
            Some(_) => Ok(()),
            None => {
                self.puts.insert(key, (kind, bytes));
                Ok(())
            }
        }
    }
}

/// Body entries served by the daemon (end-state.md, "The front end as kernel steps", order
/// row 3): a body's inference, the built MIR of each of its owners (the body, and each closure
/// and `async` coroutine written in it) and its borrow check's verdict as
/// `hir_ty::body_store::entries`' kernel entries, over the world of one unit's database
/// ([`Bodies`]). Every body the syntax keys is one: a function, a `static`, a `const`, an enum
/// variant's discriminant, and an `impl`'s or a trait's associated function or constant. A unit's
/// task serves its crate's unchanged bodies from the store's entries before it asks them
/// ([`serve`]) and keeps the entries of the steps its database ran after ([`keep`]); both on
/// the unit's worker, each with a session of its own, never the carry's, and the keep's puts
/// handed to the loop.
///
/// One set of steps with the carry: `parse`, `items` and `lines` are registered under the
/// carry's own code ([`super::code`]) and a file's `FILE` read is its blaze-vfs id
/// (`ContentId::of(blaze_vfs::FILE_DOMAIN, ..)`, as the carry's [`super::Tree`] gives it), so a
/// file's `items` the carry kept is the entry a body's part read finds, and the other way.
///
/// The syntax meets the database by where a body is written: a file's `items` and `lines`
/// give each body part's start and kind (`entries::keyed`), the database gives each body of a
/// crate its file, start and kind (`entries::written`), and the pairs equal in both are the
/// bodies keyed (`entries::join`). Never by name: two bodies alike but for their names, or an
/// `impl`'s methods, which have no path the syntax could name, map as any other.
pub(crate) mod bodies {
    use std::{
        alloc::Global,
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use base_db::{Crate, Db, FileId};
    use blaze_core::lock::Lock;
    use blaze_format::SchemaHash;
    use blaze_front_items::{
        Reading,
        entry::ItemId,
        steps::{FileKey, Items, Lines, Parse, Source, reads},
    };
    use blaze_ids::{ContentId, Hasher};
    use blaze_kernel::{
        Carried, Changed, CodeId, Entry, Get, Got, Kernel, KernelError, Pin, Put, Read, ReadKind,
        Seen, Session, World, WorldError,
    };
    use blaze_lower::Lowered;
    use blaze_query::db::View;
    use blaze_store_check::Setting;
    use hir_def::{DefWithBodyId, HasModule as _, nameres::crate_def_map};
    use hir_ty::{
        body_store::{
            BodyFacts as _, Step,
            entries::{
                self, BodyDelta, BodyKey, BodyWorld, BorrowckEntry, Exact, How, InferEntry, Keyed,
                MirEntry, Of, OwnerKey, Place, Written,
            },
        },
        next_solver::goal_store::FactRef,
    };

    use super::{CrateRoot, EXISTS_DOMAIN, FrontError, code};

    /// The pin's domain: of every file's path and content, and the database's revision (a
    /// dependency's edit moves facts under files that are equal, so the revision is in it).
    const PIN_DOMAIN: &str = "blaze v2 body entries pin 2";

    /// The steps body entries are served and kept by: the carry's `parse`, `items` and `lines`,
    /// and the three body steps of the front end `front_end` (`deps::front_end`, the item
    /// store's).
    pub(crate) fn kernel(front_end: ContentId) -> Result<Kernel<Bodies>, KernelError> {
        let mut kernel = Kernel::new();
        kernel.register_ask::<Parse<Bodies>>(code(0))?;
        kernel.register_ask::<Items<Bodies>>(code(1))?;
        kernel.register_ask::<Lines<Bodies>>(code(2))?;
        entries::register(&mut kernel, CodeId(front_end))?;
        Ok(kernel)
    }

    #[cfg(test)]
    thread_local! {
        /// The planted bug of `serve::unit::tests`' nested owners control: [`serve`] tries the
        /// MIR of [`Place::BODY`] alone, as a serve blind to a body's closures and coroutines.
        pub(crate) static BODY_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The planted bug of `serve::unit::tests`' leak control: the world sees every fact of
        /// what another body leaks (a hidden type, an auto trait's leakage, a constant's value)
        /// as gone, whatever it is, so an entry that read one verifies after the leak changed.
        pub(crate) static BLIND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        /// The planted bug of `serve::unit::tests`' delta control: [`serve`]'s carry says the
        /// identity reads exactly, with none differing (`by: "a planted bug"`), as a producer
        /// that says every identity is unchanged.
        pub(crate) static CLEAN_IDENTITIES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    #[cfg(test)]
    fn clean_identities() -> bool {
        CLEAN_IDENTITIES.get()
    }

    #[cfg(not(test))]
    fn clean_identities() -> bool {
        false
    }

    /// The owners of `def` whose MIR [`serve`] tries, in order: every place `entries::places`
    /// gives (the body, then each closure and `async` coroutine in expression order).
    fn owner_places(db: &Db, def: DefWithBodyId) -> Vec<Place> {
        #[cfg(test)]
        if BODY_ONLY.get() {
            // alloc: the planted bug's one place
            return vec![Place::BODY];
        }
        entries::places(db, def)
    }

    /// Whether `read` is a fact the world is blind to under [`BLIND`].
    #[cfg(test)]
    fn blind(read: Read<'_>) -> bool {
        use blaze_items::kinds::{AUTO_TRAITS, CONST_VALUE, HIDDEN_TYPES};
        BLIND.get() && [HIDDEN_TYPES, AUTO_TRAITS, CONST_VALUE].iter().any(|it| it.0 == read.kind.0)
    }

    /// A crate's file of the view as a map has it: its file in the database, its text's
    /// content as the database has it (`FileText::content`, what says the file is as the last
    /// map had it), its blaze-vfs id, the crate's name and reading, and its body parts
    /// (`entries::keyed`) once the map has them.
    pub(crate) struct Mapped {
        id: FileId,
        text: ContentId,
        content: ContentId,
        krate: Arc<str>,
        reading: Reading,
        keyed: Option<Vec<Keyed>>,
    }

    /// The world of one unit's task: its database (the reader's front end, at the revision the
    /// task reads), the files of the crates asked for that are in the view, by their path in
    /// it, and each body of those files by its key and by the database's id.
    pub(crate) struct Bodies {
        front: Arc<Lowered>,
        files: BTreeMap<String, Mapped>,
        /// Each file of `files` by its id in the database: where a body written there is.
        by_file: blaze_hash::HashMap<FileId, String, Global>,
        /// The crates asked for whose root is in the view, each with its stable name.
        crates: Vec<(Crate, Arc<str>)>,
        by_key: blaze_hash::HashMap<(String, ItemId), DefWithBodyId, Global>,
        by_def: blaze_hash::HashMap<DefWithBodyId, BodyKey, Global>,
        /// A body of each crate by its name: what a fact read seen from the crate is valued by
        /// (`BodyFacts::value` takes a body of the crate that reads it).
        seers: BTreeMap<Arc<str>, DefWithBodyId>,
        /// Set while [`serve`] carries a session: a body's step reached then is not run
        /// ([`BodyWorld::run`] refuses it), so the carry drops an entry the store has none of
        /// and the unit's own task runs the step, never the serve.
        carrying: AtomicBool,
        /// What the map passed over, each counted for the span (rule 3: said, not dropped):
        /// crates and files outside the view and the bodies written in one; bodies with no
        /// place in a file (in a macro's expansion, under an attribute macro); bodies written
        /// in a file of the view that no key of its syntax is (a block's item, a kind or start
        /// the two sides do not agree on).
        pub(crate) outside: usize,
        pub(crate) in_macros: usize,
        pub(crate) unmapped: usize,
    }

    /// The pin of `files` at the database's revision `revision` ([`World::pin`]).
    fn pin_of(files: &BTreeMap<String, Mapped>, revision: View) -> Pin {
        let mut hasher = Hasher::new(PIN_DOMAIN);
        for (path, file) in files {
            hasher.part(path.as_bytes()).part(&file.content.0);
        }
        hasher.part(&revision.as_u64().to_le_bytes());
        Pin(hasher.finish())
    }

    impl Bodies {
        /// The world of `crates` of `front`'s database: every file of each crate (its root's
        /// and each `mod` file's) whose path `in_view` gives a path in the view for. A crate
        /// whose root is outside the view has none. A file of `last` (the files a kept crate's
        /// last map had) whose text and reading are as they were there is taken from it, its
        /// content and body parts with it: only a file whose text moved is hashed and mapped.
        pub(crate) fn of(
            front: Arc<Lowered>,
            crates: &[Crate],
            in_view: &impl Fn(&str) -> Option<String>,
            mut last: Option<BTreeMap<String, Mapped>>,
        ) -> Bodies {
            let mut world = Bodies {
                front,
                files: BTreeMap::new(),
                by_file: blaze_hash::HashMap::new_in(Global),
                crates: Vec::new(),
                by_key: blaze_hash::HashMap::new_in(Global),
                by_def: blaze_hash::HashMap::new_in(Global),
                seers: BTreeMap::new(),
                carrying: AtomicBool::new(false),
                outside: 0,
                in_macros: 0,
                unmapped: 0,
            };
            // clone: a counted reference, so the world is filled while the database is read
            let front = Arc::clone(&world.front);
            let db = front.db();
            let paths = front.paths();
            for &krate in crates {
                let data = krate.data(db);
                let Some(root) = paths.get(&data.root_file_id).and_then(|it| in_view(it.as_str())) else {
                    world.outside += 1;
                    continue;
                };
                let extensions = krate.extensions(db);
                // clone: once a crate a task
                let cfg = krate.cfg_options(db).clone();
                // alloc: the crate's stable name, once a crate a task
                let name: Arc<str> =
                    Arc::from(CrateRoot::new(&root, root.clone(), data.edition, cfg, extensions).name);
                let reading = Reading { edition: data.edition, extensions };
                let def_map = crate_def_map(db, krate);
                for (_, module_data) in def_map.modules(db) {
                    let Some(file) = module_data.origin.file_id() else { continue };
                    let id = file.file_id(db);
                    let Some(path) = paths.get(&id).and_then(|it| in_view(it.as_str())) else {
                        world.outside += 1;
                        continue;
                    };
                    let text = db.file_text(id).content(db);
                    let was = last.as_mut().and_then(|it| it.remove(&path));
                    let (content, keyed) = match was {
                        Some(was) if was.text == text && was.reading == reading => (was.content, was.keyed),
                        _ => {
                            let bytes = db.file_text(id).text(db);
                            (ContentId::of(blaze_vfs::FILE_DOMAIN, bytes.as_str().as_bytes()), None)
                        }
                    };
                    // clone: a file's path, in both maps
                    world.by_file.insert(id, path.clone());
                    // clone: a counted reference, once a file
                    let mapped = Mapped { id, text, content, krate: Arc::clone(&name), reading, keyed };
                    world.files.insert(path, mapped);
                }
                world.crates.push((krate, name));
            }
            world
        }

        /// The files as this world mapped them, each with its body parts: what a kept crate
        /// keeps for its next map ([`Bodies::of`]'s `last`).
        fn into_files(self) -> BTreeMap<String, Mapped> {
            self.files
        }

        /// The `items` and `lines` of each file with no body parts yet (a file whose text
        /// moved, or every file of a world with nothing kept) got from the kernel in `session`
        /// (run where the store has none, kept into `store`), and each body mapped.
        fn map_got<P: Put + ?Sized>(
            &mut self,
            kernel: &Kernel<Bodies>,
            session: &mut Session,
            store: &mut P,
        ) -> Result<(), FrontError> {
            // alloc: once a task, the moved files' items' and lines' bytes until the world is filled
            let mut found: Vec<(String, Vec<u8>, Vec<u8>)> = Vec::new();
            for (path, file) in &self.files {
                if file.keyed.is_some() {
                    continue;
                }
                // clone: the steps' input
                let input = FileKey { name: path.clone(), reading: file.reading };
                let items = kernel.get_in::<Items<Bodies>, P>(session, store, self, &input, &Setting::OFF)?.value().to_vec();
                let lines = kernel.get_in::<Lines<Bodies>, P>(session, store, self, &input, &Setting::OFF)?.value().to_vec();
                found.push((input.name, items, lines));
            }
            self.fill(found)
        }

        /// Gives each file of `found` its body parts (`entries::keyed`, from its `items` and
        /// `lines`), then maps each body of the crates the database has (`crate_body_owners`)
        /// to its key: its place as the database has it (`entries::written`) joined to its
        /// file's body parts by start and kind.
        fn fill(&mut self, found: Vec<(String, Vec<u8>, Vec<u8>)>) -> Result<(), FrontError> {
            for (path, items, lines) in found {
                // alloc: an error
                let parts = entries::keyed(&items, &lines).map_err(|why| FrontError::Tree(format!("`{path}`: {why}")))?;
                if let Some(file) = self.files.get_mut(&path) {
                    file.keyed = Some(parts);
                }
            }
            // clone: a counted reference, so the world is filled while the database is read
            let front = Arc::clone(&self.front);
            let db = front.db();
            // alloc: once a task, each file's bodies as the database has them, by path (rule 25)
            let mut written: BTreeMap<String, Vec<(DefWithBodyId, Written)>> = BTreeMap::new();
            for (krate, _) in &self.crates {
                for def in hir_def::nameres::demand::crate_body_owners(db, *krate) {
                    let Some(at) = entries::written(db, def) else {
                        self.in_macros += 1;
                        continue;
                    };
                    match self.by_file.get(&at.file) {
                        // clone: a file's path, once a file with a body
                        Some(path) => written.entry(path.clone()).or_default().push((def, at)),
                        None => self.outside += 1,
                    }
                }
            }
            for (path, bodies) in written {
                let file = &self.files[&path];
                let parts = file.keyed.as_deref().unwrap_or(&[]);
                let pairs = entries::join(parts, &bodies);
                self.unmapped += bodies.len().saturating_sub(pairs.len());
                let reading = file.reading;
                // clone: a counted reference
                let krate = Arc::clone(&file.krate);
                for (item, def) in pairs {
                    // clone: one body's key, in both maps
                    let key = BodyKey { file: FileKey { name: path.clone(), reading }, item, krate: krate.to_string() };
                    self.by_key.insert((path.clone(), item), def);
                    self.seers.entry(Arc::clone(&krate)).or_insert(def);
                    self.by_def.insert(def, key);
                }
            }
            Ok(())
        }

        /// Every body mapped, by its key, in key order (rule 25).
        fn keys(&self) -> Vec<BodyKey> {
            // alloc: once a task, the mapped bodies' keys
            let mut keys: Vec<BodyKey> = self.by_def.values().cloned().collect();
            keys.sort_unstable_by(|a, b| (&a.file.name, a.item.0).cmp(&(&b.file.name, b.item.0)));
            keys
        }

        /// Why `def`, a body of this world's database, has no key: counted on `did` as a body
        /// with no place in a file, one written outside the view, or one the syntax has no key
        /// of.
        fn passed_over(&self, def: DefWithBodyId, did: &mut Did) {
            match entries::written(self.front.db(), def) {
                None => did.in_macros += 1,
                Some(at) if !self.by_file.contains_key(&at.file) => did.outside += 1,
                Some(_) => did.unmapped += 1,
            }
        }
    }

    impl World for Bodies {
        fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
            #[cfg(test)]
            if blind(read) {
                return Ok(Seen::Gone);
            }
            if let Some(hashed) = entries::hash(self, read) {
                return hashed;
            }
            let name = || core::str::from_utf8(read.key).map_err(|_| WorldError::new(read, "a path that is not UTF-8"));
            match read.kind {
                reads::FILE => Ok(self.files.get(name()?).map_or(Seen::Gone, |it| Seen::Is(it.content))),
                reads::EXISTS => Ok(match self.files.contains_key(name()?) {
                    true => Seen::Is(ContentId::of(EXISTS_DOMAIN, b"there")),
                    false => Seen::Gone,
                }),
                _ => Err(WorldError::new(read, "a kind of read the body entries' world does not answer (a crate's modules are the carry's)")),
            }
        }

        fn pin(&self) -> Pin {
            pin_of(&self.files, self.front.db().nonce_and_revision().1)
        }
    }

    impl Source for Bodies {
        fn file(&self, name: &str) -> Result<Option<&str>, WorldError> {
            let db = self.front.db();
            Ok(self.files.get(name).map(|it| db.file_text(it.id).text(db).as_str()))
        }

        fn cfg(&self, krate: &str, _: &str) -> Result<bool, WorldError> {
            let read = Read { kind: reads::CFG, key: krate.as_bytes() };
            Err(WorldError::new(read, "a cfg read, which no step of a body asks (a crate's modules are the carry's)"))
        }
    }

    /// Why no borrow check's verdict of a body is an entry: what [`BodyWorld::run`] and
    /// [`BodyWorld::schema`] say for [`Of::Borrowck`] when `hir_ty_borrowck` gives no checker.
    const NO_VERDICT: &str = "its borrow check did not pass silently, or none runs";

    impl BodyWorld for Bodies {
        fn db(&self) -> &Db {
            self.front.db()
        }

        fn body(&self, key: &BodyKey) -> Result<DefWithBodyId, String> {
            // clone: a map key
            match self.by_key.get(&(key.file.name.clone(), key.item)) {
                Some(def) => Ok(*def),
                // alloc: error path
                None => Err(format!("no body of {key:?} in this task's syntax")),
            }
        }

        fn run(&self, def: DefWithBodyId, of: Of) -> Result<SchemaHash, String> {
            if self.carrying.load(Ordering::Relaxed) {
                // alloc: error path
                return Err(String::from("a carry runs no body's step: its unit does"));
            }
            let db = self.front.db();
            match of {
                Of::Infer => Ok(entries::run_infer(db, def)),
                Of::Mir(place) => entries::run_mir(db, def, place),
                // The driver's borrow check (the query, memoized: it ran, so its memo answers),
                // whose verdict is kept only where it passed with nothing to say.
                Of::Borrowck => match hir_ty_borrowck::silent_pass(db, def) {
                    Some(checker) => Ok(entries::borrowck_schema(db, def, checker)),
                    // alloc: error path
                    None => Err(NO_VERDICT.to_owned()),
                },
            }
        }

        fn absolute(&self, def: DefWithBodyId, fact: FactRef) -> FactRef {
            match fact.subject == blaze_item_hash::values::OWN {
                true => FactRef { subject: blaze_item_hash::values::body_key(self.front.db(), def), ..fact },
                false => fact,
            }
        }

        fn fact(&self, krate: &str, fact: FactRef) -> Result<Seen, WorldError> {
            // `seer` says only which crate sees the fact: no fact here names a body
            // relatively (`absolute`), so its value is the same from any body of the crate.
            let Some(&seer) = self.seers.get(krate) else {
                // alloc: error path
                let mut key = Vec::new();
                let kind = entries::fact_read(fact, krate, &mut key);
                return Err(WorldError::new(Read { kind, key: &key }, "a fact seen from a crate this task mapped no body of"));
            };
            if fact.subject == blaze_item_hash::values::OWN {
                // alloc: error path
                let mut key = Vec::new();
                let kind = entries::fact_read(fact, krate, &mut key);
                return Err(WorldError::new(Read { kind, key: &key }, "a fact about a body's own item that names no body: an entry reads it by the item (`BodyWorld::absolute`)"));
            }
            let value = blaze_item_hash::Bodies.value(self.front.db(), seer, fact);
            Ok(value.map_or(Seen::Gone, Seen::Is))
        }

        fn schema(&self, def: DefWithBodyId, of: Of) -> Result<SchemaHash, String> {
            let db = self.front.db();
            match of {
                Of::Infer => Ok(entries::infer_schema()),
                Of::Mir(place) => Ok(entries::mir_schema(db, def, place)),
                Of::Borrowck => match hir_ty_borrowck::checker_name(db) {
                    Some(checker) => Ok(entries::borrowck_schema(db, def, checker)),
                    // alloc: error path
                    None => Err(NO_VERDICT.to_owned()),
                },
            }
        }
    }

    /// Where a serve's session came from: kept in memory from the crate's last serve, opened
    /// from the root its last save gave (the journal's mark, after a restart or a dropped
    /// front end), or new: none was kept or marked, or the marked one did not open or carry.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) enum Opened {
        #[default]
        New,
        Kept,
        Opened,
        Unreadable,
    }

    impl Opened {
        /// The span's word for it.
        pub(crate) fn name(self) -> &'static str {
            match self {
                Opened::New => "new",
                Opened::Kept => "kept",
                Opened::Opened => "opened",
                Opened::Unreadable => "unreadable",
            }
        }
    }

    /// The profile's name of a kind of read a body entry makes: the files', the identity's,
    /// and each fact kind of `blaze_items::kinds` and `blaze_item_hash::values::kinds`.
    pub(crate) fn kind_name(kind: ReadKind) -> &'static str {
        use blaze_item_hash::values::kinds as hashed;
        use blaze_items::kinds as items;
        const FACTS: [(blaze_query::QueryKind, &str); 36] = [
            (items::SIGNATURE, "signature"),
            (items::LAYOUT, "layout"),
            (items::FIELD, "field"),
            (items::METHOD, "method"),
            (items::IMPLEMENTS, "implements"),
            (items::RESOLVES, "resolves"),
            (items::CODE, "code"),
            (items::CONST_VALUE, "const value"),
            (items::NAME, "name"),
            (items::CAPTURES, "captures"),
            (items::AUTO_TRAITS, "auto traits"),
            (items::COHERENCE, "coherence"),
            (items::SPECIALIZES, "specializes"),
            (items::DROP_CHECK, "drop check"),
            (items::VTABLE, "vtable"),
            (items::DERIVE, "derive"),
            (items::HASHES, "hashes"),
            (items::TARGET, "target"),
            (items::CRATE, "crate"),
            (items::HIDDEN_TYPES, "hidden types"),
            (items::RPITIT_TYS, "rpitit tys"),
            (items::INHERENT_IMPLS, "inherent impls"),
            (items::TRAIT_IMPLS, "trait impls"),
            (items::TRAITS_NAMED, "traits named"),
            (items::SCOPE_TRAITS_NAMED, "scope traits named"),
            (items::BLOCK_IMPLS, "block impls"),
            (hashed::FIELDS, "fields"),
            (hashed::ATTRS, "attrs"),
            (hashed::CRATE_ROOT, "crate root"),
            (hashed::STEP_INFER, "step infer"),
            (hashed::STEP_MIR, "step mir"),
            (hashed::MATCH_CHECK, "match check"),
            (hashed::TRAITS_IN_SCOPE, "traits in scope"),
            (hashed::WHOLE_SCOPE, "whole scope"),
            (hashed::CRATE_NAMES, "crate names"),
            (hashed::FIELD_ORDER, "field order"),
        ];
        if kind == reads::FILE {
            return "file";
        }
        if kind == reads::EXISTS {
            return "exists";
        }
        if kind == entries::reads::IDENTITY {
            return "identity";
        }
        FACTS.iter().find(|(it, _)| it.0 == kind.0).map_or("a fact kind with no name here", |(_, name)| name)
    }

    /// One served step of a body: the identity of the entry it was served from and that
    /// entry's key, so a later serve knows the row still stands where the session holds that
    /// very entry live for the identity.
    struct Row {
        id: ContentId,
        key: blaze_cas::Key,
    }

    fn row_of(entry: &Entry<'_>) -> Row {
        Row { id: entry.identity.id(), key: entry.key }
    }

    /// What one crate keeps between serves, beside its front end's database: the kernel
    /// session, the file map (each file's content and body parts) and each body's rows as
    /// posted (its id in the database, each served step's entry, and whether every step of it
    /// was served), by its file and item. `since`: the revision from which every row of the
    /// crate posted is in `posted` (the stamp's, [`entries::stamp`]).
    pub(crate) struct Kept {
        session: Session,
        files: BTreeMap<String, Mapped>,
        posted: BTreeMap<(String, ItemId), (DefWithBodyId, Vec<Row>, bool)>,
        since: View,
    }

    enum Slot {
        Here(Kept),
        /// A task has the crate's [`Kept`] (or the right to make one).
        Out,
        /// A task has it, and another served the crate while it was out: what comes back is
        /// dropped, as its rows do not know what the other posted.
        Spoiled,
    }

    /// Each crate's [`Kept`], lent to one task at a time ([`Sessions::take`]). The lock is for
    /// the map alone, never across a kernel call (rule 27). A task that finds a crate's out
    /// uses a new session and puts nothing back.
    pub(crate) struct Sessions(Lock<BTreeMap<Crate, Slot>>);

    impl Default for Sessions {
        fn default() -> Sessions {
            Sessions(Lock::new("serve.bodies.sessions", BTreeMap::new()))
        }
    }

    /// A crate's [`Kept`] as [`Sessions::take`] lent it: `kept` is none where none was kept
    /// or it is out. [`Lent::put`] gives the crate its next one; dropped without, the crate
    /// keeps none (the task failed, and its rows may be half posted).
    pub(crate) struct Lent<'a> {
        sessions: &'a Sessions,
        krate: Crate,
        kept: Option<Kept>,
        /// This task holds the crate's slot.
        holds: bool,
    }

    impl Sessions {
        /// Lends `krate`'s [`Kept`] to the caller.
        pub(crate) fn take(&self, krate: Crate) -> Lent<'_> {
            let mut map = self.0.lock();
            let (kept, holds) = match map.remove(&krate) {
                Some(Slot::Here(kept)) => (Some(kept), true),
                Some(Slot::Out | Slot::Spoiled) => {
                    map.insert(krate, Slot::Spoiled);
                    return Lent { sessions: self, krate, kept: None, holds: false };
                }
                None => (None, true),
            };
            map.insert(krate, Slot::Out);
            Lent { sessions: self, krate, kept, holds }
        }
    }

    impl Lent<'_> {
        /// `kept` is the crate's from here on, unless the crate was served by another task
        /// while this one held it, or this one never held it: then it is dropped.
        pub(crate) fn put(&mut self, kept: Kept) {
            if !std::mem::replace(&mut self.holds, false) {
                return;
            }
            let mut map = self.sessions.0.lock();
            let left = match map.remove(&self.krate) {
                Some(Slot::Out) => {
                    map.insert(self.krate, Slot::Here(kept));
                    None
                }
                _ => Some(kept),
            };
            drop(map);
            // A spoiled slot's: dropped outside the lock.
            drop(left);
        }
    }

    impl Drop for Lent<'_> {
        fn drop(&mut self) {
            if self.holds {
                self.sessions.0.lock().remove(&self.krate);
            }
        }
    }

    /// What a serve or a keep did: its span's fields. A serve counts bodies and steps of the
    /// crate it serves; a keep counts the steps it claimed.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub(crate) struct Did {
        /// The task's claim was not the first: nothing done.
        pub(crate) skipped: bool,
        /// The database has no item store (`BLAZE_ITEM_STORE=off`): nothing done.
        pub(crate) no_store: bool,
        /// Where the serve's session came from.
        pub(crate) session: Opened,
        /// The bodies the world keyed.
        pub(crate) bodies: usize,
        /// Steps served from their entries (a serve), each looked up.
        pub(crate) served: usize,
        /// Steps left posted from an earlier revision under the crate's stamp (a serve): their
        /// entries are the session's live ones still, so nothing was looked up or copied.
        pub(crate) carried: usize,
        /// Steps not served (a serve): each step whose entry was not found or did not verify,
        /// and each later step of its body that was not tried for it; a body whose inference
        /// is not served counts its MIR as one, as its places are its inference's. In a keep,
        /// the claimed runs of a constant's value, which has no entry yet.
        pub(crate) missed: usize,
        /// Entries kept (a keep).
        pub(crate) kept: usize,
        /// Runs the item store refused to trace, and runs whose entry's step refused (its
        /// trace was refused, or it read one that was): no entry is kept of them.
        pub(crate) refused: usize,
        /// Crates and files outside the view, and bodies written in one.
        pub(crate) outside: usize,
        /// Bodies with no place in a file: in a macro's expansion or under an attribute macro.
        pub(crate) in_macros: usize,
        /// Bodies written in a file of the view that no key of its syntax is.
        pub(crate) unmapped: usize,
        /// Runs of a MIR no entry keys (an inline const's, a by-move or a `gen` coroutine's):
        /// claimed, not kept.
        pub(crate) unkeyed: usize,
        /// What the serve's carry of its session did (all zero where none ran).
        pub(crate) carry: Carried,
    }

    impl Did {
        fn of(world: &Bodies) -> Did {
            Did {
                outside: world.outside,
                in_macros: world.in_macros,
                unmapped: world.unmapped,
                bodies: world.by_def.len(),
                ..Did::default()
            }
        }
    }

    /// The session a serve with nothing kept starts from: the one saved under `saved` (the
    /// crate's mark), else a new one. One that does not open is said, named, and a new one.
    fn open_or_new<G: Get + ?Sized>(store: &G, saved: Option<blaze_cas::Key>, world: &Bodies, did: &mut Did) -> Session {
        let Some(root) = saved else {
            did.session = Opened::New;
            return Session::new(world);
        };
        match Session::open(store, root) {
            Ok(session) => {
                did.session = Opened::Opened;
                session
            }
            Err(error) => {
                let krate = world.crates.first().map_or("", |(_, name)| &**name);
                tracing::warn!(target: "blaze", krate, root = ?root, "the body entries' saved session does not open, so the crate's bodies are served by a new one: {error}");
                did.session = Opened::Unreadable;
                Session::new(world)
            }
        }
    }

    /// Carries `session` from its pin to `world`'s: every kind it reads said by
    /// [`BodyDelta::say`] (each valued, but where a producer says it exactly), one event
    /// `body reads valued` a kind with its `names` (what a valuation of the kind costs, and
    /// which producer to build next), then the kernel's carry, no body's step run in it.
    fn carry<P: Put + ?Sized>(
        kernel: &Kernel<Bodies>,
        session: &mut Session,
        store: &mut P,
        world: &Bodies,
    ) -> Result<Carried, FrontError> {
        let mut changed = Changed::between(session.pin(), world.pin());
        let delta = match clean_identities() {
            // alloc: the planted bug's one producer
            true => BodyDelta { exact: vec![Exact { kind: entries::reads::IDENTITY, by: "a planted bug", keys: Vec::new() }] },
            false => BodyDelta::default(),
        };
        for (kind, how) in delta.say(session, &mut changed) {
            let names = session.names_of(&*store, kind)?;
            let how = match how {
                How::Exact(by) => by,
                How::Valued => "valued",
            };
            tracing::info!(target: "blaze", kind = kind_name(kind), names = names as u64, how, "body reads valued");
        }
        world.carrying.store(true, Ordering::Relaxed);
        let carried = kernel.carry(session, store, world, &changed, &Setting::OFF);
        world.carrying.store(false, Ordering::Relaxed);
        Ok(carried?)
    }

    /// Whether every row of a body still stands: `session` holds, live for each row's
    /// identity, the very entry the row was served from.
    fn holds<G: Get + ?Sized>(session: &mut Session, store: &G, rows: &[Row]) -> Result<bool, FrontError> {
        for row in rows {
            if session.entry_of(store, &row.id)? != Some(row.key) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Serves the body `key` (`def`) from its entries in `session`: its inference; then, from
    /// the inference just posted, the MIR of each of its owners; then, only where every
    /// owner's MIR was served, the borrow check. The rows posted, and whether every step was.
    fn serve_body<G: Get + ?Sized>(
        kernel: &Kernel<Bodies>,
        session: &mut Session,
        store: &G,
        world: &Bodies,
        key: &BodyKey,
        def: DefWithBodyId,
        did: &mut Did,
    ) -> Result<(Vec<Row>, bool), FrontError> {
        let tried = |it: Result<(), String>| it.map_err(FrontError::Tree);
        let db = world.front.db();
        // alloc: a body's rows, a handful
        let mut rows = Vec::new();
        let Some(infer) = kernel.find_in::<InferEntry<Bodies>, G>(session, store, world, key)? else {
            // Its inference, its MIR and its borrow check.
            did.missed += 3;
            return Ok((rows, false));
        };
        rows.push(row_of(&infer));
        tried(entries::serve(world, key, Of::Infer, &Got::Hit(infer)))?;
        did.served += 1;
        let mut every = true;
        for place in owner_places(db, def) {
            // clone: a body's key, once an owner
            let owner = OwnerKey { body: key.clone(), place };
            match kernel.find_in::<MirEntry<Bodies>, G>(session, store, world, &owner)? {
                Some(mir) => {
                    rows.push(row_of(&mir));
                    tried(entries::serve(world, key, Of::Mir(place), &Got::Hit(mir)))?;
                    did.served += 1;
                }
                None => {
                    did.missed += 1;
                    every = false;
                }
            }
        }
        let verdict = match every {
            true => kernel.find_in::<BorrowckEntry<Bodies>, G>(session, store, world, key)?,
            false => None,
        };
        match verdict {
            Some(verdict) => {
                rows.push(row_of(&verdict));
                tried(entries::serve(world, key, Of::Borrowck, &Got::Hit(verdict)))?;
                did.served += 1;
                Ok((rows, true))
            }
            None => {
                did.missed += 1;
                Ok((rows, false))
            }
        }
    }

    /// Serves `krate`'s bodies in `front`'s database from `store`: each body's step whose entry
    /// the store has, and whose reads hash the same now, is posted to the item store
    /// (`entries::serve`), and the step's query answers from it at this revision. Nothing is
    /// run and nothing written but the session's save and the moved files' `items`: a lookup
    /// alone. The first serve of the crate at the revision does it; any other is `skipped`.
    ///
    /// The crate's [`Kept`] makes a revision cost what changed: the files whose text moved
    /// are mapped again, the kept session (or, with none kept, the one `marks` names under the
    /// crate's stable name, else a new one) is carried to this pin, and a body whose every
    /// row's entry is still the session's live one is left posted, answering under the
    /// crate's stamp ([`entries::stamp`]) with nothing looked up; any other body is unposted
    /// and served from its entries. Gives what it did and the crate's stable name with the
    /// root its session was saved under (none where nothing was served).
    pub(crate) fn serve<P: Put + ?Sized>(
        kernel: &Kernel<Bodies>,
        front: &Arc<Lowered>,
        krate: Crate,
        in_view: &impl Fn(&str) -> Option<String>,
        sessions: &Sessions,
        marks: &BTreeMap<String, blaze_cas::Key>,
        store: &mut P,
    ) -> Result<(Did, Option<(String, blaze_cas::Key)>), FrontError> {
        match entries::claim_serve(front.db(), krate) {
            Some(true) => {}
            Some(false) => return Ok((Did { skipped: true, ..Did::default() }, None)),
            None => return Ok((Did { no_store: true, ..Did::default() }, None)),
        }
        let db = front.db();
        let (_, now) = db.nonce_and_revision();
        let mut lent = sessions.take(krate);
        let (last, held) = match lent.kept.take() {
            Some(Kept { session, files, posted, since }) => (Some(files), Some((session, posted, since))),
            None => (None, None),
        };
        // clone: a counted reference, the world's
        let mut world = Bodies::of(Arc::clone(front), &[krate], in_view, last);
        let Some(name) = world.crates.first().map(|(_, name)| name.to_string()) else {
            // The crate's root is outside the view: nothing of it is mapped or kept.
            return Ok((Did::of(&world), None));
        };
        // The moved files' `parse`, `items` and `lines`, in a session of their own (the kept
        // one is of the last pin until it is carried): run where the store has none (an edited
        // file's, at its new content) and kept into `store`, so the unedited bodies of an
        // edited file map. No body's step is run here.
        world.map_got(kernel, &mut Session::new(&world), store)?;
        let mut did = Did::of(&world);
        let (mut session, mut posted, since) = match held {
            Some((session, posted, since)) => {
                did.session = Opened::Kept;
                (session, posted, since)
            }
            None => (open_or_new(&*store, marks.get(&name).copied(), &world, &mut did), BTreeMap::new(), now),
        };
        let carries = match did.session {
            Opened::Opened => true,
            Opened::Kept => session.pin() != world.pin(),
            Opened::New | Opened::Unreadable => false,
        };
        if carries {
            match blaze_query::db::attach(db, || carry(kernel, &mut session, store, &world)) {
                Ok(carried) => did.carry = carried,
                Err(error) if did.session == Opened::Opened => {
                    tracing::warn!(target: "blaze", krate = %name, "the body entries' saved session does not carry, so the crate's bodies are served by a new one: {error}");
                    did.session = Opened::Unreadable;
                    session = Session::new(&world);
                }
                Err(error) => return Err(error),
            }
        }
        // alloc: once a serve, the bodies posted now, by key (rule 25)
        let mut now_posted = BTreeMap::new();
        let looked: &P = store;
        blaze_query::db::attach(db, || -> Result<(), FrontError> {
            for key in world.keys() {
                let def = world.body(&key).map_err(FrontError::Tree)?;
                // clone: a body's path, a map key
                let at = (key.file.name.clone(), key.item);
                if let Some((old, rows, whole)) = posted.remove(&at) {
                    if whole && old == def && holds(&mut session, looked, &rows)? {
                        did.carried += rows.len();
                        now_posted.insert(at, (def, rows, true));
                        continue;
                    }
                    entries::unpost(db, old);
                }
                let (rows, whole) = serve_body(kernel, &mut session, looked, &world, &key, def, &mut did)?;
                now_posted.insert(at, (def, rows, whole));
            }
            Ok(())
        })?;
        // A body posted at the last serve that is no body of the world now.
        for (def, _, _) in posted.into_values() {
            entries::unpost(db, def);
        }
        // Every row of the crate posted since `since` is now in `now_posted`, and each whose
        // entry moved was unposted above: what is left answers at this revision.
        entries::stamp(db, krate, since);
        let root = session.save(store)?;
        lent.put(Kept { session, files: world.into_files(), posted: now_posted, since });
        Ok((did, Some((name, root))))
    }

    /// Where `of` comes among a body's steps: the order they read each other in (inference,
    /// then each owner's MIR by place, then the borrow check), for a keep's order (rule 25).
    fn rank(of: Of) -> (u8, Place) {
        match of {
            Of::Infer => (0, Place::BODY),
            Of::Mir(place) => (1, place),
            Of::Borrowck => (2, Place::BODY),
        }
    }

    /// The refusal `error` carries, if it is one: a step that refused its input
    /// (`StepError::Refused`, which the kernel gives as [`KernelError::Compute`]), or a step
    /// whose step it asked for refused ([`KernelError::Child`] down to one).
    fn refusal(error: &KernelError) -> Option<&str> {
        match error {
            KernelError::Compute { why, .. } => Some(why),
            KernelError::Child { child, .. } => refusal(child),
            _ => None,
        }
    }

    /// Keeps the entry of every step `front`'s database ran that no keep has claimed
    /// (`entries::claim_keeps`), into `store`: each got from the kernel, whose run of the step
    /// is the database's memoized query (it ran, so the memo answers), with what the item store
    /// traced of it. A crate at a time, in crate order: a crate's [`Kept`] whose session is of
    /// this pin (its serve ran at this revision) keeps them, is saved and put back, its stable
    /// name and root given; any other crate's are kept through a new session of its world,
    /// mapped whole. A body outside the view, or that the syntax does not key, keeps no entry
    /// and is counted; so is a step that refused (a refused trace, or a read of one), named in
    /// a debug event, and the keep goes on. Any other error is the keep's, and the claim's
    /// drop lets every run it claimed go to the next keep. The claim is kept last, once
    /// nothing here can fail: a caller hands the puts on with nothing fallible after.
    pub(crate) fn keep<P: Put + ?Sized>(
        kernel: &Kernel<Bodies>,
        front: &Arc<Lowered>,
        in_view: &impl Fn(&str) -> Option<String>,
        sessions: &Sessions,
        store: &mut P,
    ) -> Result<(Did, Vec<(String, blaze_cas::Key)>), FrontError> {
        let db = front.db();
        let Some(claim) = entries::claim_keeps(db) else {
            return Ok((Did { no_store: true, ..Did::default() }, Vec::new()));
        };
        let mut did = Did::default();
        for it in &claim.refused {
            did.refused += 1;
            tracing::debug!(target: "blaze", def = ?it.def, step = ?it.step, "a run the item store refused to trace, kept as no entry: {}", it.why);
        }
        // alloc: once a keep, the steps it keeps by crate, in order (rule 25)
        let mut by_crate: BTreeMap<Crate, Vec<(DefWithBodyId, Step, SchemaHash)>> = BTreeMap::new();
        for &(def, step, schema) in &claim.steps {
            by_crate.entry(def.krate(db)).or_default().push((def, step, schema));
        }
        // alloc: once a keep, the sessions it saved
        let mut saved = Vec::new();
        for (krate, steps) in by_crate {
            keep_crate(kernel, front, krate, &steps, in_view, sessions, store, &mut did, &mut saved)?;
        }
        claim.kept();
        Ok((did, saved))
    }

    /// [`keep`] of `steps`, the claimed runs of `krate`'s bodies.
    #[expect(clippy::too_many_arguments, reason = "one crate's part of `keep`, which lends it each")]
    fn keep_crate<P: Put + ?Sized>(
        kernel: &Kernel<Bodies>,
        front: &Arc<Lowered>,
        krate: Crate,
        steps: &[(DefWithBodyId, Step, SchemaHash)],
        in_view: &impl Fn(&str) -> Option<String>,
        sessions: &Sessions,
        store: &mut P,
        did: &mut Did,
        saved: &mut Vec<(String, blaze_cas::Key)>,
    ) -> Result<(), FrontError> {
        let db = front.db();
        let (_, now) = db.nonce_and_revision();
        let mut lent = sessions.take(krate);
        let (last, held) = match lent.kept.take() {
            // Served at this revision: its files are the world's, and its session is of this pin.
            Some(Kept { session, files, posted, since }) if session.pin() == pin_of(&files, now) => {
                (Some(files), Some((session, posted, since)))
            }
            // Of another pin: left for the crate's next serve, as it was.
            Some(kept) => {
                lent.put(kept);
                (None, None)
            }
            None => (None, None),
        };
        // clone: a counted reference, the world's
        let mut world = Bodies::of(Arc::clone(front), &[krate], in_view, last);
        let (mut session, kept) = match held {
            Some((session, posted, since)) => (session, Some((posted, since))),
            None => (Session::new(&world), None),
        };
        world.map_got(kernel, &mut session, store)?;
        did.bodies += world.by_def.len();
        blaze_query::db::attach(db, || -> Result<(), FrontError> {
            // alloc: once a crate a keep, what it keeps in key order (rule 25)
            let mut todo: Vec<(&BodyKey, Of)> = Vec::new();
            for &(def, step, schema) in steps {
                let Some(key) = world.by_def.get(&def) else {
                    world.passed_over(def, did);
                    continue;
                };
                let of = match step {
                    Step::Infer => Of::Infer,
                    Step::Mir => match entries::place_of(db, def, schema) {
                        Some(place) => Of::Mir(place),
                        None => {
                            did.unkeyed += 1;
                            continue;
                        }
                    },
                    Step::Borrowck => Of::Borrowck,
                    // A constant's value is the item store's record: claimed, counted, not kept.
                    Step::Const => {
                        did.missed += 1;
                        continue;
                    }
                };
                todo.push((key, of));
            }
            todo.sort_unstable_by(|a, b| {
                (&a.0.file.name, a.0.item.0, rank(a.1)).cmp(&(&b.0.file.name, b.0.item.0, rank(b.1)))
            });
            for (key, of) in todo {
                let got = match of {
                    Of::Infer => kernel.get_in::<InferEntry<Bodies>, P>(&mut session, store, &world, key, &Setting::OFF).map(drop),
                    Of::Mir(place) => {
                        // clone: a body's key, once an owner kept
                        let owner = OwnerKey { body: key.clone(), place };
                        kernel.get_in::<MirEntry<Bodies>, P>(&mut session, store, &world, &owner, &Setting::OFF).map(drop)
                    }
                    Of::Borrowck => kernel.get_in::<BorrowckEntry<Bodies>, P>(&mut session, store, &world, key, &Setting::OFF).map(drop),
                };
                match got {
                    Ok(()) => did.kept += 1,
                    Err(error) => match refusal(&error) {
                        Some(why) => {
                            did.refused += 1;
                            tracing::debug!(target: "blaze", "{of:?} of {key:?} kept as no entry, its step refused: {why}");
                        }
                        None => return Err(error.into()),
                    },
                }
            }
            Ok(())
        })?;
        if let Some((posted, since)) = kept {
            let root = session.save(store)?;
            let name = world.crates.first().map(|(_, name)| name.to_string());
            lent.put(Kept { session, files: world.into_files(), posted, since });
            if let Some(name) = name {
                saved.push((name, root));
            }
        }
        Ok(())
    }
}

/// What a simulated daemon counts of the carry (rule 64): the world's reads, apart by whether
/// the carry's task ([`in_task`]) made them, and each pass's [`Built`] as the loop took it.
#[cfg(test)]
pub(crate) mod sim {
    use std::cell::{Cell, RefCell};

    use super::Built;

    thread_local! {
        static IN_TASK: Cell<bool> = const { Cell::new(false) };
        static LOOP_READS: Cell<usize> = const { Cell::new(0) };
        static TASK_READS: Cell<usize> = const { Cell::new(0) };
        static BUILT: RefCell<Vec<Built>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn read() {
        match IN_TASK.get() {
            true => TASK_READS.set(TASK_READS.get() + 1),
            false => LOOP_READS.set(LOOP_READS.get() + 1),
        }
    }

    /// Runs `f` as the carry's task: its reads count as the task's.
    pub(crate) fn in_task<R>(f: impl FnOnce() -> R) -> R {
        let before = IN_TASK.replace(true);
        let out = f();
        IN_TASK.set(before);
        out
    }

    /// The world's reads on this thread so far: outside the carry's task, and in it.
    pub(crate) fn reads() -> (usize, usize) {
        (LOOP_READS.get(), TASK_READS.get())
    }

    pub(crate) fn built(built: Built) {
        BUILT.with_borrow_mut(|it| it.push(built));
    }

    /// Every pass's [`Built`] the loop took on this thread, taken.
    pub(crate) fn take_built() -> Vec<Built> {
        BUILT.with_borrow_mut(std::mem::take)
    }
}

#[cfg(test)]
mod tests;
