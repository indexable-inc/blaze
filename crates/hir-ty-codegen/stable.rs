//! Names for instances that are the same in every run of the daemon on the same sources, so
//! that code compiled by one run can be found by the next. A crate of the checkout is named
//! by what is written in it, so an edit makes other names of everything in it.

use std::{
    cell::RefCell,
    fmt::Write,
    sync::{Arc, Mutex, OnceLock},
};

use hir_def::{
    AdtId, AssocItemId, CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup, ModuleDefId,
    StaticId, TraitId, expr_store::Body, item_scope::ItemScope, nameres::crate_def_map,
};
use hir_expand::{HirFileId, MacroCallKind};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    AliasTy, AliasTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, RegionKind,
    inherent::{IntoKind, Safety as _, Term as _},
};
use salsa::plumbing::FromId;
use span::ErasedFileAstId;

use base_db::Crate;

use super::{Instance, InstanceDef, SymbolResolver};
use hir_ty::{
    ImplTraitId, InferBodyId,
    consteval::try_const_usize,
    db::{ClosureKey, HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{GenericArgs, Region, TermId, Ty, TyKind},
};

/// Before the content of a crate of the checkout, where a crate of rustc's alone has its id.
const OF_CHECKOUT: char = '~';

/// After the name of an instance compiled with `-C overflow-checks`.
const CHECKED: &str = " checked";

/// Before what stands for a crate from outside the checkout that rustc has no id for.
const OF_LIBRARY: char = '^';

/// Before the hash that stands for a type written longer than [`SPELLED`]: a closure that
/// wraps closures has a type that holds each of theirs, and written out in full its name
/// doubles with each level.
const HASHED: char = '$';

/// The longest a type is written out in a name.
const SPELLED: usize = 1024;

enum Declared {
    Function(FunctionId),
    Static(StaticId),
}

/// Lowers what [`StableNames`] lists of `crates`, so that a build that lists them later
/// finds it done. `lower` is given the bodies of each round to lower at once.
pub fn lower_declared(db: &dyn HirDatabase, crates: &[Crate], lower: &dyn Fn(&[DefWithBodyId])) {
    declared(db, crates, lower, &mut |_| ());
}

/// The statics `crates` declare, in their modules and in the blocks of their bodies.
pub(super) fn declared_statics(db: &dyn HirDatabase, crates: &[Crate]) -> Vec<StaticId> {
    let mut statics = Vec::new();
    declared(db, crates, &|_| (), &mut |it| {
        if let Declared::Static(id) = it {
            statics.push(id);
        }
    });
    statics
}

/// The functions and statics `crates` declare, in their modules and in the blocks of their
/// bodies: a `static` is often written by a macro.
fn declared(
    db: &dyn HirDatabase,
    crates: &[Crate],
    lower: &dyn Fn(&[DefWithBodyId]),
    found: &mut dyn FnMut(Declared),
) {
    let mut bodies: Vec<DefWithBodyId> = Vec::new();
    let mut scope = |scope: &ItemScope, bodies: &mut Vec<DefWithBodyId>| {
        for def in scope.declarations() {
            match def {
                ModuleDefId::StaticId(id) => {
                    found(Declared::Static(id));
                    bodies.push(id.into());
                }
                ModuleDefId::FunctionId(id) => {
                    found(Declared::Function(id));
                    bodies.push(id.into());
                }
                ModuleDefId::ConstId(id) => bodies.push(id.into()),
                // A method a trait provides has blocks of its own.
                ModuleDefId::TraitId(id) => {
                    for &(_, item) in id.trait_items(db).items.iter() {
                        match item {
                            AssocItemId::FunctionId(id) => bodies.push(id.into()),
                            AssocItemId::ConstId(id) => bodies.push(id.into()),
                            AssocItemId::TypeAliasId(_) => {}
                        }
                    }
                }
                _ => {}
            }
        }
        bodies.extend(scope.unnamed_consts().map(DefWithBodyId::from));
        for imp in scope.impls() {
            for &(_, item) in imp.impl_items(db).items.iter() {
                match item {
                    AssocItemId::FunctionId(id) => {
                        found(Declared::Function(id));
                        bodies.push(id.into());
                    }
                    AssocItemId::ConstId(id) => bodies.push(id.into()),
                    AssocItemId::TypeAliasId(_) => {}
                }
            }
        }
    };
    for &krate in crates {
        for (_, module) in crate_def_map(db, krate).modules(db) {
            scope(module.scope(db), &mut bodies);
        }
    }
    while !bodies.is_empty() {
        let round = std::mem::take(&mut bodies);
        lower(&round);
        for body in round {
            for (_, def_map) in Body::of(db, body).blocks(db) {
                for (_, module) in def_map.modules(db) {
                    scope(module.scope(db), &mut bodies);
                }
            }
        }
    }
}

/// The crates by what a name starts with, and what the ones asked about declare, for every
/// [`StableNames`] of a build: to list a crate is to go through every body of it.
#[derive(Default, Clone)]
pub(super) struct Listings(Arc<Listing>);

/// Each crate is listed once, by the first to ask, while the threads that ask of the others
/// go on: the whole build's threads ask at once when it gives the store its entries.
#[derive(Default)]
struct Listing {
    crates: OnceLock<FxHashMap<String, Vec<Crate>>>,
    listed: Mutex<FxHashMap<String, ListedCell>>,
}

/// What a crate declares, or `None` for a name of no crate the build depends on.
type ListedCell = Arc<OnceLock<Option<Listed>>>;

/// The statics and the functions without type parameters that one build of a crate declares.
#[derive(Default)]
struct Listed {
    statics: FxHashMap<String, StaticId>,
    functions: FxHashMap<String, Vec<FunctionId>>,
}

pub(super) struct StableNames<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    /// The crates whose code changes under the daemon.
    owned: FxHashSet<Crate>,
    /// Whether what is of those crates has a name, by what is written in them now.
    by_content: bool,
    /// Where cargo keeps the sources it fetched on this machine.
    cargo_home: Option<String>,
    library: Option<super::LibrarySources>,
    files: RefCell<FxHashMap<HirFileId, Option<String>>>,
    /// The crates by what a name starts with, and what the ones asked about declare.
    listings: Listings,
    /// Those of `listings` this has asked about, which it asks about again without a lock.
    listed: RefCell<FxHashMap<Box<str>, ListedCell>>,
    /// How each crate and each type is written, or why it has no name.
    crate_names: RefCell<FxHashMap<Crate, Result<Box<str>, &'static str>>>,
    tys: RefCell<FxHashMap<Ty<'db>, Result<Box<str>, &'static str>>>,
    /// What each closure asked about is written as, by a hash of its source.
    written: RefCell<FxHashMap<InternedClosure<'db>, Option<Box<str>>>>,
    /// The ids crates of the checkout are named by in place of what is written in them now:
    /// those of a base, whose names an edit leaves to the code they are of.
    frozen: FxHashMap<Crate, super::ContentId>,
    /// Why the last name that could not be written could not.
    missed: std::cell::Cell<&'static str>,
    /// How often each thing kept an instance from having a name.
    pub(super) misses: RefCell<FxHashMap<&'static str, usize>>,
}

