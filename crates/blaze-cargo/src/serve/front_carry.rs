//! The daemon's front-end carry driver (docs/v2/plan/design/end-state.md, "The front end as
//! kernel steps", the order's row 3): one [`blaze_kernel::Session`] kept across the builds of
//! a checkout, holding blaze-front-items' steps (`parse`, `items`, `lines`, `modules`) over a
//! world of the checkout's pinned tree ([`Tree`]). Each build pins the view's root; the
//! difference from the last pin is the VFS's Merkle diff ([`Vfs::diff`], which descends only
//! where directory ids differ), so the change set is exact and costs `O(changed paths × depth)`,
//! never a reread of the tree. [`Carry::build`] turns it into the carry's [`Changed`], calls
//! [`Kernel::carry`], and asks the steps' roots again ([`Carry::build`]'s doc says which).
//!
//! A sans-IO core (rule 14): it reads the VFS's in-memory tree and the store it is handed,
//! holds no clock and opens no file. The job loop's build ([`super::job`]) calls it once a build
//! and times it into the span "front end carry".
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
//! Said plainly, what is not covered yet: a crate's cfg is the one it was lowered with before
//! its build script ran (`blaze_lower::Crates::cfg`): a `cargo:rustc-cfg` that gates a `mod`
//! is not seen. Files are read with no syntax extensions (`Extensions::NONE`): a crate whose
//! root turns blaze's syntax extensions on shows their syntax as parse errors in its values.
//! A symlink in the tree is refused when a step reads it, naming it. Crates outside the
//! checkout (registry dependencies, the standard library) are not in this world: they are
//! other roots and never change under a checkout's build. The session is not yet saved into
//! the daemon's image between restarts (the round trip is [`Carry::save`] / [`Carry::open`],
//! tested): a restarted daemon seeds a new session on its first build.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use blaze_cas::Key as StoreKey;
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
use blaze_vfs::{Bytes, ChangeKind, Mode, View, Vfs, ViewId};
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
/// in the pinned tree, its edition and its cfg.
#[derive(Clone, Debug)]
pub(crate) struct CrateRoot {
    /// What the crate is (its package, target and kinds) and the digest of its cfg: equal
    /// names are equal cfgs, so a read of a crate's cfg never differs between pins.
    pub(crate) name: String,
    pub(crate) root: String,
    pub(crate) edition: Edition,
    pub(crate) cfg: CfgOptions,
}

impl CrateRoot {
    /// The crate `what` (`package target kinds`), rooted at `root` (a path in the pinned
    /// tree), named by `what` and its cfg's digest.
    pub(crate) fn new(what: &str, root: String, edition: Edition, cfg: CfgOptions) -> CrateRoot {
        // alloc: once a crate a plan, its atoms in order (rule 25)
        let mut atoms: Vec<String> = cfg.atoms().map(ToString::to_string).collect();
        atoms.sort_unstable();
        let mut hasher = Hasher::new(CFG_NAME_DOMAIN);
        for atom in &atoms {
            hasher.part(atom.as_bytes());
        }
        let digest = format!("{:x}", hasher.finish()); // alloc: once a crate a plan
        // alloc: the crate's stable name, once a crate a plan
        let name = format!("{what} cfg:{}", &digest[..32.min(digest.len())]);
        CrateRoot { name, root, edition, cfg }
    }

    fn key(&self) -> CrateKey {
        // alloc: the step's input, once a crate a build
        let root = FileKey { name: self.root.clone(), reading: reading(self.edition) };
        CrateKey { krate: self.name.clone(), root } // clone: the step's input
    }
}

