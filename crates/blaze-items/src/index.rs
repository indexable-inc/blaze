//! The items of the checkout as what refers to them sees them, kept from edit to edit: an edit
//! that moves, renames or changes items then asks again about what refers to what changed, and
//! not about all of each crate that can see the files it touched (#344).
//!
//! Each item has an entry: its interface form (with the body of a function seen into, `async`,
//! `const`, `gen` or returning `impl Trait`), what that refers to, and the names its source and
//! signature are written with. Each crate has the bindings of its modules. An edit's files are
//! written again and set beside their entries, and the crates that see them beside their
//! bindings: what changed is the items added, taken away or seen differently, and the names
//! bound to something else, whose users are written again in turn. What sees a changed item
//! through its interface changed too.

use base_db::{Crate, FileId};
use hir_def::{
    AdtId, DefWithBodyId, FunctionId, ItemContainerId, Lookup, MacroId, ModuleId,
    expr_store::{Body, ExpressionStore, path::Path},
    hir::{Expr, Pat, type_ref::TypeRef},
    nameres::{crate_def_map, diagnostics::DefDiagnosticKind},
    signatures::{ConstSignature, FunctionSignature},
    visibility::Visibility,
};
use hir_expand::HirFileId;
use hir_ty::db::HirDatabase;
use intern::Symbol;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use syntax::{AstNode, SyntaxKind, SyntaxNode, TextRange, ast::HasAttrs};

use std::sync::{Arc, OnceLock};

use crate::{Fan, ItemId, form, in_scope};

#[derive(Clone)]
pub struct Index {
    crates: FxHashSet<Crate>,
    entries: FxHashMap<ItemId, Entry>,
    files: FxHashMap<FileId, Vec<ItemId>>,
    /// For each item, those whose entries refer to it.
    users: FxHashMap<ItemId, FxHashSet<ItemId>>,
    /// For each name, the items whose entries are written with it.
    named: FxHashMap<Symbol, FxHashSet<ItemId>>,
    scopes: FxHashMap<Crate, Scope>,
    /// What each crate's own attributes say, as `DefMap::crate_data_print` writes it.
    attrs: FxHashMap<Crate, u64>,
    /// Files edited since their entries were written, by an edit that was not looked at here.
    dirty: FxHashSet<FileId>,
    /// Whether an edit since the bindings were taken can have changed them.
    scopes_dirty: bool,
    /// The items of `entries`, for [`Live`].
    live: Arc<FxHashSet<ItemId>>,
    /// What the last push found moved: each item gone with the one added in its place.
    moved: FxHashMap<ItemId, ItemId>,
    /// Those of `moved` written with other code, by the gone one: see [`moves`].
    renamed: FxHashSet<ItemId>,
    /// How many edits were pushed, which an entry is written after.
    pushes: u64,
    /// When each part of the last push was over, said when it is refused.
    parts: Vec<(&'static str, std::time::Duration)>,
}

/// The items an index has, which a thread of its own can ask of an item without asking salsa
/// what an edit took away: the module of an item gone is gone, and asking of it panics.
#[derive(Clone)]
pub struct Live {
    items: Arc<FxHashSet<ItemId>>,
    crates: Arc<FxHashSet<Crate>>,
}

impl Live {
    /// Whether `id` is of a crate the index doesn't look at, or one it has now. Only what
    /// salsa interned is asked of `id`; one of a file no crate has now is gone.
    pub fn has(&self, db: &dyn HirDatabase, id: ItemId) -> bool {
        self.items.contains(&id)
            || file_of(db, id).is_some_and(|file| {
                let crates = base_db::relevant_crates(db, file);
                !crates.is_empty() && !crates.iter().any(|it| self.crates.contains(it))
            })
    }

    pub fn has_body(&self, db: &dyn HirDatabase, body: DefWithBodyId) -> bool {
        self.has(
            db,
            match body {
                DefWithBodyId::FunctionId(it) => ItemId::Function(it),
                DefWithBodyId::ConstId(it) => ItemId::Const(it),
                DefWithBodyId::StaticId(it) => ItemId::Static(it),
                DefWithBodyId::VariantId(it) => ItemId::Adt(AdtId::EnumId(it.lookup(db).parent)),
            },
        )
    }
}

#[derive(Clone)]
struct Entry {
    file: FileId,
    /// Its crate, which the module of one gone can no longer be asked: the edit took the
    /// module away, and salsa what it knew of it.
    krate: Crate,
    /// The module it is written in and those that one is in.
    modules: Box<[ModuleId]>,
    digest: form::Hash,
    refs: Box<[ItemId]>,
    /// Of a function, the digest of its code and what that refers to, which a move keeps:
    /// `None` for one whose code could not all be written.
    code: Code,
    /// [`Index::pushes`] when it was written.
    written: u64,
    names: Box<[Symbol]>,
    missed: Option<&'static str>,
    /// Whether it is written with an error, so that it is told changed whenever it is
    /// written again.
    erred: bool,
    /// What [`changed_kind`], [`method_name`] and [`trait_counterpart`] say of it, kept for
    /// when it is gone and nothing can be asked of it.
    kind: Result<(), &'static str>,
    method: Option<Symbol>,
    counterpart: Option<ItemId>,
}

/// The bindings of a crate's modules, each name's by namespace: types and values, and macros.
#[derive(Clone, Default)]
struct Scope {
    names: FxHashMap<(ModuleId, Symbol), (u64, u64)>,
    /// The file each module is written in, and what a binding in it is a [`Bound`] as.
    modules: FxHashMap<ModuleId, (FileId, Option<Symbol>, bool)>,
    /// Whether a macro call of the crate's items names no macro, which a macro bound anywhere
    /// can be the one it names.
    unresolved: bool,
    /// The modules declared `#[macro_use]`, whose `macro_rules!` the module around sees after
    /// the declaration, by no name of its scope.
    textual: FxHashSet<ModuleId>,
    /// The module each module is in.
    parents: FxHashMap<ModuleId, ModuleId>,
    /// The names bound to a macro that only their module and those in it see.
    private_macros: FxHashSet<(ModuleId, Symbol)>,
}

/// What an edit changed, as [`Index::push`] found it.
pub struct Pushed {
    /// The items added, taken away or seen differently, and those that see one of them through
    /// their interface.
    pub changed: FxHashSet<ItemId>,
    /// The names bound to something else now or to nothing, each with the modules it is bound
    /// in so.
    bound: FxHashMap<Symbol, Vec<Bound>>,
    /// The names of methods added or taken away, which a call resolves through no module.
    methods: FxHashSet<Symbol>,
    /// The files written again: those edited, and those of modules added or taken away.
    pub files: Vec<FileId>,
    /// How many items were written again.
    pub written: usize,
}

impl Pushed {
    /// What changed but the items of `moved`, which are written as they were elsewhere, and
    /// nothing bound anew: what a moved function's code can have changed by.
    pub fn unmoved(&self, moved: &FxHashMap<ItemId, ItemId>) -> Pushed {
        let of_moves: FxHashSet<ItemId> = moved.iter().flat_map(|(a, b)| [*a, *b]).collect();
        Pushed {
            changed: self.changed.difference(&of_moves).copied().collect(),
            bound: FxHashMap::default(),
            methods: FxHashSet::default(),
            files: self.files.clone(),
            written: self.written,
        }
    }
}

#[derive(Clone)]
struct Bound {
    module: ModuleId,
    /// The module's name, which a path through it is written with.
    name: Option<Symbol>,
    /// Whether it is a crate's root, which a path from anywhere starts at.
    root: bool,
}

impl Pushed {
    /// How many names are bound to something else, or are of methods added or taken away.
    pub fn names(&self) -> usize {
        self.bound.len() + self.methods.len()
    }

    /// The names [`Self::names`] counts: no code written without one of them is
    /// [`Self::rebinds`].
    pub fn rebound(&self) -> impl Iterator<Item = &Symbol> {
        self.bound.keys().chain(&self.methods)
    }