impl<'a, 'db> StableNames<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        owned: FxHashSet<Crate>,
        by_content: bool,
    ) -> Self {
        StableNames {
            db,
            symbols,
            owned,
            by_content,
            cargo_home: symbols.cargo_home(),
            library: symbols.library_sources(),
            files: RefCell::default(),
            listings: Listings::default(),
            listed: RefCell::default(),
            crate_names: RefCell::default(),
            tys: RefCell::default(),
            written: RefCell::default(),
            frozen: FxHashMap::default(),
            missed: std::cell::Cell::new(""),
            misses: RefCell::default(),
        }
    }

    /// Lists the crates with `listings`, which other names of the build share.
    pub(super) fn listing(mut self, listings: Listings) -> Self {
        self.listings = listings;
        self.listed.get_mut().clear();
        self
    }

    pub(super) fn listings(&self) -> Listings {
        self.listings.clone()
    }

    /// Names each crate of `frozen` by its id there, whatever is written in it now.
    pub(super) fn frozen(mut self, frozen: FxHashMap<Crate, super::ContentId>) -> Self {
        self.frozen = frozen;
        self
    }

    /// The id a crate of the checkout is named by.
    pub(super) fn content_id(&self, krate: Crate) -> Option<super::ContentId> {
        self.frozen.get(&krate).copied().or_else(|| self.symbols.content_id(krate))
    }

    /// The name of a file as every machine has it, as panic locations name it in an entry.
    pub(super) fn file_path(&self, file: base_db::FileId) -> Option<String> {
        self.stored(self.symbols.source_path(file)?)
    }

    /// Whether a name is of anything of a crate of the checkout, which is only what the
    /// name says while the checkout is as it was: rustc's build of it can be of another time.
    pub(super) fn of_checkout(name: &str) -> bool {
        name.contains(OF_CHECKOUT)
    }

    /// `None` for an instance that names anything this has no stable name for.
    pub(super) fn instance(&self, instance: &Instance) -> Option<String> {
        let mut out = String::new();
        match instance.def {
            InstanceDef::Fn(func) => {
                out.push_str("fn ");
                self.function(&mut out, func)?;
            }
            InstanceDef::Closure(id) => {
                out.push_str("closure ");
                self.closure(&mut out, InternedClosureId::from_id(id).loc(self.db))?;
            }
            InstanceDef::Coroutine(id) => {
                out.push_str("coroutine ");
                self.closure(&mut out, InternedCoroutineId::from_id(id).loc(self.db))?;
            }
            InstanceDef::CoroutineByMove(id) => {
                out.push_str("coroutine by move ");
                self.closure(&mut out, InternedCoroutineId::from_id(id).loc(self.db))?;
            }
            InstanceDef::DropGlue => out.push_str("drop"),
            InstanceDef::CallOnceShim => out.push_str("call_once"),
            InstanceDef::VtableShim(func) => {
                out.push_str("vtable shim ");
                self.function(&mut out, func)?;
            }
            InstanceDef::FnPointerShim => out.push_str("fn_pointer"),
            InstanceDef::ReifyShim { receiver } => write!(out, "reify {receiver}").ok()?,
        }
        self.args(&mut out, instance.args.as_ref())?;
        if instance.checked {
            out.push_str(CHECKED);
        }
        Some(out)
    }

    /// The vtable of a type for a trait object, or the one with no methods.
    pub(super) fn vtable(
        &self,
        ty: Ty<'db>,
        trait_ref: Option<(TraitId, GenericArgs<'db>)>,
    ) -> Option<String> {
        let mut out = String::from("vtable ");
        self.ty(&mut out, ty)?;
        if let Some((trait_, args)) = trait_ref {
            out.push_str(" as ");
            self.trait_(&mut out, trait_)?;
            self.args(&mut out, args)?;
        }
        Some(out)
    }

    /// A static that [`Self::static_named`] finds again.
    pub(super) fn static_(&self, id: StaticId) -> Option<String> {
        let name = self.static_name(id)?;
        self.static_named(&name).is_some().then_some(name)
    }

    pub(super) fn static_named(&self, name: &str) -> Option<StaticId> {
        self.listed(name, |it| it.statics.get(name).copied())
    }

    /// `name` with the arguments of an instance of lifetimes alone erased, as its function
    /// is named as a root: it is one function whatever they are.
    pub(super) fn lifetimes_erased(name: &str) -> std::borrow::Cow<'_, str> {
        let (bare, checked) = match name.strip_suffix(CHECKED) {
            Some(bare) => (bare, CHECKED),
            None => (name, ""),
        };
        let erased = bare.rfind('<').filter(|&at| {
            let args = bare[at + 1..].strip_suffix('>');
            args.is_some_and(|it| it.split(',').all(|arg| arg.is_empty() || arg == "'_"))
        });
        match erased {
            Some(at) if &bare[at..] != "<>" => {
                std::borrow::Cow::Owned(format!("{}<>{checked}", &bare[..at]))
            }
            _ => std::borrow::Cow::Borrowed(name),
        }
    }

    /// The functions an instance without arguments can be of: one for each crate that the
    /// one build of its crate is here.
    pub(super) fn functions_named(&self, name: &str) -> Vec<FunctionId> {
        let name = name.strip_suffix(CHECKED).unwrap_or(name);
        // An instance of a library's function with a type of the checkout is asked about
        // too, and no function without type parameters is named so: its crate is not listed.
        let of_checkout = name.split_once(' ').is_some_and(|it| it.1.starts_with(OF_CHECKOUT));
        if !of_checkout {
            return Vec::new();
        }
        let name = Self::lifetimes_erased(name);
        // An instance with types for arguments is of no function a crate lists, and listing
        // its crate for it would go through every body there for nothing.
        if !name.ends_with("<>") {
            return Vec::new();
        }
        self.listed(&name, |it| it.functions.get(&*name).cloned()).unwrap_or_default()
    }

    /// What the crate that `name` is of declares: a name starts with the kind of the item
    /// and the crate it is written in.
    fn listed<T>(&self, name: &str, of: impl FnOnce(&Listed) -> Option<T>) -> Option<T> {
        let (_, rest) = name.split_once(' ')?;
        let krate = &rest[..=rest.find(':')?];
        let cell = self.listed.borrow().get(krate).cloned();
        let cell = cell.unwrap_or_else(|| {
            let mut listed = self.listings.0.listed.lock().unwrap_or_else(|e| e.into_inner());
            let cell = listed.entry(krate.to_owned()).or_default().clone();
            drop(listed);
            self.listed.borrow_mut().insert(krate.into(), cell.clone());
            cell
        });
        let listed = cell.get_or_init(|| {
            // One build of a crate can be several crates here, which differ in what the
            // daemon is told of them and not in what they declare.
            let crates = self.listings.0.crates.get_or_init(|| {
                let mut crates = FxHashMap::<String, Vec<Crate>>::default();
                // What is built names only what it depends on, and to name a crate of the
                // checkout is to go through every body of it: not of one that is not built.
                let named: FxHashSet<Crate> =
                    self.owned.iter().flat_map(|it| it.transitive_deps(self.db)).collect();
                let all = base_db::all_crates(self.db);
                for &krate in all.iter().filter(|it| named.contains(it)) {
                    let mut name = String::new();
                    if self.crate_(&mut name, krate).is_some() {
                        crates.entry(name).or_default().push(krate);
                    }
                }
                crates
            });
            crates.get(krate).map(|it| self.list(it))
        });
        of(listed.as_ref()?)
    }

    fn list(&self, crates: &[Crate]) -> Listed {
        let mut found = Listed::default();
        // Lowering a body is most of the listing, and the bodies do not wait on each other.
        let lower = |round: &[DefWithBodyId]| {
            self.symbols.in_parallel(round.len(), &|db, _, at| _ = Body::of(db, round[at]));
        };
        declared(self.db, crates, &lower, &mut |declared| match declared {
            Declared::Function(id) => {
                let mut name = String::from("fn ");
                if self.function(&mut name, id).is_some() {
                    name.push_str("<>");
                    found.functions.entry(name).or_default().push(id);
                }
            }
            Declared::Static(id) => {
                if let Some(name) = self.static_name(id) {
                    found.statics.entry(name).or_insert(id);
                }
            }
        });
        found
    }

    /// The name of a static whether or not [`Self::static_named`] finds it again.
    pub(super) fn static_name(&self, id: StaticId) -> Option<String> {
        let mut out = String::from("static ");
        let at = id.lookup(self.db).id;
        self.item(&mut out, id.krate(self.db), at.file_id, at.value.erase())?;
        Some(out)
    }

    /// A name of a static that stays the same across edits of the checkout while the
    /// daemon runs, for [`super::LASTING_STATIC`]: [`Self::static_name`] names a crate of the
    /// checkout by its content and a macro call by its place among the calls of its file,
    /// which an edit above it changes. Not for the store.
    pub(super) fn static_across_edits(&self, id: StaticId) -> Option<String> {
        let krate = id.krate(self.db);
        if !self.of_the_checkout(krate) {
            return self.static_name(id);
        }
        let at = id.lookup(self.db).id;
        let root = krate.data(self.db).root_file_id;
        let mut out = format!("static {OF_CHECKOUT}{root:?}:");
        file_across_edits(self.db, &mut out, at.file_id)?;
        write!(out, "#{}", at.value.erase().into_raw()).ok()?;
        Some(out)
    }

    /// The name of a file as every machine has it: one of the standard library is where it
    /// was built, and one that cargo fetched is under [`super::CARGO_HOME`] whoever fetched
    /// it. `None` for a file that is already named so and is not one of those.
    fn stored(&self, path: String) -> Option<String> {
        if let Some(built) = self.library.as_ref().and_then(|it| it.stored(&path)) {
            return Some(built);
        }
        let home = self.cargo_home.as_deref();
        if home == Some(super::CARGO_HOME) {
            return Some(path);
        }
        if super::moved(&path, super::CARGO_HOME, "").is_some() {
            return None;
        }
        let there = home.and_then(|home| super::moved(&path, home, super::CARGO_HOME));
        Some(there.unwrap_or(path))
    }

    fn miss<T>(&self, why: &'static str) -> Option<T> {
        *self.misses.borrow_mut().entry(why).or_default() += 1;
        self.missed.set(why);
        None
    }

    /// Writes what `spell` writes of `of`, once for each `of`: the second time a type is asked
    /// for, the first answer is copied, and its miss counted again.
    fn memo<K: std::hash::Hash + Eq + Copy>(
        &self,
        out: &mut String,
        memo: &RefCell<FxHashMap<K, Result<Box<str>, &'static str>>>,
        of: K,
        spell: impl FnOnce(&mut String) -> Option<()>,
    ) -> Option<()> {
        if let Some(known) = memo.borrow().get(&of) {
            return match known {
                Ok(it) => {
                    out.push_str(it);
                    Some(())
                }
                Err(why) => self.miss(why),
            };
        }
        let mut spelled = String::new();
        let found = match spell(&mut spelled) {
            Some(()) => Ok(spelled.into_boxed_str()),
            None => Err(self.missed.get()),
        };
        if let Ok(it) = &found {
            out.push_str(it);
        }
        let named = found.as_ref().ok().map(|_| ());
        memo.borrow_mut().insert(of, found);
        named
    }

    /// An item by where it is written: names do not tell two `impl`s apart.
    fn item(
        &self,
        out: &mut String,
        krate: Crate,
        file: HirFileId,
        id: ErasedFileAstId,
    ) -> Option<()> {
        self.crate_(out, krate)?;
        self.ast(out, file, id)
    }

    fn crate_(&self, out: &mut String, krate: Crate) -> Option<()> {
        self.memo(out, &self.crate_names, krate, |out| self.spell_crate(out, krate))
    }

    /// Whether the crate is named by its content: its source changes under the daemon.
    pub(super) fn of_the_checkout(&self, krate: Crate) -> bool {
        self.owned.contains(&krate) || krate.data(self.db).origin.is_local()
    }

    fn spell_crate(&self, out: &mut String, krate: Crate) -> Option<()> {
        // The source of a crate of the workspace changes too, built by the daemon or not.
        if self.of_the_checkout(krate) {
            if !self.by_content {
                return self.miss("of the checkout, in a patch");
            }
            let Some(id) = self.content_id(krate) else {
                return self.miss("no content id");
            };
            write!(out, "{OF_CHECKOUT}{id}:").ok()?;
        } else {
            match self.symbols.stable_crate_id(krate) {
                Some(id) => write!(out, "{id:x}:").ok()?,
                None => match self.symbols.content_id(krate) {
                    Some(id) => write!(out, "{OF_LIBRARY}{id}:").ok()?,
                    None => return self.miss("no crate id"),
                },
            }
        }
        Some(())
    }

    fn ast(&self, out: &mut String, file: HirFileId, id: ErasedFileAstId) -> Option<()> {
        if let Some(known) = self.files.borrow().get(&file) {
            out.push_str(known.as_deref()?);
        } else {
            let mut name = String::new();
            let named = self.file(&mut name, file);
            let name = named.map(|()| name);
            out.push_str(name.as_deref().unwrap_or_default());
            self.files.borrow_mut().insert(file, name);
            named?;
        }
        write!(out, "#{}", id.into_raw()).ok()
    }

    fn file(&self, out: &mut String, file: HirFileId) -> Option<()> {
        let named = |path: Option<String>| match path {
            Some(path) => self
                .stored(path)
                .or_else(|| self.miss("a file under what stands for cargo's directory")),
            None => self.miss("no source path"),
        };
        file_name(self.db, self.symbols, out, file, &named)
    }

    fn function(&self, out: &mut String, func: FunctionId) -> Option<()> {
        let id = func.lookup(self.db).id;
        self.item(out, func.krate(self.db), id.file_id, id.value.erase())
    }

    fn adt(&self, out: &mut String, adt: AdtId) -> Option<()> {
        let db = self.db;
        let (file, id) = match adt {
            AdtId::StructId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
            AdtId::UnionId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
            AdtId::EnumId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
        };
        self.item(out, adt.krate(db), file, id)
    }

    fn trait_(&self, out: &mut String, trait_: TraitId) -> Option<()> {
        let id = trait_.lookup(self.db).id;
        self.item(out, trait_.krate(self.db), id.file_id, id.value.erase())
    }

    /// A closure or an `async` block by the body it is written in, its place among the
    /// closures there, as it is interned, and what is written of it: an edit of the body
    /// before it leaves its name, and the names of what is generic over it, as they were.
    /// What calls a closure does not say what it takes, so one that takes the place of another
    /// has to be named apart from it. One the body has no source for, as the coroutine of an
    /// `async fn`, is named by the place of its expression in the body.
    fn closure(&self, out: &mut String, closure: InternedClosure<'db>) -> Option<()> {
        let db = self.db;
        let InferBodyId::DefWithBodyId(owner) = closure.owner else {
            return self.miss("closure in an anonymous constant");
        };
        match owner {
            DefWithBodyId::FunctionId(it) => self.function(out, it)?,
            DefWithBodyId::ConstId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
            DefWithBodyId::StaticId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
            DefWithBodyId::VariantId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
        }
        match self.written(owner, closure) {
            Some(written) => write!(out, "@{}:{written}", ClosureKey::nth(db, closure)).ok(),
            None => write!(out, "@x{}", closure.expr.into_raw().into_u32()).ok(),
        }
    }

    /// A hash of the source of a closure, `None` when the body has none for it.
    fn written(&self, owner: DefWithBodyId, closure: InternedClosure<'db>) -> Option<Box<str>> {
        if let Some(written) = self.written.borrow().get(&closure) {
            return written.clone();
        }
        let source = Body::with_source_map(self.db, owner).1.expr_syntax(closure.expr).ok();
        let written = source.map(|source| {
            use sha2::{Digest, Sha256};
            let root = source.file_id.parse_or_expand(self.db);
            let text = source.value.syntax_node_ptr().to_node(&root).text().to_string();
            let digest = Sha256::digest(text.as_bytes());
            digest[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>().into()
        });
        self.written.borrow_mut().insert(closure, written.clone());
        written
    }

    fn args(&self, out: &mut String, args: GenericArgs<'db>) -> Option<()> {
        out.push('<');
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(region) => self.region(out, region)?,
                GenericArgKind::Type(ty) => self.ty(out, ty)?,
                GenericArgKind::Const(konst) => {
                    if let Some(value) = try_const_usize(self.db, konst) {
                        write!(out, "{value}").ok()?
                    } else if let Some(value) = konst.try_to_value()
                        && let Some(leaf) = value.try_to_leaf()
                    {
                        self.ty(out, value.ty)?;
                        write!(out, ":{leaf:?}").ok()?
                    } else {
                        return self.miss("constant argument");
                    }
                }
            }
            out.push(',');
        }
        out.push('>');
        Some(())
    }

    /// Erased, unless a binder in the type binds it.
    fn region(&self, out: &mut String, region: Region<'db>) -> Option<()> {
        match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(depth), bound) => {
                write!(out, "'{}.{}", depth.as_u32(), bound.var.as_u32()).ok()
            }
            RegionKind::ReBound(..) => self.miss("bound region"),
            _ => {
                out.push_str("'_");
                Some(())
            }
        }
    }

    fn tys(&self, out: &mut String, tys: impl IntoIterator<Item = Ty<'db>>) -> Option<()> {
        for ty in tys {
            self.ty(out, ty)?;
            out.push(',');
        }
        Some(())
    }

    /// A type written out, or the hash of that when it is longer than [`SPELLED`]: what holds
    /// a type of the checkout says so in its hash too, for [`Self::of_checkout`].
    fn ty(&self, out: &mut String, ty: Ty<'db>) -> Option<()> {
        self.memo(out, &self.tys, ty, |out| {
            self.spell_ty(out, ty)?;
            if out.len() > SPELLED {
                *out = hashed(out);
            }
            Some(())
        })
    }

    fn spell_ty(&self, out: &mut String, ty: Ty<'db>) -> Option<()> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => out.push_str("bool"),
            TyKind::Char => out.push_str("char"),
            TyKind::Str => out.push_str("str"),
            TyKind::Never => out.push('!'),
            TyKind::Int(it) => out.push_str(it.name_str()),
            TyKind::Uint(it) => out.push_str(it.name_str()),
            TyKind::Float(it) => out.push_str(it.name_str()),
            TyKind::Ref(region, pointee, mutability) => {
                out.push('&');
                self.region(out, region)?;
                out.push_str(match mutability {
                    Mutability::Not => " ",
                    Mutability::Mut => " mut ",
                });
                self.ty(out, pointee)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                out.push_str(match mutability {
                    Mutability::Not => "*const ",
                    Mutability::Mut => "*mut ",
                });
                self.ty(out, pointee)?;
            }
            TyKind::Slice(element) => {
                out.push('[');
                self.ty(out, element)?;
                out.push(']');
            }
            TyKind::Array(element, len) => {
                out.push('[');
                self.ty(out, element)?;
                let Some(len) = try_const_usize(db, len) else {
                    return self.miss("array length");
                };
                write!(out, ";{len}]").ok()?;
            }
            TyKind::Tuple(tys) => {
                out.push('(');
                self.tys(out, tys.iter())?;
                out.push(')');
            }
            TyKind::Adt(adt, args) => {
                out.push_str("adt ");
                self.adt(out, adt.def_id())?;
                self.args(out, args)?;
            }
            TyKind::FnDef(def, args) => {
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        out.push_str("fn ");
                        self.function(out, it)?;
                    }
                    CallableDefId::StructId(it) => {
                        out.push_str("ctor ");
                        self.adt(out, it.into())?;
                    }
                    CallableDefId::EnumVariantId(it) => {
                        out.push_str("ctor ");
                        let id = it.lookup(db).id;
                        self.item(out, it.krate(db), id.file_id, id.value.erase())?;
                    }
                }
                self.args(out, args)?;
            }
            TyKind::FnPtr(signature, header) => {
                write!(
                    out,
                    "fnptr {:?} {} {} (",
                    header.abi(),
                    header.safety().is_safe(),
                    header.c_variadic()
                )
                .ok()?;
                self.tys(out, signature.skip_binder().inputs_and_output.iter())?;
                out.push(')');
            }
            TyKind::Closure(id, args) => {
                out.push_str("closure ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                out.push_str("async closure ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::Coroutine(id, args) => {
                out.push_str("coroutine ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::Dynamic(predicates, region) => {
                // In an order of their own: the list is sorted by ids of this run.
                let mut written = Vec::new();
                for predicate in predicates.iter() {
                    let mut out = String::new();
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            out.push_str("trait ");
                            self.trait_(&mut out, it.def_id.0)?;
                            self.args(&mut out, it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            let TermId::TypeAliasId(alias) = it.def_id.0 else {
                                return self.miss("projection");
                            };
                            out.push_str("type ");
                            let id = alias.lookup(db).id;
                            self.item(&mut out, alias.krate(db), id.file_id, id.value.erase())?;
                            self.args(&mut out, it.args)?;
                            out.push('=');
                            self.ty(&mut out, it.term.as_type()?)?;
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            out.push_str("auto ");
                            self.trait_(&mut out, it.0)?;
                        }
                    }
                    written.push(out);
                }
                written.sort();
                out.push_str("dyn (");
                for it in written {
                    out.push_str(&it);
                    out.push('+');
                }
                self.region(out, region)?;
                out.push(')');
            }
            // What was left of a projection that no `impl` in reach gives a type to.
            TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { def_id }, args, .. }) => {
                let at = def_id.0.lookup(db).id;
                out.push_str("alias ");
                self.item(out, def_id.0.krate(db), at.file_id, at.value.erase())?;
                self.args(out, args)?;
            }
            // An `impl Trait` nothing has told the type of, as in the drop glue of what holds one.
            TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => {
                let (at, index) = match def_id.0.loc(db) {
                    ImplTraitId::ReturnTypeImplTrait(of, index) => {
                        out.push_str("opaque fn ");
                        self.function(out, of)?;
                        (None, index)
                    }
                    ImplTraitId::TypeAliasImplTrait(of, index) => {
                        out.push_str("opaque alias ");
                        (Some(of), index)
                    }
                };
                if let Some(of) = at {
                    let at = of.lookup(db).id;
                    self.item(out, of.krate(db), at.file_id, at.value.erase())?;
                }
                write!(out, " {}", index.into_raw().into_u32()).ok()?;
                self.args(out, args)?;
            }
            TyKind::Alias(..) => return self.miss("alias type"),
            TyKind::Param(..) => return self.miss("type parameter"),
            TyKind::CoroutineWitness(..) => return self.miss("coroutine witness"),
            TyKind::Foreign(def_id) => {
                let at = def_id.0.lookup(db).id;
                out.push_str("foreign ");
                self.item(out, def_id.0.krate(db), at.file_id, at.value.erase())?;
            }
            TyKind::Pat(..) => return self.miss("pattern type"),
            _ => return self.miss("other type"),
        }
        Some(())
    }
}

