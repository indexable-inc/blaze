//! Where each item is documented: the module tree of every crate documented, walked as rustdoc
//! walks it, with `pub use` either inlined (the item gets a page where it is re-exported) or
//! left as a line under "Re-exports", and the one page every other page links to for an item.
//!
//! The rules are rustdoc's (`clean::clean_use_statement` and `visit_ast` in librustdoc at rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0): a re-export of another crate's item is inlined
//! unless `#[doc(no_inline)]`; one of this crate's is inlined when `#[doc(inline)]` asks or the
//! item cannot be reached where it is declared; a glob from a module that cannot be reached is
//! inlined item by item; `#[doc(hidden)]` drops the item or the re-export.

use base_db::{Db, LangCrateOrigin};
use blaze_hash::{HashMap, HashSet, SharedMap};
use hir::{
    AsAssocItem, AssocItem, AssocItemContainer, Crate, HasAttrs, HasVisibility, Macro, MacroKind,
    Module, ModuleDef, Visibility,
};
use hir_def::{
    item_scope::{ImportOrExternCrate, ImportOrGlob},
    per_ns::PerNs,
};

use crate::{
    TestOnly,
    model::{Code, ItemRef, Kind, Target},
};

/// A crate whose documentation is written.
#[derive(Debug, Clone)]
pub struct Documented {
    pub krate: Crate,
    /// The name code uses, the directory under `doc/`.
    pub name: String,
    /// Items not public are documented too.
    pub private: bool,
    /// Warnings about its docs are shown: a crate asked for or one from a path, not from a
    /// registry or git, whose lints cargo caps.
    pub warn: bool,
}

/// One item where a module page lists it.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    pub(crate) def: ModuleDef,
    pub(crate) kind: Kind,
    pub(crate) name: String,
    /// The modules from the crate root down to the one listing it.
    pub(crate) modules: Vec<String>,
    /// A module's items, as indices into the crate's placements.
    pub(crate) children: Vec<usize>,
    /// A module's `pub use` lines that are not inlined.
    pub(crate) reexports: Vec<Line>,
    /// The `use` that inlined the item, whose docs rustdoc puts before the item's own.
    pub(crate) via: Option<hir_def::UseId>,
}

/// A `pub use` or `pub extern crate` shown as itself.
#[derive(Debug, Clone)]
pub(crate) enum Line {
    Use { name: String, target: ModuleDef, glob: bool, use_id: hir_def::UseId },
    ExternCrate { name: String, krate: Crate },
}

pub(crate) struct Places {
    pub(crate) crates: Vec<Documented>,
    /// Each crate's placements; `[0]` is its root module.
    pub(crate) placed: Vec<Vec<Placed>>,
    canonical: HashMap<ModuleDef, (usize, usize)>,
    /// Each crate's own page of the items it places, inlined ones of other crates included.
    own: Vec<HashMap<ModuleDef, usize>>,
    /// The page of an item of a crate not documented here, found once: on doc.rust-lang.org for
    /// the sysroot's crates, under its `html_root_url` for a crate that has one.
    elsewhere: SharedMap<ModuleDef, Option<ItemRef>>,
    /// Each crate not documented here: its `html_root_url`.
    roots: SharedMap<Crate, Option<String>>,
    test_only: TestOnly,
}

impl Places {
    /// The page an item placed in crate `krate` at `at` is on.
    pub(crate) fn placed_ref(&self, krate: usize, at: usize) -> ItemRef {
        let placed = &self.placed[krate][at];
        ItemRef {
            krate: self.crates[krate].name.clone(),
            modules: placed.modules.clone(),
            kind: placed.kind,
            name: placed.name.clone(),
            anchor: None,
            descr: None,
            declared: None,
            remote: None,
        }
    }

    /// The places as the pages of crate `krate` link them.
    pub(crate) fn linking(&self, krate: usize) -> Linking<'_> {
        Linking { places: self, krate, subst: &[] }
    }
}

/// The places as one documented crate's pages link them, as rustdoc's cache of paths does: an
/// item the crate placed, inlined from another crate or not, on the crate's own page; another
/// documented crate's item on its page there, titled with the path it is declared at.
#[derive(Clone, Copy)]
pub(crate) struct Linking<'a> {
    places: &'a Places,
    krate: usize,
    /// Type parameters code shows as other code: an aliased type's as the alias's arguments.
    pub(crate) subst: &'a [(hir_def::TypeParamId, Code)],
}