    /// Whether code written in `modules` (its module and those that one is in) with `names`
    /// (in the order of their text) can call something by a name bound to another thing now: it
    /// names a method added or taken away, or a name bound differently in a crate's root, in
    /// one of `modules`, or in a module it names.
    pub fn rebinds(&self, modules: &[ModuleId], names: &[Symbol]) -> bool {
        let named = |it: &Symbol| names.binary_search_by(|n| n.as_str().cmp(it.as_str())).is_ok();
        names.iter().any(|name| {
            self.methods.contains(name)
                || self.bound.get(name).is_some_and(|bounds| {
                    bounds.iter().any(|it| {
                        it.root
                            || modules.contains(&it.module)
                            || it.name.as_ref().is_some_and(named)
                    })
                })
        })
    }
}

/// The module `id` is written in, and those that one is in.
pub fn modules_of(db: &dyn HirDatabase, id: ItemId) -> Vec<ModuleId> {
    use hir_def::HasModule;
    let module = match id {
        ItemId::Function(it) => it.module(db),
        ItemId::Adt(it) => it.module(db),
        ItemId::Trait(it) => it.module(db),
        ItemId::Impl(it) => it.module(db),
        ItemId::Const(it) => it.module(db),
        ItemId::Static(it) => it.module(db),
        ItemId::Alias(it) => it.module(db),
        ItemId::Macro(it) => it.module(db),
    };
    hir::Module::from(module).path_to_root(db).into_iter().map(ModuleId::from).collect()
}

/// The entries of some items, written apart from an [`Index`], on any thread.
pub struct Entries(Vec<(ItemId, Entry)>);

/// The modules of `crates`, for [`entries_in`].
pub fn modules_in(db: &dyn HirDatabase, crates: &[Crate]) -> Vec<ModuleId> {
    crates
        .iter()
        .flat_map(|&krate| crate_def_map(db, krate).modules(db).map(|(id, _)| id))
        .collect()
}

/// The entries of the items declared in `modules`, and in the bodies of those.
pub fn entries_in(db: &dyn HirDatabase, modules: &[ModuleId]) -> Entries {
    let (mut items, mut bodies) = (Vec::new(), Vec::new());
    for &id in modules {
        in_scope(db, id.def_map(db).module(db, id).scope(db), &mut items, &mut bodies);
    }
    crate::in_bodies(db, bodies, &mut items);
    let cx = form::Cx::new(db);
    Entries(items.into_iter().filter_map(|id| Some((id, entry(db, &cx, id)?))).collect())
}

impl Index {
    /// The entries of every item `crates` declare, and their bindings.
    pub fn new(db: &dyn HirDatabase, crates: &[Crate]) -> Index {
        Index::of_entries(db, crates, [entries_in(db, &modules_in(db, crates))])
    }

    /// The index of `crates` with the entries of the items they declare, and their bindings.
    pub fn of_entries(
        db: &dyn HirDatabase,
        crates: &[Crate],
        entries: impl IntoIterator<Item = Entries>,
    ) -> Index {
        let mut index = Index {
            crates: crates.iter().copied().collect(),
            entries: FxHashMap::default(),
            files: FxHashMap::default(),
            users: FxHashMap::default(),
            named: FxHashMap::default(),
            scopes: FxHashMap::default(),
            attrs: FxHashMap::default(),
            dirty: FxHashSet::default(),
            scopes_dirty: false,
            live: Arc::default(),
            moved: FxHashMap::default(),
            renamed: FxHashSet::default(),
            pushes: 0,
            parts: Vec::new(),
        };
        for Entries(part) in entries {
            for (id, entry) in part {
                index.insert(id, entry);
            }
        }
        for &krate in crates {
            index.scopes.insert(krate, scope(db, krate));
            index.attrs.insert(krate, attrs(db, krate));
        }
        index
    }

    pub fn items(&self) -> usize {
        self.entries.len()
    }

    /// [`moves`] of the last edit pushed, which a push refused after finding them keeps.
    pub fn moved(&self) -> &FxHashMap<ItemId, ItemId> {
        &self.moved
    }

    /// Those of [`Self::moved`] written elsewhere with other code, by the gone one: an item of
    /// the same interface and names, which what called the gone one calls as it did.
    pub fn renamed(&self) -> &FxHashSet<ItemId> {
        &self.renamed
    }

    /// The functions whose code is not written yet, and how many edits were pushed before:
    /// what [`codes_of`] writes of them is for [`Self::take_codes`].
    pub fn wanting_code(&self) -> (u64, Vec<FunctionId>) {
        let funcs = (self.entries.iter())
            .filter(|it| it.1.code.is_none() && !it.1.erred && it.1.missed.is_none())
            .filter_map(|it| match it.0 {
                ItemId::Function(func) => Some(*func),
                _ => None,
            })
            .collect();
        (self.pushes, funcs)
    }

    /// Takes the code of functions written after `pushes` edits, of those whose entries were
    /// not written again since.
    pub fn take_codes(&mut self, pushes: u64, funcs: &[FunctionId], codes: Vec<Code>) {
        for (&func, code) in funcs.iter().zip(codes) {
            if let Some(entry) = self.entries.get_mut(&ItemId::Function(func))
                && entry.written <= pushes
                && entry.code.is_none()
            {
                entry.code = code;
            }
        }
    }

    /// Writes the code of the functions of `files` whose code is not written yet, on the
    /// threads `fan` has: what a move out of them is found by.
    pub fn write_codes(&mut self, db: &dyn HirDatabase, fan: &Fan<'_>, files: &[FileId]) -> usize {
        let funcs: Vec<FunctionId> = (files.iter())
            .flat_map(|it| self.files.get(it).into_iter().flatten())
            .filter_map(|id| match (id, self.entries.get(id)) {
                (ItemId::Function(func), Some(entry))
                    if entry.code.is_none() && !entry.erred && entry.missed.is_none() =>
                {
                    Some(*func)
                }
                _ => None,
            })
            .collect();
        let live = self.live();
        for (func, code) in funcs.iter().zip(codes_of(db, fan, &funcs, Some(&live))) {
            if let Some(entry) = self.entries.get_mut(&ItemId::Function(*func)) {
                entry.code = code;
            }
        }
        funcs.len()
    }

    pub fn live(&self) -> Live {
        Live { items: self.live.clone(), crates: Arc::new(self.crates.clone()) }
    }

    /// Before an edit of `files`: what edits before it left out of date is written again, of
    /// the revision they left, and `files` are noted as edited.
    pub fn before_edit(&mut self, db: &dyn HirDatabase, files: &[FileId], fan: &Fan<'_>) {
        let dirty: Vec<FileId> = self.dirty.drain().collect();
        if !dirty.is_empty() {
            let declared = self.declared(db, fan, &dirty);
            let ids: Vec<ItemId> = declared.iter().flat_map(|it| it.1.iter().copied()).collect();
            let mut entries = entries_of(db, fan, &ids).into_iter();
            for (file, now) in declared {
                for id in self.files.remove(&file).unwrap_or_default() {
                    self.remove(id);
                }
                for id in now {
                    if let Some(entry) = entries.next().flatten() {
                        self.remove(id);
                        self.insert(id, entry);
                    }
                }
            }
        }
        if std::mem::take(&mut self.scopes_dirty) {
            let crates: Vec<Crate> = self.crates.iter().copied().collect();
            for (krate, scope) in crates.iter().copied().zip(scopes_of(db, fan, &crates)) {
                self.scopes.insert(krate, scope);
                self.attrs.insert(krate, attrs(db, krate));
            }
        }
        self.dirty.extend(files.iter().copied());
    }

    /// The items each of `files` declares now, on the threads `fan` has.
    fn declared(
        &self,
        db: &dyn HirDatabase,
        fan: &Fan<'_>,
        files: &[FileId],
    ) -> Vec<(FileId, Vec<ItemId>)> {
        let crates: Vec<Vec<Crate>> = files.iter().map(|&it| self.crates_of(db, it)).collect();
        let declared: Vec<OnceLock<Vec<ItemId>>> = files.iter().map(|_| OnceLock::new()).collect();
        let work = |db: &dyn HirDatabase, at: usize| {
            declared[at].get_or_init(|| {
                let mut now = Vec::new();
                for &krate in &crates[at] {
                    declared_in(db, krate, files[at], &mut now);
                }
                now
            });
        };
        fan(files.len(), &work);
        (0..files.len()).for_each(|at| work(db, at));
        files
            .iter()
            .copied()
            .zip(declared.into_iter().map(|it| it.into_inner().unwrap_or_default()))
            .collect()
    }

    /// Why an item of `files` could not be hashed at the last build, if one could not: a push
    /// that writes the files again hashes it again and refuses for it, so it refuses before.
    pub fn unhashed_in(&self, files: &[FileId]) -> Option<&'static str> {
        (files.iter().filter_map(|it| self.files.get(it)).flatten())
            .filter_map(|it| self.entries.get(it))
            .find_map(|it| it.missed)
    }

    /// Notes that an edit since [`Index::before_edit`] can have changed the bindings of modules.
    pub fn bindings_edited(&mut self) {
        self.scopes_dirty = true;
    }