fn reading(edition: Edition) -> Reading {
    Reading { edition, extensions: Extensions::NONE }
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
    fn load(vfs: &Vfs, root: ContentId) -> Result<Tree, FrontError> {
        let reader = vfs.reader();
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
    fn moved(&mut self, vfs: &Vfs, root: ContentId, changes: &[blaze_vfs::Change]) -> Result<(), FrontError> {
        let reader = vfs.reader();
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

/// One checkout's session and what it was asked: kept by the job loop across builds.
pub(crate) struct Carry {
    kernel: Kernel<Tree>,
    session: Session,
    tree: Tree,
    /// The view the session's pin is of, held open in the VFS so the next pin diffs against
    /// it.
    held: ViewId,
    /// Each crate's `modules` value's hash as last asked: an equal one asks none of its
    /// files again.
    modules: BTreeMap<String, ContentId>,
    /// The files whose `items` and `lines` were asked, by name and reading.
    asked: BTreeSet<Asked>,
}

impl Carry {
    /// A session of `root`, every crate's roots asked: the first build of a checkout.
    pub(crate) fn seed<P: Put + ?Sized>(
        vfs: &mut Vfs,
        store: &mut P,
        root: ContentId,
        crates: &[CrateRoot],
    ) -> Result<(Carry, Built), FrontError> {
        let held = open(vfs, root)?;
        let tree = Tree::load(vfs, root)?;
        let session = Session::new(&tree);
        let mut carry =
            Carry { kernel: kernel()?, session, tree, held, modules: BTreeMap::new(), asked: BTreeSet::new() };
        let crates = carry.know(crates)?;
        carry.ask(store, &crates)?;
        let built = Built { first: true, crates: crates.len(), files: carry.asked.len(), ..Built::default() };
        Ok((carry, built))
    }

    /// The session saved under `saved` ([`Carry::save`]), pinned at its root, which `vfs`
    /// must still have. The crates' cfgs are known again from `crates`; every root is asked
    /// again at the next build (hits, but for what the carry reaches).
    pub(crate) fn open<G: Get + ?Sized>(
        vfs: &mut Vfs,
        store: &G,
        saved: Saved,
        crates: &[CrateRoot],
    ) -> Result<Carry, FrontError> {
        let session = Session::open(store, saved.session)?;
        let tree = Tree::load(vfs, saved.root)?;
        if session.pin() != tree.pin() {
            // alloc: an error
            return Err(FrontError::Tree(format!("the saved session is of another pin than the root {:x} it was saved with", saved.root)));
        }
        let held = open(vfs, saved.root)?;
        let mut carry =
            Carry { kernel: kernel()?, session, tree, held, modules: BTreeMap::new(), asked: BTreeSet::new() };
        carry.know(crates)?;
        Ok(carry)
    }

    /// Writes the session into `store` ([`Session::save`]): what [`Carry::open`] takes.
    pub(crate) fn save<P: Put + ?Sized>(&mut self, store: &mut P) -> Result<Saved, FrontError> {
        Ok(Saved { session: self.session.save(store)?, root: self.tree.root })
    }

    /// The build of `root` with `crates`: the session carried from its pin to `root`'s, then
    /// the roots asked.
    ///
    /// The change set: each path the VFS's diff names differs as a `FILE` read, and as an
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
        vfs: &mut Vfs,
        store: &mut P,
        root: ContentId,
        crates: &[CrateRoot],
    ) -> Result<Built, FrontError> {
        let crates = self.know(crates)?;
        let mut built = Built { crates: crates.len(), ..Built::default() };
        // alloc: one build's edited files, asked again after the carry
        let mut edited: Vec<Asked> = Vec::new();
        if root != self.tree.root {
            let held = open(vfs, root)?;
            let changes = vfs.diff(self.tree.root, root);
            let from = self.tree.pin();
            self.tree.moved(vfs, root, &changes)?;
            vfs.close(std::mem::replace(&mut self.held, held));
            let mut changed = Changed::between(from, self.tree.pin());
            changed.exact(reads::FILE);
            changed.exact(reads::EXISTS);
            changed.exact(reads::CFG);
            for change in &changes {
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

    /// Lets go of the view the session's pin holds open.
    pub(crate) fn close(self, vfs: &mut Vfs) {
        vfs.close(self.held);
    }
}

fn open(vfs: &mut Vfs, root: ContentId) -> Result<ViewId, FrontError> {
    // alloc: an error
    vfs.open(View { root }).map_err(|error| FrontError::Tree(format!("the root {root:x} cannot be held open: {error:?}")))
}

#[cfg(test)]
mod tests;