impl std::ops::Deref for Linking<'_> {
    type Target = Places;
    fn deref(&self) -> &Places {
        self.places
    }
}

impl<'a> Linking<'a> {
    /// The same, with `subst`'s type parameters shown as its code.
    pub(crate) fn substituting<'b>(&self, subst: &'b [(hir_def::TypeParamId, Code)]) -> Linking<'b>
    where
        'a: 'b,
    {
        Linking { places: self.places, krate: self.krate, subst }
    }

    /// Where the documentation of `def` is, for a link to it: the page in the docs written, or
    /// on doc.rust-lang.org for `std` and the crates under it, or nowhere.
    pub(crate) fn target(&self, db: &Db, def: ModuleDef) -> Option<Target> {
        match def {
            ModuleDef::BuiltinType(builtin) => {
                Some(Target::Item(primitive(builtin.name().as_str())))
            }
            ModuleDef::EnumVariant(variant) => {
                let mut parent =
                    self.item_ref(db, ModuleDef::Adt(variant.parent_enum(db).into()))?;
                parent.anchor = Some(format!("variant.{}", variant.name(db).as_str()));
                parent.descr = Some("variant");
                Some(Target::Item(parent))
            }
            ModuleDef::Function(it) => match it.as_assoc_item(db) {
                Some(assoc) => self.assoc_target(db, assoc),
                None => self.item_ref(db, def).map(Target::Item),
            },
            ModuleDef::Const(it) => match it.as_assoc_item(db) {
                Some(assoc) => self.assoc_target(db, assoc),
                None => self.item_ref(db, def).map(Target::Item),
            },
            ModuleDef::TypeAlias(it) => match it.as_assoc_item(db) {
                Some(assoc) => self.assoc_target(db, assoc),
                None => self.item_ref(db, def).map(Target::Item),
            },
            _ => self.item_ref(db, def).map(Target::Item),
        }
    }

    pub(crate) fn field_target(&self, db: &Db, field: hir::Field) -> Option<Target> {
        let name = field.name(db);
        let mut parent = match field.parent_def(db) {
            hir::Variant::Struct(it) => self.item_ref(db, ModuleDef::Adt(it.into()))?,
            hir::Variant::Union(it) => self.item_ref(db, ModuleDef::Adt(it.into()))?,
            hir::Variant::EnumVariant(variant) => {
                let mut parent =
                    self.item_ref(db, ModuleDef::Adt(variant.parent_enum(db).into()))?;
                parent.anchor =
                    Some(format!("variant.{}.field.{}", variant.name(db).as_str(), name.as_str()));
                parent.descr = Some("field");
                return Some(Target::Item(parent));
            }
        };
        parent.anchor = Some(format!("structfield.{}", name.as_str()));
        parent.descr = Some("field");
        Some(Target::Item(parent))
    }

    pub(crate) fn assoc_target(&self, db: &Db, assoc: AssocItem) -> Option<Target> {
        let (mut parent, in_trait) = match assoc.container(db) {
            AssocItemContainer::Trait(it) => (self.item_ref(db, ModuleDef::Trait(it))?, true),
            AssocItemContainer::Impl(it) => {
                let ty = it.self_ty(db);
                let parent = if let Some(adt) = ty.as_adt() {
                    self.item_ref(db, ModuleDef::Adt(adt))?
                } else {
                    primitive(ty.as_builtin()?.name().as_str())
                };
                (parent, false)
            }
        };
        let name = assoc.name(db)?;
        let anchor = match assoc {
            AssocItem::Function(f) if in_trait && !f.has_body(db) => "tymethod",
            AssocItem::Function(_) => "method",
            AssocItem::Const(_) => "associatedconstant",
            AssocItem::TypeAlias(_) => "associatedtype",
        };
        parent.anchor = Some(format!("{anchor}.{}", name.as_str()));
        parent.descr = Some(match assoc {
            AssocItem::Function(f) if f.has_self_param(db) => "method",
            AssocItem::Function(_) => "associated function",
            AssocItem::Const(_) => "associated constant",
            AssocItem::TypeAlias(_) => "associated type",
        });
        Some(Target::Item(parent))
    }

    pub(crate) fn item_ref(&self, db: &Db, def: ModuleDef) -> Option<ItemRef> {
        if let Some(&at) = self.own[self.krate].get(&def) {
            return Some(self.placed_ref(self.krate, at));
        }
        if let Some(&(krate, at)) = self.canonical.get(&def) {
            let mut found = self.placed_ref(krate, at);
            found.declared = declared_path(db, def);
            return Some(found);
        }
        let krate = krate_of(db, def)?;
        if self.crates.iter().any(|it| it.krate == krate) {
            return None;
        }
        if let Some(found) = self.elsewhere.pin().get(&def) {
            return found.clone();
        }
        let found = match lang_crate_name(db, krate) {
            Some(name) => defined_ref(db, def, name),
            // rustdoc's `extern_location`: a crate with no docs here is linked where its
            // `html_root_url` says, else not at all.
            None => defined_ref(db, def, &crate::crate_name(db, krate)).map(|mut it| {
                it.remote = self.html_root_url(db, krate);
                it
            }),
        };
        self.elsewhere.pin().insert(def, found.clone());
        found
    }

    fn html_root_url(&self, db: &Db, krate: Crate) -> Option<String> {
        if let Some(found) = self.roots.pin().get(&krate) {
            return found.clone();
        }
        let found = match krate.root_module(db).definition_source_view(db).value {
            hir::ModuleSourceView::SourceFile(file) => crate::attrs::html_root_url(file),
            _ => None,
        };
        self.roots.pin().insert(krate, found.clone());
        found
    }
}