    /// What the edit of `files` changed, of the revision after it; the entries and bindings are
    /// then those of it. `Err` says why that can't be told here, and the entries are left to be
    /// written again before the next edit.
    pub fn push(
        &mut self,
        db: &dyn HirDatabase,
        files: &[FileId],
        fan: &Fan<'_>,
    ) -> Result<Pushed, &'static str> {
        self.parts.clear();
        let start = std::time::Instant::now();
        let pushed = self.push_inner(db, files, fan);
        if pushed.is_err() {
            self.scopes_dirty = true;
            if std::env::var_os("RA_JIT_STATS").is_some() {
                let parts: Vec<String> = (self.parts.iter())
                    .map(|(part, at)| format!("{part} {}", at.as_micros()))
                    .collect();
                eprintln!(
                    "items-refused-us: {} at {} us",
                    parts.join(", "),
                    start.elapsed().as_micros()
                );
            }
        }
        pushed
    }

    fn push_inner(
        &mut self,
        db: &dyn HirDatabase,
        files: &[FileId],
        fan: &Fan<'_>,
    ) -> Result<Pushed, &'static str> {
        let start = std::time::Instant::now();
        self.moved.clear();
        self.renamed.clear();
        self.pushes += 1;
        if let Some(why) = self.unhashed_in(files) {
            return Err(why);
        }
        let mut took = Vec::new();
        let mut edited: FxHashSet<Crate> = FxHashSet::default();
        for &file in files {
            let crates = self.crates_of(db, file);
            if crates.is_empty() && base_db::relevant_crates(db, file).iter().next().is_some() {
                return Err("a file of a crate not looked at");
            }
            edited.extend(crates);
            // A file taken away is of no crate now, and its items are of the crates they were.
            let was = self.files.get(&file).into_iter().flatten();
            let was = was.filter_map(|it| self.entries.get(it)).map(|it| it.krate);
            edited.extend(was.filter(|it| self.crates.contains(it)));
        }
        for &krate in &edited {
            if self.attrs.get(&krate) != Some(&attrs(db, krate)) {
                return Err("an edit of a crate's own attributes");
            }
        }
        // An item of a kind no move lets through, added to the files or taken out of them,
        // refuses the push before the bindings are taken.
        for (file, now) in self.declared(db, fan, files) {
            let now_set: FxHashSet<ItemId> = now.iter().copied().collect();
            let was = self.files.get(&file).into_iter().flatten();
            for &id in was.filter(|it| !now_set.contains(it)) {
                if let Some(gone) = self.entries.get(&id) {
                    unmovable(gone.kind).map_err(|why| refused(db, id, why, "gone"))?;
                }
            }
            for &id in now.iter().filter(|it| !self.entries.contains_key(it)) {
                unmovable(changed_kind(db, id)).map_err(|why| refused(db, id, why, "added"))?;
            }
        }
        self.parts.push(("kinds", start.elapsed()));
        let mut around: FxHashSet<Crate> = edited.clone();
        for &krate in &edited {
            around
                .extend(krate.transitive_rev_deps(db).iter().filter(|it| self.crates.contains(it)));
        }

        // The bindings, and the files of the modules added or taken away.
        let mut bound: FxHashMap<Symbol, Vec<Bound>> = FxHashMap::default();
        let mut methods: FxHashSet<Symbol> = FxHashSet::default();
        // Of what the scopes kept, as a module taken away has nothing to be asked.
        let mut rebound = |key: &(ModuleId, Symbol), was: &Scope, now: &Scope| {
            let (module, name) = key;
            let (_, of, root) = now.modules.get(module).or(was.modules.get(module)).cloned()?;
            bound.entry(name.clone()).or_default().push(Bound { module: *module, name: of, root });
            Some(())
        };
        took.push(start.elapsed());
        self.parts.push(("crates", start.elapsed()));
        let mut rewritten: Vec<FileId> = files.to_vec();
        let mut scopes = Vec::new();
        let around: Vec<Crate> = around.into_iter().collect();
        for (&krate, now) in around.iter().zip(scopes_of(db, fan, &around)) {
            let was = self.scopes.get(&krate);
            let empty = Scope::default();
            let was = was.unwrap_or(&empty);
            // A macro bound in a module added or taken away is named only through it, where
            // no name could reach one before, and in what is written again or names it: a
            // glob of it binds the name in a module that stays as well. Only a call that named
            // no macro, or one the `macro_rules!` of a module declared `#[macro_use]` now or
            // before are seen in, can be another's now.
            let kept = |module: &ModuleId| {
                now.modules.contains_key(module) && was.modules.contains_key(module)
            };
            if now.textual != was.textual {
                return Err("a module declared `#[macro_use]` now or before");
            }
            for (module, (file, ..)) in now.modules.iter().chain(&was.modules) {
                let file_of = |scope: &Scope| scope.modules.get(module).map(|it| it.0);
                if file_of(&now) != file_of(was) && !rewritten.contains(file) {
                    rewritten.push(*file);
                }
            }
            let moved = |key: &(ModuleId, Symbol)| !was.unresolved && !kept(&key.0);
            // A macro bound in a module that stays is seen where the name is written, which is
            // asked again, and in what a macro wrote around the items: in calls of it among
            // the items of a module or a block, in a derive or an attribute. Those of a module
            // written again are written again too. Where only that module and those in it
            // can see the macro, and all of those are, nothing else can be another's.
            let within = |module: &ModuleId, of: &ModuleId| {
                let mut at = Some(*module);
                while let Some(it) = at {
                    if it == *of {
                        return true;
                    }
                    at = now.parents.get(&it).or(was.parents.get(&it)).copied();
                }
                false
            };
            let local = |key: &(ModuleId, Symbol)| {
                let private = |scope: &Scope| {
                    scope.names.get(key).is_none_or(|it| it.1 == 0)
                        || scope.private_macros.contains(key)
                };
                let written = |module: &ModuleId| {
                    !kept(module)
                        || now.modules.get(module).is_some_and(|it| rewritten.contains(&it.0))
                };
                !was.unresolved
                    && private(was)
                    && private(&now)
                    && (now.modules.keys().chain(was.modules.keys()))
                        .filter(|it| within(it, &key.0))
                        .all(written)
            };
            let macro_refused = |why: &'static str, (module, name): &(ModuleId, Symbol)| {
                if std::env::var_os("RA_JIT_STATS").is_some() {
                    let (kept, unresolved) = (kept(module), was.unresolved);
                    eprintln!(
                        "items-refused: {why}: `{}` in {module:?}, kept {kept}, unresolved {unresolved}",
                        name.as_str()
                    );
                }
                why
            };
            for (key, &(values, macros)) in &now.names {
                match was.names.get(key) {
                    Some(&(_, was_macros)) if was_macros != macros && !local(key) => {
                        return Err(macro_refused("a name bound to another macro", key));
                    }
                    Some(&it) if it == (values, macros) => {}
                    None if macros != 0 && !moved(key) && !local(key) => {
                        return Err(macro_refused("a name bound to a macro", key));
                    }
                    _ => rebound(key, was, &now).ok_or("a name bound in no module")?,
                }
            }
            for (key, &(_, macros)) in &was.names {
                if !now.names.contains_key(key) {
                    if macros != 0 && !moved(key) && !local(key) {
                        return Err(macro_refused("a macro's name bound to nothing", key));
                    }
                    rebound(key, was, &now).ok_or("a name bound in no module")?;
                }
            }
            scopes.push((krate, now));
        }
        // What a file no module is written in holds is in the items of whoever `include!`s
        // it, by no file here.
        let was_scopes = around.iter().filter_map(|it| self.scopes.get(it));
        let module_files: FxHashSet<FileId> = (scopes.iter().map(|it| &it.1).chain(was_scopes))
            .flat_map(|it| it.modules.values().map(|it| it.0))
            .collect();
        // One no crate `include!`s is in no crate's build, as an example cargo builds alone: it
        // holds no item.
        let unwritten: Vec<FileId> =
            files.iter().copied().filter(|it| !module_files.contains(it)).collect();
        if !unwritten.is_empty() {
            let elsewhere = (self.scopes.values())
                .flat_map(|it| it.modules.values())
                .any(|it| unwritten.contains(&it.0));
            if elsewhere {
                return Err("a file of a module of a crate the file is not of");
            }
            for &krate in &self.crates {
                let included = hir_def::include_macro_invoc(db, krate);
                if included.iter().any(|(_, it)| unwritten.contains(&it.file_id(db))) {
                    return Err("a file no module is written in that a module `include!`s");
                }
            }
        }
        for &krate in &edited {
            let included = hir_def::include_macro_invoc(db, krate);
            if included.iter().any(|(_, it)| files.contains(&it.file_id(db))) {
                return Err("a file a module `include!`s");
            }
        }

        // One written with a name bound anew that could not be hashed before the edit refuses
        // the push before the files are written again: it is written again below.
        let rewritten_files: FxHashSet<FileId> = rewritten.iter().copied().collect();
        let early = Pushed {
            changed: FxHashSet::default(),
            bound: std::mem::take(&mut bound),
            methods: FxHashSet::default(),
            files: Vec::new(),
            written: 0,
        };
        let unhashed = (early.bound.keys())
            .filter_map(|it| self.named.get(it))
            .flatten()
            .filter_map(|it| self.entries.get(it))
            .filter(|it| !rewritten_files.contains(&it.file))
            .find_map(|it| it.missed.filter(|_| early.rebinds(&it.modules, &it.names)));
        bound = early.bound;
        if let Some(why) = unhashed.or_else(|| self.unhashed_in(&rewritten)) {
            return Err(why);
        }
        took.push(start.elapsed());
        self.parts.push(("bindings", start.elapsed()));
        // The files written again, beside their entries.
        let mut changed: FxHashSet<ItemId> = FxHashSet::default();
        let mut counterparts: FxHashMap<ItemId, ItemId> = FxHashMap::default();
        let mut written = 0;
        let declared = self.declared(db, fan, &rewritten);
        let declared_at = start.elapsed();
        self.parts.push(("declared", declared_at));
        let ids: Vec<ItemId> = declared.iter().flat_map(|it| it.1.iter().copied()).collect();
        let mut entries = entries_of(db, fan, &ids);
        self.parts.push(("entries", start.elapsed()));
        // One whose hash can't be told refuses the push, before the moves are looked for.
        let unhashed =
            (ids.iter().zip(&entries)).find_map(|(&id, it)| Some((id, it.as_ref()?.missed?)));
        if let Some((id, why)) = unhashed {
            return Err(refused(db, id, why, "now"));
        }
        let now: FxHashSet<ItemId> = ids.iter().copied().collect();
        // And so does a kind of item added, gone or changed that no move lets through, of the
        // files of modules added or taken away too.
        for (file, _) in &declared {
            for &id in self.files.get(file).into_iter().flatten().filter(|it| !now.contains(it)) {
                if let Some(gone) = self.entries.get(&id) {
                    unmovable(gone.kind).map_err(|why| refused(db, id, why, "gone"))?;
                }
            }
        }
        for (&id, entry) in ids.iter().zip(&entries) {
            let Some(entry) = entry else { continue };
            match self.entries.get(&id) {
                None => unmovable(entry.kind).map_err(|why| refused(db, id, why, "added"))?,
                Some(was)
                    if was.erred
                        || entry.erred
                        || was.digest != entry.digest
                        || was.refs != entry.refs =>
                {
                    was.kind.map_err(|why| refused(db, id, why, "changed"))?;
                    entry.kind.map_err(|why| refused(db, id, why, "changed"))?;
                }
                Some(_) => {}
            }
        }
        self.parts.push(("changed kinds", start.elapsed()));
        // The code of an added function is written when a gone one is written alike, with its
        // code known from before the edit.
        let alike: FxHashSet<(form::Hash, &[Symbol])> = (declared.iter())
            .flat_map(|(file, _)| self.files.get(file).into_iter().flatten())
            .filter(|it| !now.contains(it))
            .filter_map(|it| self.entries.get(it))
            .filter(|it| it.code.is_some())
            .map(|it| (it.digest, &it.names[..]))
            .collect();
        let wanted: Vec<usize> = (0..ids.len())
            .filter(|&at| {
                matches!(ids[at], ItemId::Function(_)) && !self.entries.contains_key(&ids[at])
            })
            .filter(|&at| {
                entries[at].as_ref().is_some_and(|it| {
                    it.code.is_none() && alike.contains(&(it.digest, &it.names[..]))
                })
            })
            .collect();
        let funcs: Vec<FunctionId> = (wanted.iter())
            .filter_map(|&at| match ids[at] {
                ItemId::Function(it) => Some(it),
                _ => None,
            })
            .collect();
        for (at, code) in wanted.into_iter().zip(codes_of(db, fan, &funcs, None)) {
            if let Some(entry) = &mut entries[at] {
                entry.code = code;
            }
        }
        let gone: Vec<(ItemId, &Entry)> = (declared.iter())
            .flat_map(|(file, _)| self.files.get(file).into_iter().flatten())
            .filter(|it| !now.contains(it))
            .filter_map(|it| Some((*it, self.entries.get(it)?)))
            .collect();
        let added: Vec<(ItemId, &Entry)> = (ids.iter().zip(&entries))
            .filter(|(id, _)| !self.entries.contains_key(id))
            .filter_map(|(id, entry)| Some((*id, entry.as_ref()?)))
            .collect();
        self.parts.push(("codes", start.elapsed()));
        let (moved, renamed) = moves(&gone, &added);
        self.parts.push(("moves", start.elapsed()));
        if std::env::var_os("RA_JIT_STATS").is_some() {
            eprintln!(
                "items-moved: {} of {} gone and {} added, {} of them renamed",
                moved.len(),
                gone.len(),
                added.len(),
                renamed.len()
            );
        }
        self.renamed = renamed;
        // An impl of a trait written as it was elsewhere selects what it did: what called its
        // items calls those where they are now, and its trait's items are asked about.
        let of_moves: FxHashSet<ItemId> = moved.iter().flat_map(|(a, b)| [*a, *b]).collect();
        let kind = |id: ItemId, kind: Result<(), &'static str>| match kind {
            Err("an impl of a trait" | "an item of an impl of a trait")
                if of_moves.contains(&id) =>
            {
                Ok(())
            }
            other => other,
        };
        self.moved = moved;
        let mut entries = entries.into_iter();
        for (file, now) in declared {
            let was: FxHashSet<ItemId> =
                self.files.get(&file).into_iter().flatten().copied().collect();
            let now_set: FxHashSet<ItemId> = now.iter().copied().collect();
            for &id in was.iter().filter(|it| !now_set.contains(it)) {
                let gone = &self.entries[&id];
                kind(id, gone.kind).map_err(|why| refused(db, id, why, "gone"))?;
                methods.extend(gone.method.clone());
                counterparts.extend(gone.counterpart.map(|it| (id, it)));
                changed.insert(id);
            }
            for &id in &now {
                written += 1;
                let entry = entries.next().flatten().ok_or("an item that is not an item now")?;
                if let Some(why) = entry.missed {
                    return Err(why);
                }
                match self.entries.get(&id) {
                    Some(was)
                        if !was.erred
                            && !entry.erred
                            && was.digest == entry.digest
                            && was.refs == entry.refs => {}
                    Some(was) => {
                        was.kind.map_err(|why| refused(db, id, why, "changed"))?;
                        entry.kind.map_err(|why| refused(db, id, why, "changed"))?;
                        changed_header(id).map_err(|why| refused(db, id, why, "changed"))?;
                        changed.insert(id);
                    }
                    None => {
                        kind(id, entry.kind).map_err(|why| refused(db, id, why, "added"))?;
                        methods.extend(entry.method.clone());
                        changed.insert(id);
                    }
                }
                self.remove(id);
                self.insert(id, entry);
            }
            for id in was.into_iter().filter(|it| !now_set.contains(it)) {
                self.remove(id);
            }
            self.dirty.remove(&file);
        }

        took.push(start.elapsed());
        // What is written with a name bound to something else now, written again.
        let pushed =
            Pushed { changed: FxHashSet::default(), bound, methods, files: Vec::new(), written: 0 };
        let mut again: Vec<ItemId> = pushed
            .bound
            .keys()
            .chain(&pushed.methods)
            .filter_map(|it| self.named.get(it))
            .flatten()
            .copied()
            .filter(|it| {
                self.entries.get(it).is_some_and(|it| {
                    !rewritten_files.contains(&it.file) && pushed.rebinds(&it.modules, &it.names)
                })
            })
            .collect();
        again.sort_unstable_by_key(|it| format!("{it:?}"));
        again.dedup();
        let again_entries = entries_of(db, fan, &again);
        self.parts.push(("again", start.elapsed()));
        for (id, entry) in again.into_iter().zip(again_entries) {
            written += 1;
            let entry = entry.ok_or("an item gone with no edit of its file")?;
            if let Some(why) = entry.missed {
                return Err(why);
            }
            let was = &self.entries[&id];
            if was.erred || entry.erred || was.digest != entry.digest || was.refs != entry.refs {
                was.kind.map_err(|why| refused(db, id, why, "named"))?;
                entry.kind?;
                changed_header(id).map_err(|why| refused(db, id, why, "named"))?;
                changed.insert(id);
            }
            self.remove(id);
            self.insert(id, entry);
        }
        for (krate, now) in scopes {
            self.scopes.insert(krate, now);
        }

        took.push(start.elapsed());
        // And what sees one of those through its interface.
        let mut queue: Vec<ItemId> = changed.iter().copied().collect();
        while let Some(id) = queue.pop() {
            let mut reached: Vec<ItemId> =
                self.users.get(&id).into_iter().flatten().copied().collect();
            // What a projection through an impl is depends on the impl's associated items: a
            // change to one is a change to the trait's of its name, which the projection names.
            let of_trait = self.entries.get(&id).and_then(|it| it.counterpart);
            reached.extend(of_trait.or_else(|| counterparts.get(&id).copied()));
            for user in reached {
                if changed.insert(user) {
                    queue.push(user);
                }
            }
        }
        took.push(start.elapsed());
        if std::env::var_os("RA_JIT_STATS").is_some() {
            let took: Vec<u128> = took.iter().map(|it| it.as_micros()).collect();
            eprintln!(
                "items-pushed: {} of {} crates around; crates {} us, bindings {}, declared {}, files {}, names {}, users {}",
                around.len(),
                self.crates.len(),
                took[0],
                took[1],
                declared_at.as_micros(),
                took[2],
                took[3],
                took[4]
            );
        }
        Ok(Pushed { changed, files: rewritten, written, ..pushed })
    }

    fn crates_of(&self, db: &dyn HirDatabase, file: FileId) -> Vec<Crate> {
        base_db::relevant_crates(db, file)
            .iter()
            .copied()
            .filter(|it| self.crates.contains(it))
            .collect()
    }

    fn insert(&mut self, id: ItemId, mut entry: Entry) {
        entry.written = self.pushes;
        self.files.entry(entry.file).or_default().push(id);
        for &r in &entry.refs {
            self.users.entry(r).or_default().insert(id);
        }
        for name in &entry.names {
            self.named.entry(name.clone()).or_default().insert(id);
        }
        self.entries.insert(id, entry);
        Arc::make_mut(&mut self.live).insert(id);
    }

    fn remove(&mut self, id: ItemId) {
        let Some(entry) = self.entries.remove(&id) else { return };
        Arc::make_mut(&mut self.live).remove(&id);
        if let Some(items) = self.files.get_mut(&entry.file) {
            items.retain(|it| *it != id);
        }
        for r in &entry.refs {
            if let Some(users) = self.users.get_mut(r) {
                users.remove(&id);
            }
        }
        for name in &entry.names {
            if let Some(named) = self.named.get_mut(name) {
                named.remove(&id);
            }
        }
    }
}