/// A file by its path as `named` has it, or for what a macro wrote by the call that wrote it.
fn file_name(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    out: &mut String,
    file: HirFileId,
    named: &dyn Fn(Option<String>) -> Option<String>,
) -> Option<()> {
    let written_at = |out: &mut String, file: HirFileId, id: ErasedFileAstId| {
        file_name(db, symbols, out, file, named)?;
        write!(out, "#{}", id.into_raw()).ok()
    };
    match file {
        HirFileId::FileId(file) => out.push_str(&named(symbols.source_path(file.file_id(db)))?),
        HirFileId::MacroFile(call) => match &call.loc(db).kind {
            MacroCallKind::FnLike { ast_id, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                out.push('!');
            }
            MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                write!(out, "!derive {derive_attr_index:?} {derive_index}").ok()?;
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                written_at(out, ast_id.file_id, ast_id.value.erase())?;
                write!(out, "!attr {censored_attr_ids:?}").ok()?;
            }
        },
    }
    Some(())
}

/// [`file_name`] for [`StableNames::static_across_edits`]: a file by its id, and a call of a
/// macro written as one (`name!(...)`) by its text, the same wherever the call moves in its
/// file. A derive's or an attribute's call is the item's, which its name stands for.
fn file_across_edits(db: &dyn HirDatabase, out: &mut String, file: HirFileId) -> Option<()> {
    match file {
        HirFileId::FileId(file) => write!(out, "{:?}", file.file_id(db)).ok(),
        HirFileId::MacroFile(call) => {
            let loc = call.loc(db);
            match &loc.kind {
                MacroCallKind::FnLike { ast_id, .. } => {
                    file_across_edits(db, out, ast_id.file_id)?;
                    let text = loc.to_node(db).value.text().to_string();
                    write!(out, "#{:032x}!", super::print_of(&text)).ok()
                }
                MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                    file_across_edits(db, out, ast_id.file_id)?;
                    let at = ast_id.value.erase().into_raw();
                    write!(out, "#{at}!derive {derive_attr_index:?} {derive_index}").ok()
                }
                MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                    file_across_edits(db, out, ast_id.file_id)?;
                    let at = ast_id.value.erase().into_raw();
                    write!(out, "#{at}!attr {censored_attr_ids:?}").ok()
                }
            }
        }
    }
}