pub(crate) fn primitive(name: &str) -> ItemRef {
    ItemRef {
        krate: "std".to_owned(),
        modules: vec![],
        kind: Kind::Primitive,
        name: name.to_owned(),
        anchor: None,
        descr: None,
        declared: None,
        remote: None,
    }
}

/// `clap_builder::builder::command::Command`: the crate, the modules down to the one declaring
/// `def` and its name.
fn declared_path(db: &Db, def: ModuleDef) -> Option<String> {
    let mut parts = vec![def.name(db)?.as_str().to_owned()];
    let mut at = match def {
        ModuleDef::Macro(it) if is_macro_export(db, it) => {
            Some(it.module(db).krate(db).root_module(db))
        }
        _ => def.module(db),
    };
    while let Some(module) = at {
        match module.parent(db) {
            Some(parent) => {
                parts.push(module.name(db)?.as_str().to_owned());
                at = Some(parent);
            }
            None => {
                parts.push(crate::crate_name(db, module.krate(db)));
                at = None;
            }
        }
    }
    parts.reverse();
    Some(parts.join("::"))
}

/// The name of a crate doc.rust-lang.org documents, as its URLs have it.
pub(crate) fn lang_crate_name(db: &Db, krate: Crate) -> Option<&'static str> {
    match krate.origin(db) {
        base_db::CrateOrigin::Lang(lang) => match lang {
            LangCrateOrigin::Core => Some("core"),
            LangCrateOrigin::Alloc => Some("alloc"),
            LangCrateOrigin::Std => Some("std"),
            LangCrateOrigin::ProcMacro => Some("proc_macro"),
            LangCrateOrigin::Test => Some("test"),
            LangCrateOrigin::Dependency | LangCrateOrigin::Other => None,
        },
        _ => None,
    }
}