/// The items an edit moved: each gone with the one added that is written the same and refers
/// to the same items in the same places, or to ones moved with it. Only a gone item and an added
/// one that are each the only one written so are taken for one moved. With them, the functions
/// renamed (the second): see [`Index::renamed`].
fn moves(
    gone: &[(ItemId, &Entry)],
    added: &[(ItemId, &Entry)],
) -> (FxHashMap<ItemId, ItemId>, FxHashSet<ItemId>) {
    let whole = |it: &&(ItemId, &Entry)| {
        !it.1.erred
            && it.1.missed.is_none()
            && (it.1.code.is_some() || !matches!(it.0, ItemId::Function(_)))
    };
    type Key<'a> = (form::Hash, Option<form::Hash>, Option<&'a [Symbol]>);
    let key =
        |entry: &Entry| -> Key<'_> { (entry.digest, entry.code.as_ref().map(|it| it.0), None) };
    let mut written: FxHashMap<Key<'_>, (Vec<ItemId>, Vec<ItemId>)> = FxHashMap::default();
    for &(id, entry) in gone.iter().filter(whole) {
        written.entry(key(entry)).or_default().0.push(id);
    }
    for &(id, entry) in added.iter().filter(whole) {
        written.entry(key(entry)).or_default().1.push(id);
    }
    // Of several written alike, those alone with their names.
    let entries: FxHashMap<ItemId, &Entry> = gone.iter().chain(added).copied().collect();
    let several: Vec<Key<'_>> =
        written.iter().filter(|it| it.1.0.len() > 1 || it.1.1.len() > 1).map(|it| *it.0).collect();
    for at in several {
        let (was, now) = written.remove(&at).unwrap_or_default();
        for (side, ids) in [(0, was), (1, now)] {
            for id in ids {
                let named = (at.0, at.1, Some(&entries[&id].names[..]));
                let it = written.entry(named).or_default();
                if side == 0 { it.0.push(id) } else { it.1.push(id) }
            }
        }
    }
    let alike = written.values().filter(|it| it.0.len() > 1 || it.1.len() > 1).count();
    let unlike = written.values().filter(|it| it.0.is_empty() || it.1.is_empty()).count();
    let twins: Vec<(Vec<ItemId>, Vec<ItemId>)> = (written.values())
        .filter(|it| (it.0.len() > 1 || it.1.len() > 1) && !it.0.is_empty() && !it.1.is_empty())
        .cloned()
        .collect();
    let mut moved: FxHashMap<ItemId, ItemId> = (written.into_values())
        .filter_map(|it| match (&it.0[..], &it.1[..]) {
            (&[was], &[now]) if std::mem::discriminant(&was) == std::mem::discriminant(&now) => {
                Some((was, now))
            }
            _ => None,
        })
        .collect();
    // Of the functions left, a gone one and an added one alone in their interface and names
    // are one renamed: written elsewhere with other code, which is compiled again, and called
    // as the gone one was.
    let mut renamed: FxHashSet<ItemId> = FxHashSet::default();
    let mut named: FxHashMap<(form::Hash, &[Symbol]), (Vec<ItemId>, Vec<ItemId>)> =
        FxHashMap::default();
    let moved_to: FxHashSet<ItemId> = moved.values().copied().collect();
    for (side, all) in [(0, gone), (1, added)] {
        for (id, entry) in all {
            if !matches!(id, ItemId::Function(_))
                || entry.erred
                || entry.missed.is_some()
                || moved.contains_key(id)
                || moved_to.contains(id)
            {
                continue;
            }
            let it = named.entry((entry.digest, &entry.names[..])).or_default();
            if side == 0 { it.0.push(*id) } else { it.1.push(*id) }
        }
    }
    for it in named.into_values() {
        if let (&[was], &[now]) = (&it.0[..], &it.1[..]) {
            moved.insert(was, now);
            renamed.insert(was);
        }
    }
    let interface: FxHashMap<ItemId, &[ItemId]> =
        (gone.iter().chain(added)).map(|(id, entry)| (*id, &entry.refs[..])).collect();
    let refs: FxHashMap<ItemId, Vec<ItemId>> = (gone.iter().chain(added))
        .map(|(id, entry)| {
            let code = entry.code.iter().flat_map(|it| it.1.iter());
            (*id, entry.refs.iter().chain(code).copied().collect())
        })
        .collect();
    // Of several written alike, a gone one and an added one that alone refer to the same items,
    // or to ones moved with them: an item's impl or the type it is of tells twins apart, such as
    // the methods of the impls a derive writes. Each pair can tell more, until none is found.
    let mut twins = twins;
    let mut twinned = 0;
    loop {
        let mut found = Vec::new();
        for (was, now) in &mut twins {
            let mut by: FxHashMap<Vec<ItemId>, (Vec<ItemId>, Vec<ItemId>)> = FxHashMap::default();
            for it in was.iter() {
                let to = refs[it].iter().map(|it| *moved.get(it).unwrap_or(it)).collect();
                by.entry(to).or_default().0.push(*it);
            }
            for it in now.iter() {
                by.entry(refs[it].clone()).or_default().1.push(*it);
            }
            for (a, b) in by.into_values() {
                if let (&[a], &[b]) = (&a[..], &b[..])
                    && std::mem::discriminant(&a) == std::mem::discriminant(&b)
                {
                    found.push((a, b));
                    was.retain(|it| *it != a);
                    now.retain(|it| *it != b);
                }
            }
        }
        if found.is_empty() {
            break;
        }
        twinned += found.len();
        moved.extend(found);
    }
    let paired = moved.len();
    // What one refers to has to have moved with it: those that don't are dropped until none
    // is left that refers to one dropped.
    loop {
        let wrong: Vec<ItemId> = (moved.iter())
            .filter(|&(was, now)| {
                let (was, now): (&[ItemId], &[ItemId]) = match renamed.contains(was) {
                    true => (interface[was], interface[now]),
                    false => (&refs[was], &refs[now]),
                };
                was.len() != now.len()
                    || was.iter().zip(now).any(|(a, b)| a != b && moved.get(a) != Some(b))
            })
            .map(|it| *it.0)
            .collect();
        if wrong.is_empty() {
            if std::env::var_os("RA_JIT_STATS").is_some() {
                let erred = gone.iter().filter(|it| !whole(it)).count();
                let kinds = |all: &[(ItemId, &Entry)]| {
                    let mut kinds: FxHashMap<&str, usize> = FxHashMap::default();
                    for (id, _) in all.iter().filter(|it| !moved.contains_key(&it.0)) {
                        *kinds.entry(id.kind()).or_default() += 1;
                    }
                    let mut kinds: Vec<_> = kinds.into_iter().collect();
                    kinds.sort();
                    kinds
                };
                eprintln!(
                    "items-unmoved: {erred} of the gone with errors, {} digests of several ({twinned} twins paired by their refs), {} of one side, {} paired and dropped for their refs; unmoved gone {:?}",
                    alike,
                    unlike,
                    paired - moved.len(),
                    kinds(gone)
                );
                let now: FxHashSet<ItemId> = moved.values().copied().collect();
                let mut twins: FxHashMap<&[Symbol], Vec<&Entry>> = FxHashMap::default();
                for (_, entry) in added.iter().filter(|it| !now.contains(&it.0)) {
                    twins.entry(&entry.names[..]).or_default().push(entry);
                }
                let (mut digest, mut refs, mut shown, mut other) = (0, 0, 0, 0);
                for (id, was) in gone.iter().filter(|it| !moved.contains_key(&it.0)) {
                    let Some([now]) = twins.get(&was.names[..]).map(|it| &it[..]) else { continue };
                    let other_code =
                        was.code.as_ref().map(|it| it.0) != now.code.as_ref().map(|it| it.0);
                    if now.digest != was.digest {
                        digest += 1;
                    } else {
                        refs += 1;
                    }
                    other += usize::from(other_code);
                    if shown < 12 && other_code {
                        shown += 1;
                        let names: Vec<&str> = was.names.iter().map(|it| it.as_str()).collect();
                        let diff: Vec<String> = (was.refs.iter().zip(&now.refs))
                            .filter(|(a, b)| a != b && moved.get(a) != Some(b))
                            .take(3)
                            .map(|(a, b)| format!("{a:?}->{b:?}"))
                            .collect();
                        let code =
                            |it: &Entry| it.code.as_ref().map(|(hash, refs)| (*hash, refs.len()));
                        let (was_code, now_code) = (code(was), code(now));
                        let gone_ids: FxHashSet<ItemId> = gone.iter().map(|it| it.0).collect();
                        let added_ids: FxHashSet<ItemId> = added.iter().map(|it| it.0).collect();
                        let side =
                            |it: &ItemId| match (gone_ids.contains(it), added_ids.contains(it)) {
                                (true, _) => "gone",
                                (_, true) => "added",
                                _ => "kept",
                            };
                        let code_diff: Vec<String> = (was.code.iter().zip(&now.code))
                            .flat_map(|(a, b)| a.1.iter().zip(b.1.iter()))
                            .filter(|(a, b)| a != b && moved.get(a) != Some(b))
                            .take(3)
                            .map(|(a, b)| {
                                format!("{} {} -> {} {}", a.kind(), side(a), b.kind(), side(b))
                            })
                            .collect();
                        eprintln!(
                            "items-unmoved: {} {names:?}: digest {}, refs {} and {}, {diff:?}, erred {} {}, missed {:?}, code {} and {} ({}), code refs {:?} and {:?} {code_diff:?}",
                            id.kind(),
                            if now.digest == was.digest { "same" } else { "other" },
                            was.refs.len(),
                            now.refs.len(),
                            was.erred,
                            now.erred,
                            now.missed,
                            was_code.is_some(),
                            now_code.is_some(),
                            if was_code.map(|it| it.0) == now_code.map(|it| it.0) {
                                "same"
                            } else {
                                "other"
                            },
                            was_code.map(|it| it.1),
                            now_code.map(|it| it.1),
                        );
                    }
                }
                eprintln!(
                    "items-unmoved: twins by name with another digest {digest}, with the digest {refs}, with other code {other}"
                );
            }
            renamed.retain(|it| moved.contains_key(it));
            return (moved, renamed);
        }
        for it in wrong {
            moved.remove(&it);
        }
    }
}