fn hashed(spelled: &str) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(66);
    out.push(HASHED);
    if spelled.contains(OF_CHECKOUT) {
        out.push(OF_CHECKOUT);
    }
    for byte in Sha256::digest(spelled.as_bytes()) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 15)] as char);
    }
    out
}

/// Where the toolchain installs the standard library's sources, the directory `library/` is in.
const INSTALLED_LIBRARY: &str = "/lib/rustlib/src/rust/";

/// A file's path as a place names it ([`block_place`], [`static_place`]): the same on every
/// machine for a file of the standard library or of what cargo fetched. The standard library
/// from the store was built on another machine, and a program finds the statics of its bodies
/// by the name their place gives them: under a path of this machine's, std's `thread_local!`s
/// and `thread::min_stack`'s `MIN` were not found, and a program that spawned a thread or made
/// a `HashMap` was refused (#840).
fn place_path(symbols: &dyn SymbolResolver, path: Option<String>) -> Option<String> {
    let library = symbols.library_sources().map(|it| it.here);
    Some(path_everywhere(path?, symbols.cargo_home().as_deref(), library.as_deref()))
}

/// [`place_path`] with cargo's directory on this machine at `cargo_home`, and the standard
/// library's `library/` at `library`: `/rustc/library/...` names a file of it wherever it is,
/// in a toolchain or as blaze's own.
fn path_everywhere(path: String, cargo_home: Option<&str>, library: Option<&str>) -> String {
    if let Some(rest) = library.and_then(|it| path.strip_prefix(it))
        && let Some(rest) = rest.strip_prefix(['/', '\\'])
    {
        return format!("/rustc/library/{}", rest.replace('\\', "/"));
    }
    let slashed = path.replace('\\', "/");
    if let Some(at) = slashed.find(INSTALLED_LIBRARY) {
        return format!("/rustc/{}", &slashed[at + INSTALLED_LIBRARY.len()..]);
    }
    cargo_home.and_then(|home| super::moved(&path, home, super::CARGO_HOME)).unwrap_or(path)
}