/// The page of an item of a crate not documented here where it is defined, as rustdoc links
/// it: under a private module, a page the crate's docs redirect to where it is exported.
/// `None` for an item the crate does not export, or only hidden.
fn defined_ref(db: &Db, def: ModuleDef, krate_name: &str) -> Option<ItemRef> {
    let kind = kind_of(db, def)?;
    if let ModuleDef::Module(it) = def
        && it.is_crate_root(db)
    {
        let krate = krate_name.to_owned();
        return Some(ItemRef {
            krate,
            modules: vec![],
            kind,
            name: String::new(),
            anchor: None,
            descr: None,
            declared: None,
            remote: None,
        });
    }
    let name = def.name(db)?.as_str().to_owned();
    if let ModuleDef::Macro(it) = def
        && is_macro_export(db, it)
    {
        let krate = krate_name.to_owned();
        return Some(ItemRef {
            krate,
            modules: vec![],
            kind,
            name,
            anchor: None,
            descr: None,
            declared: None,
            remote: None,
        });
    }
    // The module whose scope declares it; a module's parent for a module.
    let module = def.module(db)?;
    let mut modules = vec![];
    let mut reachable = def.visibility(db) == Visibility::Public && !def.attrs(db).is_doc_hidden();
    let mut at = Some(module);
    while let Some(m) = at {
        let Some(parent) = m.parent(db) else { break };
        if m.visibility(db) != Visibility::Public || m.attrs(db).is_doc_hidden() {
            reachable = false;
        }
        modules.push(m.name(db)?.as_str().to_owned());
        at = Some(parent);
    }
    modules.reverse();
    if !reachable && def.attrs(db).is_doc_hidden() {
        return None;
    }
    if !reachable {
        let root = module.krate(db).root_module(db);
        let config = hir::FindPathConfig {
            prefer_no_std: false,
            prefer_prelude: false,
            prefer_absolute: false,
            allow_unstable: true,
        };
        let path = root.find_path(db, def, config)?;
        let segments = path.segments();
        let (_, parents) = segments.split_last()?;
        let mut at = root;
        for segment in parents {
            if segment.as_str() == "prelude" {
                return None;
            }
            let Some(ModuleDef::Module(next)) =
                at.scope(db, None).into_iter().find_map(|(name, def)| match def {
                    hir::ScopeDef::ModuleDef(def) if name == *segment => Some(def),
                    _ => None,
                })
            else {
                return None;
            };
            if next.attrs(db).is_doc_hidden() {
                return None;
            }
            at = next;
        }
    }
    Some(ItemRef {
        krate: krate_name.to_owned(),
        modules,
        kind,
        name,
        anchor: None,
        descr: None,
        declared: None,
        remote: None,
    })
}

/// The kind of page rustdoc gives an item, or none for an item that has no page of its own.
pub(crate) fn kind_of(db: &Db, def: ModuleDef) -> Option<Kind> {
    Some(match def {
        ModuleDef::Module(_) => Kind::Module,
        ModuleDef::Function(_) => Kind::Function,
        ModuleDef::Adt(hir::Adt::Struct(_)) => Kind::Struct,
        ModuleDef::Adt(hir::Adt::Enum(_)) => Kind::Enum,
        ModuleDef::Adt(hir::Adt::Union(_)) => Kind::Union,
        ModuleDef::Const(_) => Kind::Constant,
        ModuleDef::Static(_) => Kind::Static,
        ModuleDef::Trait(it) => {
            if trait_is_alias(db, it) {
                Kind::TraitAlias
            } else {
                Kind::Trait
            }
        }
        ModuleDef::TypeAlias(_) => Kind::TypeAlias,
        ModuleDef::Macro(it) => macro_kind(db, it)?,
        ModuleDef::BuiltinType(_) => Kind::Primitive,
        ModuleDef::EnumVariant(_) => return None,
    })
}

pub(crate) fn trait_is_alias(db: &Db, it: hir::Trait) -> bool {
    let id: hir_def::TraitId = it.into();
    hir_def::signatures::TraitSignature::of(db, id)
        .flags
        .contains(hir_def::signatures::TraitFlags::ALIAS)
}

pub(crate) fn macro_kind(db: &Db, it: Macro) -> Option<Kind> {
    // The built-in ones are `core`'s, which documents them as it does any other.
    Some(match it.kind(db) {
        MacroKind::Declarative | MacroKind::ProcMacro | MacroKind::DeclarativeBuiltIn => {
            Kind::Macro
        }
        MacroKind::Derive | MacroKind::DeriveBuiltIn => Kind::ProcDerive,
        MacroKind::Attr | MacroKind::AttrBuiltIn => Kind::ProcAttribute,
    })
}