/// Whether a change to `id` is one [`Index::push`] can follow: what an impl or a trait is, or
/// a macro, changes what code is in more ways than what refers to them shows.
/// `why` an item `how` can't be pushed, said with RA_JIT_STATS.
fn refused(db: &dyn HirDatabase, id: ItemId, why: &'static str, how: &str) -> &'static str {
    if std::env::var_os("RA_JIT_STATS").is_some() {
        // Nothing is asked of one gone, whose code the database no longer has.
        let name = match id {
            ItemId::Function(it) if how != "gone" => {
                Some(FunctionSignature::of(db, it).name.symbol().clone())
            }
            _ => None,
        };
        let file = (how != "gone").then(|| file_of(db, id)).flatten();
        eprintln!("items-refused: {why}, {how}: {id:?} {name:?} in {file:?}");
    }
    why
}

/// What of `kind` refuses an item added or taken away even when it moved: an impl of a trait
/// written as it was elsewhere selects what it did.
fn unmovable(kind: Result<(), &'static str>) -> Result<(), &'static str> {
    match kind {
        Err("an impl of a trait" | "an item of an impl of a trait") => Ok(()),
        other => other,
    }
}

fn changed_kind(db: &dyn HirDatabase, id: ItemId) -> Result<(), &'static str> {
    let container = |container| match container {
        ItemContainerId::TraitId(_) => Err("an item of a trait"),
        ItemContainerId::ImplId(imp) if db.impl_trait(imp).is_some() => {
            Err("an item of an impl of a trait")
        }
        _ => Ok(()),
    };
    match id {
        // An inherent impl gone or added is its items gone or added, named by their names.
        ItemId::Impl(imp) if db.impl_trait(imp).is_some() => Err("an impl of a trait"),
        ItemId::Impl(_) => Ok(()),
        ItemId::Trait(_) => Err("a trait"),
        ItemId::Macro(_) => Err("a macro"),
        ItemId::Function(it) => container(it.lookup(db).container),
        ItemId::Const(it) => container(it.lookup(db).container),
        ItemId::Alias(it) => match it.lookup(db).container {
            ItemContainerId::ImplId(_) => Err("an associated type"),
            other => container(other),
        },
        ItemId::Adt(_) | ItemId::Static(_) => Ok(()),
    }
}

/// The name of a function or constant of an impl: one added or taken away can be what a call
/// of the name or a path to it through its type resolves to.
fn method_name(db: &dyn HirDatabase, id: ItemId) -> Option<Symbol> {
    match id {
        ItemId::Function(func) => {
            let ItemContainerId::ImplId(_) = func.lookup(db).container else { return None };
            Some(FunctionSignature::of(db, func).name.symbol().clone())
        }
        ItemId::Const(konst) => {
            let ItemContainerId::ImplId(_) = konst.lookup(db).container else { return None };
            Some(ConstSignature::of(db, konst).name.as_ref()?.symbol().clone())
        }
        _ => None,
    }
}

/// Why an item seen differently can't be pushed, beside what its kind says: what an impl's
/// header is decides which calls its items take, and nothing here tells which.
fn changed_header(id: ItemId) -> Result<(), &'static str> {
    match id {
        ItemId::Impl(_) => Err("an impl seen differently"),
        _ => Ok(()),
    }
}