/// Where a function inside a block is written, which its path does not say: two blocks of
/// one function, or two `const _` of one module, can each hold an item of the same name.
/// `None` for a function outside of any block.
pub fn block_place(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    func: FunctionId,
) -> Option<String> {
    func.module(db).block(db)?;
    let id = func.lookup(db).id;
    let mut out = String::new();
    file_name(db, symbols, &mut out, id.file_id, &|path| place_path(symbols, path))?;
    write!(out, "#{}", id.value.erase().into_raw()).ok()?;
    Some(out)
}

/// Where any function is written, block or not: for a function whose path blaze cannot write
/// (an `impl` for a type it has no name for, such as `!`, `dyn Trait` or a type taking a
/// constant), the place names it instead.
pub fn function_place(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    func: FunctionId,
) -> Option<String> {
    let id = func.lookup(db).id;
    let mut out = String::new();
    file_name(db, symbols, &mut out, id.file_id, &|path| place_path(symbols, path))?;
    write!(out, "#{}", id.value.erase().into_raw()).ok()?;
    Some(out)
}

/// Where a static inside a block is written, as [`block_place`] has it of a function.
pub fn static_place(
    db: &dyn HirDatabase,
    symbols: &dyn SymbolResolver,
    id: hir_def::StaticId,
) -> Option<String> {
    id.module(db).block(db)?;
    let id = id.lookup(db).id;
    let mut out = String::new();
    file_name(db, symbols, &mut out, id.file_id, &|path| place_path(symbols, path))?;
    write!(out, "#{}", id.value.erase().into_raw()).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::path_everywhere;

    #[test]
    fn a_file_of_the_standard_library_or_of_cargo_is_named_the_same_on_every_machine() {
        let std = "lib/rustlib/src/rust/library/std/src/thread/mod.rs";
        let runner = format!("/Users/runner/.rustup/toolchains/nightly-aarch64-apple-darwin/{std}");
        let user = format!("/home/me/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/{std}");
        let windows = format!(r"C:\Users\me\.rustup\toolchains\nightly\{}", std.replace('/', r"\"));
        let named = path_everywhere(runner, None, None);
        assert_eq!(named, "/rustc/library/std/src/thread/mod.rs");
        assert_eq!(path_everywhere(user, Some("/home/me/.cargo"), None), named);
        assert_eq!(path_everywhere(windows, None, None), named);
        // blaze's own library, from a checkout or downloaded, is named as a toolchain's is.
        let own = "/home/me/.blaze/library/b8b8/library/std/src/thread/mod.rs";
        let at = Some("/home/me/.blaze/library/b8b8/library");
        assert_eq!(path_everywhere(own.to_owned(), None, at), named);
        let checkout = "/src/blaze/library/std/src/thread/mod.rs";
        assert_eq!(path_everywhere(checkout.to_owned(), None, Some("/src/blaze/library")), named);
        let beside = "/src/blaze/library-x/src/lib.rs";
        assert_eq!(path_everywhere(beside.to_owned(), None, Some("/src/blaze/library")), beside);
        let fetched = "/home/me/.cargo/registry/src/index.crates.io-1/x-1.0.0/src/lib.rs";
        assert_eq!(
            path_everywhere(fetched.to_owned(), Some("/home/me/.cargo"), None),
            "/cargo/registry/src/index.crates.io-1/x-1.0.0/src/lib.rs"
        );
        assert_eq!(
            path_everywhere("src/main.rs".to_owned(), Some("/home/me/.cargo"), None),
            "src/main.rs"
        );
    }
}