/// Places every item of every crate, in parallel over the crates.
pub(crate) fn place(db: &Db, crates: Vec<Documented>, test_only: TestOnly) -> Places {
    // A worker waiting on a query runs other jobs of the pool, which may be another database's:
    // each item puts back the database attached below it.
    let placed: Vec<Vec<Placed>> = base_db::par_map(crates.len(), |at| {
        hir::attach_db_allow_change(db, || Walker::new(db, &crates[at]).walk())
    });
    let mut canonical: HashMap<ModuleDef, ((u8, usize), (usize, usize))> = HashMap::default();
    let mut own: Vec<HashMap<ModuleDef, usize>> = vec![];
    for (krate, list) in placed.iter().enumerate() {
        let mut here: HashMap<ModuleDef, ((u8, usize), usize)> = HashMap::default();
        for (at, placed) in list.iter().enumerate() {
            let own = krate_of(db, placed.def) == Some(crates[krate].krate);
            let rank = match (own, placed.via.is_none()) {
                (true, true) => 0,
                (true, false) => 1,
                (false, _) => 2,
            };
            let key = (rank, placed.modules.len());
            canonical
                .entry(placed.def)
                .and_modify(|best| {
                    if key < best.0 {
                        *best = (key, (krate, at));
                    }
                })
                .or_insert((key, (krate, at)));
            here.entry(placed.def)
                .and_modify(|best| {
                    if key < best.0 {
                        *best = (key, at);
                    }
                })
                .or_insert((key, at));
        }
        own.push(here.into_iter().map(|(def, (_, at))| (def, at)).collect());
    }
    Places {
        crates,
        placed,
        canonical: canonical.into_iter().map(|(def, (_, at))| (def, at)).collect(),
        own,
        elsewhere: SharedMap::default(),
        roots: SharedMap::default(),
        test_only,
    }
}

struct Walker<'a> {
    db: &'a Db,
    krate: Crate,
    private: bool,
    out: Vec<Placed>,
    /// Modules being walked, so a module that re-exports its parent does not loop.
    walking: HashSet<Module>,
}

impl<'a> Walker<'a> {
    fn new(db: &'a Db, documented: &Documented) -> Self {
        Walker {
            db,
            krate: documented.krate,
            private: documented.private,
            out: vec![],
            walking: HashSet::default(),
        }
    }

    fn walk(mut self) -> Vec<Placed> {
        let root = self.krate.root_module(self.db);
        self.out.push(Placed {
            def: ModuleDef::Module(root),
            kind: Kind::Module,
            name: String::new(),
            modules: vec![],
            children: vec![],
            reexports: vec![],
            via: None,
        });
        self.module(root, 0, vec![], true);
        self.out
    }

    /// Whether an item declared in this crate is shown: public or documenting private items,
    /// and not `#[doc(hidden)]`.
    fn shown(&self, def: ModuleDef) -> bool {
        if def.attrs(self.db).is_doc_hidden() {
            return false;
        }
        self.private || def.visibility(self.db) == Visibility::Public
    }

    /// Whether `def`, declared in this crate, is documented where it is declared: shown, and in
    /// modules all shown down from the root.
    fn reachable_as_declared(&self, def: ModuleDef) -> bool {
        let db = self.db;
        if !self.shown(def) {
            return false;
        }
        if let ModuleDef::Macro(it) = def
            && is_macro_export(db, it)
        {
            return true;
        }
        // The module whose scope declares it; none for the crate root.
        let Some(mut module) = def.module(db) else { return true };
        loop {
            let Some(parent) = module.parent(db) else { return true };
            if !self.shown(ModuleDef::Module(module)) {
                return false;
            }
            module = parent;
        }
    }