/// The item of a trait an associated item of an impl of it is a value of.
fn trait_counterpart(db: &dyn HirDatabase, id: ItemId) -> Option<ItemId> {
    let (container, name) = match id {
        ItemId::Alias(it) => {
            let name = hir_def::signatures::TypeAliasSignature::of(db, it).name.clone();
            (it.lookup(db).container, name)
        }
        ItemId::Const(it) => {
            let name = hir_def::signatures::ConstSignature::of(db, it).name.clone()?;
            (it.lookup(db).container, name)
        }
        _ => return None,
    };
    let ItemContainerId::ImplId(imp) = container else { return None };
    let trait_ref = db.impl_trait(imp)?;
    let trait_id = trait_ref.skip_binder().def_id.0;
    trait_id.trait_items(db).items.iter().find(|(it, _)| *it == name).map(|&(_, item)| item.into())
}

/// The entry of an item, or `None` for one that is not one now.
/// The entries of `ids`, on the threads `fan` has.
/// The digest of a function's code and what it refers to, `None` when it can't all be written.
pub type Code = Option<(form::Hash, Box<[ItemId]>)>;

/// The [`Code`] of a function's code form: its digest with the places of its code.
fn written_code(code: form::Sub) -> Code {
    let digest =
        *blake3::Hasher::new().update(&code.digest).update(&code.spans).finalize().as_bytes();
    code.missed.is_none().then(|| (digest, code.refs.into()))
}