    /// Lists the items of `module` under the placement `at`. `local` is false inside a module
    /// of another crate that is inlined, whose public items all are.
    fn module(&mut self, module: Module, at: usize, path: Vec<String>, local: bool) {
        if !self.walking.insert(module) {
            return;
        }
        let db = self.db;
        let id: hir_def::ModuleId = module.into();
        let scope = id.def_map(db).module(db, id).scope(db);
        let mut seen_globs = HashSet::default();
        let entries: Vec<(hir::Name, PerNs)> =
            scope.entries().map(|(name, per_ns)| (name.clone(), per_ns)).collect();
        for (name, per_ns) in entries {
            let mut defs: Vec<(ModuleDef, Option<ImportOrExternCrate>, bool)> = vec![];
            if let Some(item) = per_ns.types {
                defs.push((
                    item.def.into(),
                    item.import,
                    item.vis == hir_def::visibility::Visibility::Public,
                ));
            }
            if let Some(item) = per_ns.values {
                defs.push((
                    item.def.into(),
                    item.import.map(|it| match it {
                        ImportOrGlob::Glob(it) => ImportOrExternCrate::Glob(it),
                        ImportOrGlob::Import(it) => ImportOrExternCrate::Import(it),
                    }),
                    item.vis == hir_def::visibility::Visibility::Public,
                ));
            }
            if let Some(item) = per_ns.macros {
                defs.push((
                    ModuleDef::Macro(item.def.into()),
                    item.import,
                    item.vis == hir_def::visibility::Visibility::Public,
                ));
            }
            let mut done: HashSet<ModuleDef> = HashSet::default();
            for (def, import, public) in defs {
                if !done.insert(def) {
                    continue;
                }
                self.entry(module, at, &path, local, &name, def, import, public, &mut seen_globs);
            }
        }
        self.walking.remove(&module);
    }

    #[allow(clippy::too_many_arguments)]
    fn entry(
        &mut self,
        module: Module,
        at: usize,
        path: &[String],
        local: bool,
        name: &hir::Name,
        def: ModuleDef,
        import: Option<ImportOrExternCrate>,
        public: bool,
        seen_globs: &mut HashSet<hir_def::item_scope::GlobId>,
    ) {
        let db = self.db;
        if matches!(def, ModuleDef::BuiltinType(_) | ModuleDef::EnumVariant(_)) {
            return;
        }
        if let ModuleDef::Function(it) = def
            && it.as_proc_macro(db).is_some()
        {
            return;
        }
        let Some(kind) = kind_of(db, def) else { return };
        let def_krate = krate_of(db, def);
        let Some(import) = import else {
            // Declared here (or a `#[macro_export]` macro, which the crate root has).
            let declared_here = match def {
                ModuleDef::Macro(it) => {
                    it.module(db) == module || (module.is_crate_root(db) && is_macro_export(db, it))
                }
                _ => def.module(db) == Some(module),
            };
            if !declared_here || from_builtin_attr(db, def) {
                return;
            }
            let shown =
                if local { self.shown(def) } else { public && !def.attrs(db).is_doc_hidden() };
            if shown {
                self.put(at, path, def, kind, name, None, local);
            }
            return;
        };
        // rustdoc shows no `use` that is not public, private items documented or not.
        if !public {
            return;
        }
        match import {
            ImportOrExternCrate::ExternCrate(extern_crate) => {
                let decl: hir::ExternCrateDecl = extern_crate.into();
                if decl.attrs(db).is_doc_hidden() {
                    return;
                }
                if let ModuleDef::Module(m) = def
                    && m.is_crate_root(db)
                {
                    self.out[at].reexports.push(Line::ExternCrate {
                        name: name.as_str().to_owned(),
                        krate: m.krate(db),
                    });
                }
            }
            ImportOrExternCrate::Import(import) => {
                let use_id = import.use_;
                let use_attrs = UseAttrs::of(db, use_id);
                if use_attrs.hidden || def.attrs(db).is_doc_hidden() {
                    return;
                }
                let foreign = def_krate != Some(self.krate);
                let inline = if !local {
                    true
                } else if use_attrs.no_inline {
                    false
                } else if use_attrs.inline {
                    true
                } else if foreign {
                    !matches!(def, ModuleDef::Module(m) if m.is_crate_root(db))
                } else {
                    !self.reachable_as_declared(def) && !self.through_public_reexport(import)
                };
                if inline {
                    if foreign && !local_or_public(db, def) {
                        return;
                    }
                    self.put(at, path, def, kind, name, Some(use_id), local && !foreign);
                } else {
                    self.out[at].reexports.push(Line::Use {
                        name: name.as_str().to_owned(),
                        target: def,
                        glob: false,
                        use_id,
                    });
                }
            }
            ImportOrExternCrate::Glob(glob) => {
                let use_id = glob.use_;
                let use_attrs = UseAttrs::of(db, use_id);
                if use_attrs.hidden || def.attrs(db).is_doc_hidden() {
                    return;
                }
                let foreign = def_krate != Some(self.krate);
                let inline = !local
                    || use_attrs.inline
                    || (!use_attrs.no_inline && (foreign || !self.reachable_as_declared(def)));
                if inline {
                    if foreign && !local_or_public(db, def) {
                        return;
                    }
                    self.put(at, path, def, kind, name, Some(use_id), local && !foreign);
                } else if seen_globs.insert(glob) {
                    let target = def.module(db).map(ModuleDef::Module).unwrap_or(def);
                    self.out[at].reexports.push(Line::Use {
                        name: "*".to_owned(),
                        target,
                        glob: true,
                        use_id,
                    });
                }
            }
        }
    }