/// [`Code`] of each of `ids`, on the threads `fan` has; `None` of one `live` doesn't have.
pub fn codes_of(
    db: &dyn HirDatabase,
    fan: &Fan<'_>,
    ids: &[FunctionId],
    live: Option<&Live>,
) -> Vec<Code> {
    const PART: usize = 8;
    let parts: Vec<OnceLock<Vec<Code>>> =
        (0..ids.len().div_ceil(PART)).map(|_| OnceLock::new()).collect();
    let work = |db: &dyn HirDatabase, at: usize| {
        parts[at].get_or_init(|| {
            let cx = form::Cx::new(db);
            let part = &ids[at * PART..((at + 1) * PART).min(ids.len())];
            (part.iter())
                .map(|&func| {
                    if live.is_some_and(|it| !it.has(db, ItemId::Function(func))) {
                        return None;
                    }
                    // A function the database can't write the code of is one that isn't moved;
                    // a write to it stops all of them.
                    let code = std::panic::AssertUnwindSafe(|| written_code(cx.code(func.into())));
                    match std::panic::catch_unwind(code) {
                        Ok(code) => code,
                        Err(stopped) => match stopped.downcast_ref::<salsa::Cancelled>() {
                            Some(salsa::Cancelled::PropagatedPanic) | None => None,
                            Some(_) => std::panic::resume_unwind(stopped),
                        },
                    }
                })
                .collect()
        });
    };
    fan(parts.len(), &work);
    (0..parts.len()).for_each(|at| work(db, at));
    parts.into_iter().filter_map(OnceLock::into_inner).flatten().collect()
}

fn entries_of(db: &dyn HirDatabase, fan: &Fan<'_>, ids: &[ItemId]) -> Vec<Option<Entry>> {
    const PART: usize = 8;
    let parts: Vec<OnceLock<Vec<Option<Entry>>>> =
        (0..ids.len().div_ceil(PART)).map(|_| OnceLock::new()).collect();
    let work = |db: &dyn HirDatabase, at: usize| {
        parts[at].get_or_init(|| {
            let cx = form::Cx::new(db);
            let part = &ids[at * PART..((at + 1) * PART).min(ids.len())];
            part.iter().map(|&id| entry(db, &cx, id)).collect()
        });
    };
    fan(parts.len(), &work);
    (0..parts.len()).for_each(|at| work(db, at));
    parts.into_iter().flat_map(|it| it.into_inner().unwrap_or_default()).collect()
}

/// The bindings of each of `crates`, on the threads `fan` has.
fn scopes_of(db: &dyn HirDatabase, fan: &Fan<'_>, crates: &[Crate]) -> Vec<Scope> {
    let scopes: Vec<OnceLock<Scope>> = crates.iter().map(|_| OnceLock::new()).collect();
    let work = |db: &dyn HirDatabase, at: usize| {
        scopes[at].get_or_init(|| scope(db, crates[at]));
    };
    fan(crates.len(), &work);
    (0..crates.len()).for_each(|at| work(db, at));
    scopes.into_iter().filter_map(OnceLock::into_inner).collect()
}

fn entry(db: &dyn HirDatabase, cx: &form::Cx<'_>, id: ItemId) -> Option<Entry> {
    let file = file_of(db, id)?;
    // Before anything is asked of one gone, which nothing would find in its file.
    source(db, id)?;
    let node = cx.node_with(id, false);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&node.interface.digest);
    let mut refs = node.interface.refs;
    let (mut missed, mut erred) = (node.interface.missed, node.interface.erred);
    let seen = seen_into(db, id, &refs);
    let mut whole_code = None;
    if let (true, ItemId::Function(func)) = (seen, id) {
        let code = cx.code(func.into());
        hasher.update(&code.digest);
        refs.extend(code.refs.iter().copied());
        if code.missed.is_some() {
            erred = (missed.is_none() || erred) && code.erred;
            missed = missed.or(code.missed);
        }
        whole_code = written_code(code);
    }
    let missed = missed.filter(|_| !erred);
    let mut unique = FxHashSet::default();
    refs.retain(|it| unique.insert(*it));
    let mut names = Vec::new();
    interface_names(db, id, seen, &mut names)?;
    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    Some(Entry {
        file,
        krate: id.krate(db),
        modules: modules_of(db, id).into(),
        digest: *hasher.finalize().as_bytes(),
        refs: refs.into(),
        code: whole_code,
        written: 0,
        names: names.into(),
        missed,
        erred,
        kind: changed_kind(db, id),
        method: method_name(db, id),
        counterpart: trait_counterpart(db, id),
    })
}

/// Whether what refers to the function sees into its body: the future of an `async fn`, the
/// value of a `const fn` where it is evaluated, the type an `impl Trait` it returns is.
fn seen_into(db: &dyn HirDatabase, id: ItemId, interface_refs: &[ItemId]) -> bool {
    let ItemId::Function(func) = id else { return false };
    let signature = FunctionSignature::of(db, func);
    signature.is_async()
        || signature.is_const()
        || signature.is_gen()
        || interface_refs.contains(&id)
}

/// The file an item is written in, or of the macro call that wrote it.
pub fn file_of(db: &dyn HirDatabase, id: ItemId) -> Option<FileId> {
    let file: HirFileId = match id {
        ItemId::Function(it) => it.lookup(db).id.file_id,
        ItemId::Adt(AdtId::StructId(it)) => it.lookup(db).id.file_id,
        ItemId::Adt(AdtId::UnionId(it)) => it.lookup(db).id.file_id,
        ItemId::Adt(AdtId::EnumId(it)) => it.lookup(db).id.file_id,
        ItemId::Trait(it) => it.lookup(db).id.file_id,
        ItemId::Impl(it) => it.lookup(db).id.file_id,
        ItemId::Const(it) => it.lookup(db).id.file_id,
        ItemId::Static(it) => it.lookup(db).id.file_id,
        ItemId::Alias(it) => it.lookup(db).id.file_id,
        ItemId::Macro(MacroId::Macro2Id(it)) => it.lookup(db).id.file_id,
        ItemId::Macro(MacroId::MacroRulesId(it)) => it.lookup(db).id.file_id,
        ItemId::Macro(MacroId::ProcMacroId(it)) => it.lookup(db).id.file_id,
    };
    Some(file.original_file(db).file_id(db))
}

/// The node of an item, and of a function's body; `None` for one its file no longer has.
fn source(db: &dyn HirDatabase, id: ItemId) -> Option<(SyntaxNode, Option<TextRange>)> {
    fn node<N: AstNode>(db: &dyn HirDatabase, id: hir_expand::AstId<N>) -> Option<SyntaxNode> {
        let ptr = id.file_id.ast_id_map(db).try_get_erased(id.value.erase())?;
        Some(ptr.to_node(&id.file_id.parse_or_expand(db)))
    }
    Some(match id {
        ItemId::Function(it) => {
            let fun = node(db, it.lookup(db).id)?;
            let body = syntax::ast::Fn::cast(fun.clone())
                .and_then(|it| it.body())
                .map(|it| it.syntax().text_range());
            (fun, body)
        }
        ItemId::Adt(AdtId::StructId(it)) => (node(db, it.lookup(db).id)?, None),
        ItemId::Adt(AdtId::UnionId(it)) => (node(db, it.lookup(db).id)?, None),
        ItemId::Adt(AdtId::EnumId(it)) => (node(db, it.lookup(db).id)?, None),
        ItemId::Trait(it) => (node(db, it.lookup(db).id)?, None),
        ItemId::Impl(it) => (node(db, it.lookup(db).id)?, None),
        ItemId::Const(it) => (node(db, it.lookup(db).id)?, None),
        ItemId::Static(it) => (node(db, it.lookup(db).id)?, None),
        ItemId::Alias(it) => (node(db, it.lookup(db).id)?, None),
        ItemId::Macro(MacroId::Macro2Id(it)) => (node(db, it.lookup(db).id)?, None),
        ItemId::Macro(MacroId::MacroRulesId(it)) => (node(db, it.lookup(db).id)?, None),
        ItemId::Macro(MacroId::ProcMacroId(it)) => (node(db, it.lookup(db).id)?, None),
    })
}

/// The names an item's interface is written with: those of its source, without the body of a
/// function not seen into.
fn interface_names(
    db: &dyn HirDatabase,
    id: ItemId,
    seen: bool,
    out: &mut Vec<Symbol>,
) -> Option<()> {
    let (node, body) = source(db, id)?;
    idents(&node, body.filter(|_| !seen), out);
    if let (true, ItemId::Function(func)) = (seen, id) {
        store_names(&Body::of(db, func.into()), out);
    }
    Some(())
}

/// Whether an edit that writes each `(file, text)` can move an item between its files through
/// [`Index::push`], as the texts alone tell before it: a name declared is gone from one file
/// and one is declared anew in another, and no trait or macro is added or taken away, which
/// no move lets through.
pub fn moves_written(db: &dyn HirDatabase, written: &[(FileId, &str)]) -> bool {
    let changes = declared_changes(db, written);
    let files = |pick: fn(&Declared) -> bool| {
        (written.iter().zip(&changes)).filter(move |it| pick(it.1)).map(|it| it.0.0)
    };
    !changes.iter().any(|it| it.unmovable)
        && files(|it| it.gone).any(|from| files(|it| it.added).any(|to| to != from))
}