    /// Whether a `use` of this crate's item names it through another module's `pub use`, in a
    /// module documented where it is declared: rustdoc lists such a `use` rather than inline the
    /// item a second time (`maybe_inline_local`, its `reexport_chain` check).
    fn through_public_reexport(&self, import: hir_def::item_scope::ImportId) -> bool {
        use hir_def::Lookup;
        use syntax::view::{self as ast, AstNode};
        let db = self.db;
        let loc = import.use_.lookup(db);
        let tree = hir_def::src::use_tree_to_view(db, loc.id, import.idx);
        // `use a::{b::C}`: the leaf's path after those of the trees around it.
        let mut segments: Vec<&str> = vec![];
        for tree in tree.syntax().ancestors().filter_map(ast::UseTree::cast) {
            if let Some(path) = tree.path() {
                let mut outer: Vec<&str> = path.segments().map(|it| it.syntax().text()).collect();
                outer.append(&mut segments);
                segments = outer;
            }
        }
        let Some((&last, prefix)) = segments.split_last() else { return false };
        let here: Module = loc.container.into();
        let mut at = here;
        for (i, &segment) in prefix.iter().enumerate() {
            let next = match segment {
                "crate" if i == 0 => Some(here.krate(db).root_module(db)),
                "self" if i == 0 => Some(here),
                "super" => at.parent(db),
                name => at.scope(db, None).into_iter().find_map(|(it, def)| match def {
                    hir::ScopeDef::ModuleDef(ModuleDef::Module(m)) if it.as_str() == name => {
                        Some(m)
                    }
                    _ => None,
                }),
            };
            let Some(next) = next else { return false };
            at = next;
        }
        if at == here
            || at.krate(db) != self.krate
            || !self.reachable_as_declared(ModuleDef::Module(at))
        {
            return false;
        }
        let id: hir_def::ModuleId = at.into();
        let scope = id.def_map(db).module(db, id).scope(db);
        let public = hir_def::visibility::Visibility::Public;
        scope.entries().any(|(name, per_ns)| {
            name.as_str() == last
                && (per_ns.types.is_some_and(|it| it.import.is_some() && it.vis == public)
                    || per_ns.values.is_some_and(|it| it.import.is_some() && it.vis == public)
                    || per_ns.macros.is_some_and(|it| it.import.is_some() && it.vis == public))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        at: usize,
        path: &[String],
        def: ModuleDef,
        kind: Kind,
        name: &hir::Name,
        via: Option<hir_def::UseId>,
        local: bool,
    ) {
        let index = self.out.len();
        let name = name.as_str().to_owned();
        self.out.push(Placed {
            def,
            kind,
            name: name.clone(),
            modules: path.to_vec(),
            children: vec![],
            reexports: vec![],
            via,
        });
        self.out[at].children.push(index);
        if let ModuleDef::Module(module) = def {
            let mut inner = path.to_vec();
            inner.push(name);
            self.module(module, index, inner, local);
        }
    }
}

/// Whether a built-in attribute macro wrote the function: rust-analyzer's `#[global_allocator]`
/// writes functions beside the `static` where rustc's writes nothing rustdoc shows.
fn from_builtin_attr(db: &Db, def: ModuleDef) -> bool {
    use hir::view::HasSourceView;
    let ModuleDef::Function(it) = def else { return false };
    it.source_view(db)
        .and_then(|it| it.file_id.macro_file())
        .is_some_and(|call| call.kind(db) == hir_expand::MacroKind::AttrBuiltIn)
}

/// An item of another crate that rustdoc can inline: public there, not hidden.
fn local_or_public(db: &Db, def: ModuleDef) -> bool {
    def.visibility(db) == Visibility::Public && !def.attrs(db).is_doc_hidden()
}

pub(crate) fn is_macro_export(db: &Db, it: Macro) -> bool {
    let id: hir_def::MacroId = it.into();
    hir_def::attrs::AttrFlags::query(db, id.into())
        .contains(hir_def::attrs::AttrFlags::IS_MACRO_EXPORT)
}

/// What the attributes of a `use` item say about inlining.
#[derive(Default)]
struct UseAttrs {
    hidden: bool,
    inline: bool,
    no_inline: bool,
}

impl UseAttrs {
    fn of(db: &Db, id: hir_def::UseId) -> UseAttrs {
        use hir_def::{Lookup, src::HasSource};
        use syntax::view::HasAttrs as _;
        let flags = hir_def::attrs::AttrFlags::query(db, id.into());
        let mut out = UseAttrs {
            hidden: flags.contains(hir_def::attrs::AttrFlags::IS_DOC_HIDDEN),
            ..UseAttrs::default()
        };
        let source = id.lookup(db).source_view(db);
        for attr in source.value.attrs() {
            for word in crate::attrs::doc_words(attr) {
                match word.as_str() {
                    "inline" => out.inline = true,
                    "no_inline" => out.no_inline = true,
                    _ => {}
                }
            }
        }
        out
    }
}

impl Places {
    /// The crates a documented crate's docs may link to and list impls from: itself and every
    /// crate it depends on, directly or not, but for its tests alone.
    pub(crate) fn upstream(&self, db: &Db, krate: Crate) -> HashSet<Crate> {
        let mut seen = HashSet::default();
        let mut stack = vec![krate];
        while let Some(it) = stack.pop() {
            if seen.insert(it) {
                stack.extend(
                    it.dependencies(db)
                        .into_iter()
                        .map(|dep| dep.krate)
                        .filter(|dep| !self.test_only.contains(&(it, *dep))),
                );
            }
        }
        seen
    }

    /// The crates linked into `krate`: its dependencies, transitively, but not those for its
    /// tests alone nor those of a crate of procedural macros it uses, which rustc loads as the
    /// compiler's own. Of the sysroot's crates, which rust-analyzer makes dependencies of every
    /// crate, `core` is linked; `std` and `alloc` unless the crate is `#![no_std]`, `proc_macro`
    /// for a crate of procedural macros, and any of them the crate names with `extern crate`.
    pub(crate) fn linked(&self, db: &Db, krate: Crate) -> HashSet<Crate> {
        let mut seen = HashSet::default();
        let mut stack = vec![krate];
        while let Some(it) = stack.pop() {
            if !seen.insert(it) {
                continue;
            }
            let def_map = hir_def::nameres::crate_def_map(db, it.into());
            if it != krate && def_map.is_proc_macro() {
                continue;
            }
            let named: Vec<Crate> = it
                .root_module(db)
                .extern_crate_decls(db)
                .into_iter()
                .filter_map(|it| it.resolved_crate(db))
                .collect();
            for dep in it.dependencies(db) {
                if self.test_only.contains(&(it, dep.krate)) {
                    continue;
                }
                let linked = match lang_crate_name(db, dep.krate) {
                    Some("std" | "alloc") => !def_map.is_no_std(),
                    Some("proc_macro") => def_map.is_proc_macro(),
                    Some("test") => false,
                    _ => true,
                };
                if linked || named.contains(&dep.krate) {
                    stack.push(dep.krate);
                }
            }
        }
        seen
    }
}

/// The crate of what a type is: an ADT's, a `dyn Trait`'s trait's; `None` for a builtin type,
/// a reference or anything else whose docs are the standard library's.
pub(crate) fn type_home(db: &Db, ty: &hir::Type<'_>) -> Option<Crate> {
    if let Some(adt) = ty.as_adt() {
        return krate_of(db, ModuleDef::Adt(adt));
    }
    ty.as_dyn_trait().and_then(|it| krate_of(db, ModuleDef::Trait(it)))
}

/// The crate an item belongs to.
pub(crate) fn krate_of(db: &Db, def: ModuleDef) -> Option<Crate> {
    match def {
        ModuleDef::BuiltinType(_) => None,
        ModuleDef::Module(it) => Some(it.krate(db)),
        _ => def.module(db).map(|it| it.krate(db)),
    }
}