/// Whether names declared are gone from a file of `written` that declares none anew, and no
/// file declares one anew: half of a move, whose other half is still to be written. A file
/// that takes names away and declares others renames them.
pub fn moved_out(db: &dyn HirDatabase, written: &[(FileId, &str)]) -> bool {
    let changes = declared_changes(db, written);
    !changes.iter().any(|it| it.added) && changes.iter().any(|it| it.gone)
}

/// How writing a file changes the names it declares.
#[derive(Clone, Copy, Default)]
struct Declared {
    /// A trait or a macro is added or taken away.
    unmovable: bool,
    /// A name is no longer declared.
    gone: bool,
    /// A name is declared anew.
    added: bool,
}

/// Of each `(file, text)` written, how the names it declares change: each file lexed on a
/// thread of its own, as a save can be of many.
fn declared_changes(db: &dyn HirDatabase, written: &[(FileId, &str)]) -> Vec<Declared> {
    let was: Vec<&str> = written.iter().map(|&(file, _)| &**db.file_text(file).text(db)).collect();
    let one = |at: usize| {
        let mut was = names_declared(was[at]);
        let now = names_declared(written[at].1);
        let unmovable = |it: &&(&str, &str)| matches!(it.0, "trait" | "macro" | "macro_rules");
        let unmovable = was.iter().filter(unmovable).ne(now.iter().filter(unmovable));
        let mut added = false;
        for it in &now {
            match was.binary_search(it) {
                Ok(at) => _ = was.remove(at),
                Err(_) => added = true,
            }
        }
        Declared { unmovable, gone: !was.is_empty(), added }
    };
    let threads = std::thread::available_parallelism().map_or(1, |it| it.get());
    let threads = threads.min(written.len()).max(1);
    let one = &one;
    let mut changes = vec![Declared::default(); written.len()];
    std::thread::scope(|scope| {
        let parts: Vec<_> = (0..threads)
            .map(|part| {
                scope.spawn(move || {
                    let ats = (part..written.len()).step_by(threads);
                    ats.map(|at| (at, one(at))).collect::<Vec<_>>()
                })
            })
            .collect();
        for part in parts {
            for (at, it) in part.join().unwrap_or_default() {
                changes[at] = it;
            }
        }
    });
    changes
}

/// The names `text` declares, each after its keyword and sorted: lexed, not parsed, as each
/// file of an edit is read twice before it.
fn names_declared(text: &str) -> Vec<(&str, &str)> {
    use SyntaxKind::*;
    let lexed = syntax::LexedStr::new(syntax::Edition::CURRENT, text);
    let tokens: Vec<(SyntaxKind, &str)> = (0..lexed.len())
        .map(|at| (lexed.kind(at), &text[lexed.text_range(at)]))
        .filter(|(kind, _)| !kind.is_trivia())
        .collect();
    let mut names: Vec<(&str, &str)> = (0..tokens.len())
        .filter_map(|at| match (tokens[at], tokens.get(at + 1), tokens.get(at + 2)) {
            ((FN_KW | STRUCT_KW | ENUM_KW | TYPE_KW | CONST_KW, kind), Some(&(IDENT, name)), _)
            | ((STATIC_KW | TRAIT_KW | MOD_KW | MACRO_KW, kind), Some(&(IDENT, name)), _)
            | ((STATIC_KW, kind), Some(&(MUT_KW, _)), Some(&(IDENT, name))) => Some((kind, name)),
            ((IDENT, "union"), Some(&(IDENT, name)), _) => Some(("union", name)),
            ((IDENT, "macro_rules"), Some(&(BANG, _)), Some(&(IDENT, name))) => {
                Some(("macro_rules", name))
            }
            _ => None,
        })
        .collect();
    names.sort_unstable();
    names
}

/// The names a body's code is written with: those of its source, and of the paths and method
/// calls macros wrote into it; `None` for a body its file no longer has.
pub fn code_names(db: &dyn HirDatabase, body: DefWithBodyId) -> Option<Vec<Symbol>> {
    let mut out = Vec::new();
    let id = match body {
        DefWithBodyId::FunctionId(it) => Some(ItemId::Function(it)),
        DefWithBodyId::ConstId(it) => Some(ItemId::Const(it)),
        DefWithBodyId::StaticId(it) => Some(ItemId::Static(it)),
        DefWithBodyId::VariantId(_) => None,
    };
    if let Some(id) = id {
        idents(&source(db, id)?.0, None, &mut out);
    }
    store_names(&Body::of(db, body), &mut out);
    out.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
    out.dedup();
    Some(out)
}

fn idents(node: &SyntaxNode, skip: Option<TextRange>, out: &mut Vec<Symbol>) {
    for token in node.descendants_with_tokens().filter_map(|it| it.into_token()) {
        if token.kind() != SyntaxKind::IDENT
            || skip.is_some_and(|it| it.contains_range(token.text_range()))
        {
            continue;
        }
        let text = token.text();
        out.push(Symbol::intern(text.strip_prefix("r#").unwrap_or(text)));
    }
}

fn store_names(store: &ExpressionStore, out: &mut Vec<Symbol>) {
    fn path(path: &Path, out: &mut Vec<Symbol>) {
        for segment in path.segments().iter() {
            out.push(segment.name.symbol().clone());
        }
    }
    for (_, expr) in store.exprs() {
        match expr {
            Expr::Path(it) | Expr::RecordLit { path: it, .. } => path(it, out),
            Expr::MethodCall { method_name, .. } => out.push(method_name.symbol().clone()),
            _ => {}
        }
    }
    for (_, pat) in store.pats() {
        match pat {
            Pat::Path(it) | Pat::Record { path: it, .. } | Pat::TupleStruct { path: it, .. } => {
                path(it, out)
            }
            _ => {}
        }
    }
    for (_, ty) in store.types.iter() {
        if let TypeRef::Path(it) = ty {
            path(it, out);
        }
    }
}

/// The bindings of `krate`'s modules.
fn scope(db: &dyn HirDatabase, krate: Crate) -> Scope {
    use std::hash::{Hash, Hasher};
    let mut scope = Scope::default();
    let def_map = crate_def_map(db, krate);
    scope.unresolved = def_map
        .diagnostics()
        .iter()
        .any(|it| matches!(it.kind, DefDiagnosticKind::UnresolvedMacroCall { .. }));
    for (module, data) in def_map.modules(db) {
        let file = data.definition_source_file_id().original_file(db).file_id(db);
        let name = module.name(db).map(|it| it.symbol().clone());
        let root = module.containing_module(db).is_none() && !module.is_block_module(db);
        scope.modules.insert(module, (file, name, root));
        if let Some(parent) = hir::Module::from(module).parent(db) {
            scope.parents.insert(module, parent.into());
        }
        let macro_use =
            |it: syntax::ast::Attr| it.syntax().text().to_string().contains("macro_use");
        if data.declaration_source(db).is_some_and(|it| it.value.attrs().any(macro_use)) {
            scope.textual.insert(module);
        }
        for (name, per_ns) in data.scope(db).entries() {
            let hash = |it: &dyn Fn(&mut FxHasher)| {
                let mut hasher = FxHasher::default();
                it(&mut hasher);
                hasher.finish()
            };
            // What a name is bound to and who sees it, not the import it comes through: a
            // reorder of the `use`s of a module gives its imports other ids.
            let values = hash(&|h| {
                per_ns.types.as_ref().map(|it| (it.def, it.vis)).hash(h);
                per_ns.values.as_ref().map(|it| (it.def, it.vis)).hash(h);
            });
            let macros = match per_ns.macros {
                Some(it) => hash(&|h| (it.def, it.vis).hash(h)) | 1,
                None => 0,
            };
            if per_ns
                .macros
                .is_some_and(|it| matches!(it.vis, Visibility::Module(m, _) if m == module))
            {
                scope.private_macros.insert((module, name.symbol().clone()));
            }
            scope.names.insert((module, name.symbol().clone()), (values, macros));
        }
    }
    scope
}

fn attrs(db: &dyn HirDatabase, krate: Crate) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = FxHasher::default();
    crate_def_map(db, krate).crate_data_print().hash(&mut hasher);
    hasher.finish()
}

/// The items `krate` declares in `file`: in its modules written there, the impls and traits of
/// those, and the blocks of the bodies of all of them.
fn declared_in(db: &dyn HirDatabase, krate: Crate, file: FileId, out: &mut Vec<ItemId>) {
    let def_map = crate_def_map(db, krate);
    let mut bodies = Vec::new();
    for (_, data) in def_map.modules(db) {
        if data.definition_source_file_id().original_file(db).file_id(db) == file {
            in_scope(db, data.scope(db), out, &mut bodies);
        }
    }
    while let Some(body) = bodies.pop() {
        for (_, def_map) in Body::of(db, body).blocks(db) {
            for (_, module) in def_map.modules(db) {
                in_scope(db, module.scope(db), out, &mut bodies);
            }
        }
    }
}
