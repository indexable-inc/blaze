//! The model of every crate documented: each placed item with its declaration, docs and links,
//! members and impls, built in parallel over the crates.

use std::cell::RefCell;

use base_db::Db;
use blaze_hash::{HashMap, HashSet};
use either::Either;
use hir::{
    AssocItem, Crate, HasAttrs, HasCrate, HasVisibility, Impl, InFile, ModuleDef, Visibility,
    view::HasSourceView,
};
use syntax::{
    SyntaxKind, T, TextRange, TextSize,
    view::{self as ast, AstNode},
};

use crate::{
    attrs,
    links::{self, Resolved},
    lint,
    model::{
        Code, CrateDoc, DerefMethods, Doc, ImplDoc, Impls, Item, ItemRef, Kind, Member, Model,
        Notes, Piece, Reexport, Span, Target, Warning,
    },
    places::{self, Line, Linking, Placed, Places, kind_of, primitive},
    sig::{self, FnContext},
    source::Files,
};

/// Builds the model of every crate placed.
pub(crate) fn build(db: &Db, places: &Places) -> Model {
    let files = Files::new(db, places);
    let std = StdItems::find(db, places);
    let by_trait_arg = impls_by_trait_arg(db, places);
    // As in `places::place`: a waiting worker may run another database's job above this one.
    let built: Vec<(CrateDoc, Vec<Warning>)> = base_db::par_map(places.crates.len(), |krate| {
        hir::attach_db_allow_change(db, || {
            let builder = Builder {
                db,
                places: places.linking(krate),
                files: &files,
                std: &std,
                by_trait_arg: &by_trait_arg,
                krate,
                allowed: places.upstream(db, places.crates[krate].krate),
                linked: places.linked(db, places.crates[krate].krate),
                warnings: RefCell::default(),
            };
            builder.build()
        })
    });
    let mut model = Model::default();
    let mut sources: Vec<_> = files.per_crate.into_iter().map(Some).collect();
    for (at, (mut doc, warnings)) in built.into_iter().enumerate() {
        doc.sources = sources[at].take().unwrap_or_default();
        model.crates.push(doc);
        model.warnings.extend(warnings);
    }
    model
}

/// Which members an impl's docs list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Members {
    /// The associated types and constants the impl writes, with no docs: an implementor on a
    /// trait's page.
    TypesAndConsts,
    /// The items the impl writes.
    Written,
    /// Those and the trait's provided items it leaves as they are.
    WithProvided,
}

/// Whether `imp` is a `#[rustc_reservation_impl]` (`impl<T> From<!> for T`), which no type has.
fn reservation(db: &Db, imp: Impl) -> bool {
    hir_def::ImplId::try_from(imp).is_ok_and(|id| {
        hir_def::attrs::AttrFlags::query(db, id.into())
            .contains(hir_def::attrs::AttrFlags::RUSTC_RESERVATION_IMPL)
    })
}

/// The trait impls of the crates documented that rustdoc shows on the page of a type other
/// than the one they are for: of each documented type the impl names as an argument of the
/// trait (`impl From<Local> for Foreign`), as the self type behind a reference, or inside a
/// `#[fundamental]` type (`impl Clone for Box<Local>`).
fn impls_by_trait_arg(db: &Db, places: &Places) -> HashMap<hir::Adt, Vec<Impl>> {
    let mut out: HashMap<hir::Adt, Vec<Impl>> = HashMap::default();
    for documented in &places.crates {
        for imp in Impl::all_in_crate(db, documented.krate) {
            let Some(trait_ref) = imp.trait_ref(db) else { continue };
            let self_ty = imp.self_ty(db);
            let behind_ref = self_ty.as_reference_inner().and_then(|it| it.as_adt());
            let own = self_ty.as_adt();
            // A `#[fundamental]` type's arguments (`Box<Local>`, `Pin<&mut Local>`) are what the
            // impl is for, as far as a page is concerned.
            let inside: Vec<hir::Adt> = match own {
                Some(adt) if fundamental(db, adt) => self_ty
                    .type_arguments()
                    .filter_map(|it| it.strip_references().as_adt())
                    .collect(),
                _ => vec![],
            };
            // A trait's arguments: the self type is the first, lifetimes and consts are none.
            let args = (1..16).filter_map(|at| trait_ref.get_type_argument(at));
            for adt in
                args.filter_map(|it| it.strip_references().as_adt()).chain(behind_ref).chain(inside)
            {
                if Some(adt) != own
                    && places
                        .crates
                        .iter()
                        .any(|it| Some(it.krate) == places::krate_of(db, ModuleDef::Adt(adt)))
                {
                    let list = out.entry(adt).or_default();
                    if !list.contains(&imp) {
                        list.push(imp);
                    }
                }
            }
        }
    }
    out
}

fn fundamental(db: &Db, adt: hir::Adt) -> bool {
    hir_def::attrs::AttrFlags::query(db, hir_def::AdtId::from(adt).into())
        .contains(hir_def::attrs::AttrFlags::FUNDAMENTAL)
}

/// What every crate's impls need from `core`: the auto traits, a type that is not each, and
/// the blanket impls of the sysroot and of every crate a crate documented links, documented
/// or not.
struct StdItems {
    auto: Vec<AutoTrait>,
    blankets: Vec<Impl>,
    deref: Option<hir::Trait>,
    deref_mut: Option<hir::Trait>,
    copy: Option<hir::Trait>,
    partial_eq: Option<hir::Trait>,
    structural_partial_eq: Option<hir::Trait>,
}

struct AutoTrait {
    trait_: hir::Trait,
    /// A type that does not implement the trait, and its argument when it has one.
    not: (hir::Adt, Option<hir::Adt>),
}

impl StdItems {
    fn find(db: &Db, places: &Places) -> StdItems {
        let core =
            Crate::all(db).into_iter().find(|it| places::lang_crate_name(db, *it) == Some("core"));
        let mut auto = vec![];
        let mut blankets = vec![];
        let trait_at = |path: &[&str]| match core.and_then(|core| lookup(db, core, path)) {
            Some(ModuleDef::Trait(it)) => Some(it),
            _ => None,
        };
        let (deref, deref_mut, copy) = (
            trait_at(&["ops", "Deref"]),
            trait_at(&["ops", "DerefMut"]),
            trait_at(&["marker", "Copy"]),
        );
        if let Some(core) = core {
            let adt = |path: &[&str]| match lookup(db, core, path) {
                Some(ModuleDef::Adt(it)) => Some(it),
                _ => None,
            };
            let non_null = adt(&["ptr", "NonNull"]);
            let unsafe_cell = adt(&["cell", "UnsafeCell"]);
            let pinned = adt(&["marker", "PhantomPinned"]);
            let unsafe_pinned = adt(&["pin", "UnsafePinned"]);
            let traits: [(&[&str], Option<(hir::Adt, Option<hir::Adt>)>); 7] = [
                (&["marker", "Freeze"], unsafe_cell.map(|it| (it, None))),
                (&["panic", "RefUnwindSafe"], unsafe_cell.map(|it| (it, None))),
                (&["marker", "Send"], non_null.map(|it| (it, None))),
                (&["marker", "Sync"], non_null.map(|it| (it, None))),
                (&["marker", "Unpin"], pinned.map(|it| (it, None))),
                (&["marker", "UnsafeUnpin"], unsafe_pinned.map(|it| (it, None))),
                (&["panic", "UnwindSafe"], non_null.map(|it| (it, unsafe_cell))),
            ];
            for (path, not) in traits {
                if let (Some(ModuleDef::Trait(trait_)), Some(not)) = (lookup(db, core, path), not) {
                    auto.push(AutoTrait { trait_, not });
                }
            }
            let mut upstream = HashSet::default();
            for documented in &places.crates {
                upstream.extend(places.upstream(db, documented.krate));
            }
            // Not the crates the sysroot's own depend on (its tests' `rand`).
            let crates = Crate::all(db).into_iter().filter(|it| match it.origin(db) {
                base_db::CrateOrigin::Lang(_) => {
                    matches!(places::lang_crate_name(db, *it), Some("core" | "alloc" | "std"))
                }
                _ => upstream.contains(it),
            });
            for krate in crates {
                for imp in Impl::all_in_crate(db, krate) {
                    if imp.trait_(db).is_some()
                        && imp.self_ty(db).as_type_param(db).is_some()
                        && !reservation(db, imp)
                    {
                        blankets.push(imp);
                    }
                }
            }
        }
        let (partial_eq, structural_partial_eq) =
            (trait_at(&["cmp", "PartialEq"]), trait_at(&["marker", "StructuralPartialEq"]));
        StdItems { auto, blankets, deref, deref_mut, copy, partial_eq, structural_partial_eq }
    }
}

/// The item at `path` from the root of `krate`, through modules then a name in the last.
fn lookup(db: &Db, krate: Crate, path: &[&str]) -> Option<ModuleDef> {
    let (last, modules) = path.split_last()?;
    let mut module = krate.root_module(db);
    for name in modules {
        module = module.children(db).find(|it| it.name(db).is_some_and(|n| n.as_str() == *name))?;
    }
    module.scope(db, None).into_iter().find_map(|(name, def)| match def {
        hir::ScopeDef::ModuleDef(def) if name.as_str() == *last => Some(def),
        _ => None,
    })
}

struct Builder<'a> {
    db: &'a Db,
    places: Linking<'a>,
    files: &'a Files,
    std: &'a StdItems,
    by_trait_arg: &'a HashMap<hir::Adt, Vec<Impl>>,
    krate: usize,
    /// Crates whose impls a type's page lists: the crate's own and its dependencies'. A crate
    /// downstream adds its impls to the pages of its traits, not of the types they are for.
    allowed: HashSet<Crate>,
    /// The crates linked into the crate, whose blanket impls apply to its types.
    linked: HashSet<Crate>,
    warnings: RefCell<Vec<Warning>>,
}

impl Builder<'_> {
    fn documented(&self) -> &places::Documented {
        &self.places.crates[self.krate]
    }

    fn build(self) -> (CrateDoc, Vec<Warning>) {
        let db = self.db;
        let placed = &self.places.placed[self.krate];
        let items: Vec<Item> = placed.iter().map(|it| self.item(it)).collect();
        let documented = self.documented();
        let doc = CrateDoc {
            name: documented.name.clone(),
            version: documented.krate.version(db),
            items,
            sources: vec![],
            private: documented.private,
        };
        (doc, self.warnings.into_inner())
    }

    fn item(&self, placed: &Placed) -> Item {
        let db = self.db;
        let def = placed.def;
        let private = self.documented().private;
        let mut item = Item {
            kind: placed.kind,
            name: placed.name.clone(),
            modules: placed.modules.clone(),
            docs: Doc::default(),
            decl: Code::default(),
            notes: Notes::default(),
            source: None,
            children: placed.children.clone(),
            reexports: vec![],
            fields: vec![],
            variants: vec![],
            aliased: None,
            trait_items: vec![],
            impls: Impls::default(),
            hidden_fields: false,
            dyn_compatible: None,
        };
        let (source, notes) = self.source_and_notes(def);
        item.source = source;
        item.notes = notes;
        item.docs = match def {
            ModuleDef::Module(it) => self.doc(it, placed.via),
            ModuleDef::Function(it) => self.doc(it, placed.via),
            ModuleDef::Adt(it) => self.doc(it, placed.via),
            ModuleDef::Const(it) => self.doc(it, placed.via),
            ModuleDef::Static(it) => self.doc(it, placed.via),
            ModuleDef::Trait(it) => self.doc(it, placed.via),
            ModuleDef::TypeAlias(it) => self.doc(it, placed.via),
            ModuleDef::Macro(it) => self.doc(it, placed.via),
            ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => Doc::default(),
        };
        let renamed = |mut code: Code| {
            // A re-export under another name documents the item under that name.
            for piece in &mut code.0 {
                if let Piece::Name(name, _) = piece
                    && def.name(db).is_some_and(|it| it.as_str() == name)
                    && *name != placed.name
                {
                    *name = placed.name.clone();
                    break;
                }
            }
            code
        };
        match def {
            ModuleDef::Module(_) => {
                item.reexports =
                    placed.reexports.iter().filter_map(|it| self.reexport(it)).collect();
            }
            ModuleDef::Function(it) => {
                item.decl =
                    renamed(sig::function(db, self.places, it, FnContext::Free, Kind::Function));
                item.notes.is_unsafe = it.is_unsafe_to_call(db, None, span::Edition::CURRENT);
            }
            ModuleDef::Adt(hir::Adt::Struct(it)) => {
                let (decl, hidden) = sig::struct_decl(db, self.places, it, private, None);
                item.decl = renamed(decl);
                item.hidden_fields = hidden;
                item.fields = self.fields(it.fields(db), self.places);
                item.impls = self.adt_impls(hir::Adt::Struct(it));
            }
            ModuleDef::Adt(hir::Adt::Union(it)) => {
                let (decl, hidden) = sig::union_decl(db, self.places, it, private, None);
                item.decl = renamed(decl);
                item.hidden_fields = hidden;
                item.fields = self.fields(it.fields(db), self.places);
                item.impls = self.adt_impls(hir::Adt::Union(it));
            }
            ModuleDef::Adt(hir::Adt::Enum(it)) => {
                item.decl = renamed(sig::enum_decl(db, self.places, it, None));
                item.variants = it
                    .variants(db)
                    .into_iter()
                    .filter(|v| !v.attrs(db).is_doc_hidden())
                    .map(|v| self.variant(v, self.places))
                    .collect();
                item.impls = self.adt_impls(hir::Adt::Enum(it));
            }
            ModuleDef::Trait(it) => {
                item.trait_items = it
                    .items(db)
                    .into_iter()
                    .filter(|assoc| !assoc_hidden(db, *assoc))
                    .filter_map(|assoc| self.assoc_member(assoc, FnContext::Trait, None, true))
                    .collect();
                item.decl = renamed(self.trait_decl(it, &item.trait_items));
                item.impls = self.trait_impls(it);
                item.dyn_compatible = Some(it.dyn_compatibility(db).is_none());
            }
            ModuleDef::TypeAlias(it) => {
                item.decl =
                    renamed(sig::type_alias(db, self.places, it, FnContext::Free, Kind::TypeAlias));
                if let Some(alias) = sig::aliased(db, self.places, it) {
                    let places = self.places.substituting(&alias.subst);
                    // Another crate's private fields stay private, whatever this crate shows.
                    let private = private && alias.adt.krate(db) == self.documented().krate;
                    match alias.adt {
                        hir::Adt::Struct(adt) => {
                            let (decl, hidden) =
                                sig::struct_decl(db, self.places, adt, private, Some(&alias));
                            item.aliased = Some(renamed(decl));
                            item.hidden_fields = hidden;
                            item.fields = self.fields(adt.fields(db), places);
                        }
                        hir::Adt::Union(adt) => {
                            let (decl, hidden) =
                                sig::union_decl(db, self.places, adt, private, Some(&alias));
                            item.aliased = Some(renamed(decl));
                            item.hidden_fields = hidden;
                            item.fields = self.fields(adt.fields(db), places);
                        }
                        hir::Adt::Enum(adt) => {
                            item.aliased =
                                Some(renamed(sig::enum_decl(db, self.places, adt, Some(&alias))));
                            item.variants = adt
                                .variants(db)
                                .into_iter()
                                .filter(|v| !v.attrs(db).is_doc_hidden())
                                .map(|v| self.variant(v, places))
                                .collect();
                        }
                    }
                }
            }
            ModuleDef::Const(it) => {
                item.decl =
                    renamed(sig::constant(db, self.places, it, FnContext::Free, Kind::Constant));
            }
            ModuleDef::Static(it) => {
                item.decl = renamed(sig::static_decl(db, self.places, it));
            }
            ModuleDef::Macro(it) => {
                item.decl = self.macro_decl(it, &placed.name);
            }
            ModuleDef::EnumVariant(_) | ModuleDef::BuiltinType(_) => {}
        }
        item
    }

    fn reexport(&self, line: &Line) -> Option<Reexport> {
        let db = self.db;
        let mut decl = Code::default();
        decl.kw("pub").text(" ");
        match line {
            Line::ExternCrate { name, krate } => {
                decl.kw("extern").text(" ").kw("crate").text(" ");
                let crate_name = krate
                    .display_name(db)
                    .map(|it| it.crate_name().to_string())
                    .unwrap_or_else(|| name.clone());
                match self.places.target(db, ModuleDef::Module(krate.root_module(db))) {
                    Some(to) => {
                        decl.push(Piece::Link { text: crate_name.clone(), kind: Kind::Module, to })
                    }
                    None => decl.push(Piece::Plain(crate_name.clone())),
                };
                if *name != crate_name {
                    decl.text(" ").kw("as").text(format!(" {name}"));
                }
                decl.text(";");
                Some(Reexport { decl, name: name.clone(), docs: Doc::default() })
            }
            Line::Use { name, target, glob, use_id } => {
                decl.kw("use").text(" ");
                let path = self.path_of(*target);
                let to = self.places.target(db, *target);
                let kind = match target {
                    ModuleDef::EnumVariant(_) => Kind::Variant,
                    _ => kind_of(db, *target).unwrap_or(Kind::Module),
                };
                let (parents, last) = match path.split_last() {
                    Some((last, parents)) => (parents.to_vec(), last.clone()),
                    None => (vec![], name.clone()),
                };
                for parent in &parents {
                    decl.push(Piece::Plain(parent.clone())).text("::");
                }
                match to {
                    Some(to) => decl.push(Piece::Link { text: last.clone(), kind, to }),
                    None => decl.push(Piece::Plain(last.clone())),
                };
                if *glob {
                    decl.text("::*");
                } else if *name != last {
                    decl.text(" ").kw("as").text(format!(" {name}"));
                }
                decl.text(";");
                let docs = hir_def::attrs::AttrFlags::docs(db, (*use_id).into())
                    .map(|it| Doc { markdown: it.docs().to_owned(), ..Doc::default() })
                    .unwrap_or_default();
                Some(Reexport { decl, name: name.clone(), docs })
            }
        }
    }

    /// The path code writes for an item: its crate's name then modules to it.
    fn path_of(&self, def: ModuleDef) -> Vec<String> {
        let db = self.db;
        if let Some(target) = self.places.item_ref(db, def) {
            let mut out = vec![target.krate.clone()];
            out.extend(target.modules.iter().cloned());
            if !target.name.is_empty() {
                out.push(target.name.clone());
            }
            return out;
        }
        let mut out = vec![];
        if let Some(name) = def.name(db) {
            out.push(name.as_str().to_owned());
        }
        let mut module = def.module(db);
        while let Some(m) = module {
            match m.name(db) {
                Some(name) => out.push(name.as_str().to_owned()),
                None => out.push(
                    m.krate(db)
                        .display_name(db)
                        .map(|it| it.crate_name().to_string())
                        .unwrap_or_default(),
                ),
            }
            module = m.parent(db);
        }
        out.reverse();
        out
    }

    /// The docs of `owner` with their intra-doc links resolved, after the docs of the `use`
    /// that inlined it.
    fn doc(&self, owner: impl HasAttrs + Copy + HasCrate, via: Option<hir_def::UseId>) -> Doc {
        let db = self.db;
        let mut out = Doc::default();
        let docs = owner.hir_docs(db);
        if let Some(use_id) = via
            && let Some(use_docs) = hir_def::attrs::AttrFlags::docs(db, use_id.into())
        {
            out.markdown.push_str(use_docs.docs());
            out.markdown.push_str("\n\n");
        }
        let Some(docs) = docs else { return out };
        let offset = out.markdown.len();
        out.markdown.push_str(docs.docs());
        let warn = self.documented().warn && owner.krate(db) == self.documented().krate;
        for link in links::markdown_links(&out.markdown) {
            let pp = match links::preprocess(&link) {
                None => continue,
                Some(Ok(pp)) => pp,
                Some(Err(message)) => {
                    if warn && link.range.start >= offset {
                        self.warn(
                            format!("unresolved link to `{}`: it {message}", link.link),
                            docs,
                            link.range.start - offset..link.range.end - offset,
                        );
                    }
                    continue;
                }
            };
            let range = if link.range.start >= offset {
                Some(link.range.start - offset..link.range.end - offset)
            } else {
                None
            };
            let is_inner = range
                .as_ref()
                .and_then(|range| docs.find_ast_range(text_range(range)))
                .map_or(hir::IsInnerDoc::No, |(_, inner)| inner);
            let target = match links::resolve(db, owner, &pp, is_inner) {
                Some(Resolved::Def(def)) => self.places.target(db, def),
                Some(Resolved::Field(field)) => self.places.field_target(db, field),
                Some(Resolved::Primitive(name, item)) => {
                    let mut target = primitive(name);
                    if let Some(item) = item {
                        let (kind, descr) = if item.starts_with(|c: char| c.is_ascii_uppercase()) {
                            ("associatedconstant", "associated constant")
                        } else {
                            ("method", "method")
                        };
                        target.anchor = Some(format!("{kind}.{item}"));
                        target.descr = Some(descr);
                    }
                    Some(Target::Item(target))
                }
                None => {
                    if warn && let Some(range) = range {
                        self.warn(format!("unresolved link to `{}`", pp.path), docs, range);
                    }
                    continue;
                }
            };
            if let Some(mut target) = target {
                if let (Some(fragment), Target::Item(ItemRef { anchor, descr, .. })) =
                    (&pp.fragment, &mut target)
                {
                    *anchor = Some(fragment.clone());
                    *descr = None;
                }
                out.links.insert(link.link.clone(), target);
            }
            if pp.text != link.link.replace('`', "") {
                out.texts.insert(link.link.clone(), pp.text.clone());
            }
        }
        out
    }

    fn warn(&self, message: String, docs: &hir::Docs, range: std::ops::Range<usize>) {
        let found = docs.find_ast_range(text_range(&range)).map(|(at, _)| at);
        if let Some(found) = found
            && !lint::broken_links_reported(self.db, self.documented().krate, found)
        {
            return;
        }
        let at = found.and_then(|at| self.files.location(self.db, at));
        self.warnings.borrow_mut().push(Warning {
            krate: self.documented().name.clone(),
            message,
            at,
        });
    }

    /// Where `def` is in the sources, and what its attributes note.
    fn source_and_notes(&self, def: ModuleDef) -> (Option<Span>, Notes) {
        let db = self.db;
        let mut notes = Notes::default();
        let node: Option<InFile<ast::AnyHasAttrs<'_>>> = match def {
            ModuleDef::Module(it) => {
                it.declaration_source_view(db).map(|src| src.map(ast::AnyHasAttrs::new))
            }
            ModuleDef::Function(it) => it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
            ModuleDef::Adt(hir::Adt::Struct(it)) => {
                it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new))
            }
            ModuleDef::Adt(hir::Adt::Enum(it)) => {
                it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new))
            }
            ModuleDef::Adt(hir::Adt::Union(it)) => {
                it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new))
            }
            ModuleDef::Const(it) => it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
            ModuleDef::Static(it) => it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
            ModuleDef::Trait(it) => it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
            ModuleDef::TypeAlias(it) => {
                it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new))
            }
            ModuleDef::Macro(it) => it.source_view(db).map(|s| {
                s.map(|it| match it {
                    Either::Left(mac) => ast::AnyHasAttrs::new(mac),
                    Either::Right(func) => ast::AnyHasAttrs::new(func),
                })
            }),
            ModuleDef::EnumVariant(it) => {
                it.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new))
            }
            ModuleDef::BuiltinType(_) => None,
        };
        let span = match (def, node) {
            // A module's lines are its file's, or its inline body's.
            (ModuleDef::Module(it), _) => {
                let src = it.definition_source_view(db);
                let syntax = match src.value {
                    hir::ModuleSourceView::SourceFile(file) => file.syntax(),
                    hir::ModuleSourceView::Module(module) => module.syntax(),
                    hir::ModuleSourceView::BlockExpr(block) => block.syntax(),
                };
                self.files.span(db, src.with_value(syntax))
            }
            (_, Some(node)) => self.files.span(db, node.map(|it| it.syntax())),
            _ => None,
        };
        if let Some(node) = node {
            let flags = def.attrs(db);
            if flags.is_deprecated() {
                notes.deprecated =
                    attrs::deprecation(node.value).or_else(|| Some(Default::default()));
            }
            notes.cfg = attrs::doc_cfg(node.value);
        }
        notes.must_use = must_use(db, def);
        (span, notes)
    }

    fn member_notes(
        &self,
        node: Option<InFile<ast::AnyHasAttrs<'_>>>,
        deprecated: bool,
    ) -> (Option<Span>, Notes) {
        let mut notes = Notes::default();
        let Some(node) = node else { return (None, notes) };
        let span = self.files.span(self.db, node.map(|it| it.syntax()));
        if deprecated {
            notes.deprecated = attrs::deprecation(node.value).or_else(|| Some(Default::default()));
        }
        notes.cfg = attrs::doc_cfg(node.value);
        (span, notes)
    }

    fn fields(&self, fields: Vec<hir::Field>, places: Linking<'_>) -> Vec<Member> {
        let db = self.db;
        let private = self.documented().private;
        fields
            .into_iter()
            .filter(|field| {
                (private || field.visibility(db) == Visibility::Public)
                    && !field.attrs(db).is_doc_hidden()
            })
            .map(|field| self.field(field, places))
            .collect()
    }

    fn field(&self, field: hir::Field, places: Linking<'_>) -> Member {
        let db = self.db;
        let name = field.name(db).as_str().to_owned();
        let mut decl = Code::default();
        decl.push(Piece::Name(name.clone(), Kind::StructField)).text(": ");
        decl.extend(sig::field_type(db, places, field));
        let node = field.source_view(db).map(|src| {
            src.map(|it| match it {
                hir::FieldSourceView::Named(it) => ast::AnyHasAttrs::new(it),
                hir::FieldSourceView::Pos(it) => ast::AnyHasAttrs::new(it),
            })
        });
        let (source, notes) = self.member_notes(node, field.attrs(db).is_deprecated());
        Member {
            kind: Kind::StructField,
            name,
            decl,
            docs: self.doc_field(field),
            notes,
            source,
            fields: vec![],
            docs_from_trait: false,
            provided: false,
        }
    }

    fn doc_field(&self, field: hir::Field) -> Doc {
        let db = self.db;
        let Some(docs) = field.hir_docs(db) else { return Doc::default() };
        let mut out = Doc { markdown: docs.docs().to_owned(), ..Doc::default() };
        let warn = self.documented().warn && field.krate(db) == self.documented().krate;
        for link in links::markdown_links(&out.markdown) {
            let Some(Ok(pp)) = links::preprocess(&link) else { continue };
            let is_inner = hir::IsInnerDoc::No;
            let target = match links::resolve(db, field, &pp, is_inner) {
                Some(Resolved::Def(def)) => self.places.target(db, def),
                Some(Resolved::Field(field)) => self.places.field_target(db, field),
                Some(Resolved::Primitive(name, _)) => Some(Target::Item(primitive(name))),
                None => {
                    if warn {
                        self.warn(
                            format!("unresolved link to `{}`", pp.path),
                            docs,
                            link.range.clone(),
                        );
                    }
                    continue;
                }
            };
            if let Some(target) = target {
                out.links.insert(link.link.clone(), target);
            }
            if pp.text != link.link.replace('`', "") {
                out.texts.insert(link.link.clone(), pp.text.clone());
            }
        }
        out
    }

    fn variant(&self, variant: hir::EnumVariant, places: Linking<'_>) -> Member {
        let db = self.db;
        let node = variant.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new));
        let (source, notes) = self.member_notes(node, variant.attrs(db).is_deprecated());
        let fields = variant.fields(db);
        let named = variant.kind(db) == hir::StructKind::Record;
        let fields: Vec<Member> = fields
            .into_iter()
            .filter(|f| !f.attrs(db).is_doc_hidden())
            .map(|f| self.field(f, places))
            .filter(|f| named || !f.docs.is_empty())
            .collect();
        Member {
            kind: Kind::Variant,
            name: variant.name(db).as_str().to_owned(),
            decl: sig::variant(db, places, variant),
            docs: self.doc(variant, None),
            notes,
            source,
            fields,
            docs_from_trait: false,
            provided: false,
        }
    }

    /// An associated item as its trait or impl lists it. `trait_item` is the trait's item an
    /// impl's implements, whose docs show when the impl's has none.
    fn assoc_member(
        &self,
        assoc: AssocItem,
        context: FnContext,
        trait_item: Option<AssocItem>,
        in_trait: bool,
    ) -> Option<Member> {
        let db = self.db;
        let name = assoc.name(db)?.as_str().to_owned();
        let (kind, decl, node, provided) = match assoc {
            AssocItem::Function(f) => {
                let kind = if in_trait && !f.has_body(db) { Kind::TyMethod } else { Kind::Method };
                let decl = sig::function(db, self.places, f, context, kind);
                (
                    kind,
                    decl,
                    f.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
                    in_trait && f.has_body(db),
                )
            }
            AssocItem::Const(c) => {
                let decl = sig::constant(db, self.places, c, context, Kind::AssocConst);
                (
                    Kind::AssocConst,
                    decl,
                    c.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
                    in_trait && c.has_body(db),
                )
            }
            AssocItem::TypeAlias(t) => {
                let decl = sig::type_alias(db, self.places, t, context, Kind::AssocType);
                (
                    Kind::AssocType,
                    decl,
                    t.source_view(db).map(|s| s.map(ast::AnyHasAttrs::new)),
                    in_trait && t.has_type(db),
                )
            }
        };
        let deprecated = match assoc {
            AssocItem::Function(it) => it.attrs(db).is_deprecated(),
            AssocItem::Const(it) => it.attrs(db).is_deprecated(),
            AssocItem::TypeAlias(it) => it.attrs(db).is_deprecated(),
        };
        let (source, mut notes) = self.member_notes(node, deprecated);
        notes.must_use = must_use(db, assoc_def(assoc));
        if let AssocItem::Function(f) = assoc {
            notes.is_unsafe = f.is_unsafe_to_call(db, None, span::Edition::CURRENT);
        }
        let mut docs = self.assoc_doc(assoc);
        let mut docs_from_trait = false;
        if docs.is_empty()
            && let Some(trait_item) = trait_item
        {
            docs = self.assoc_doc(trait_item);
            docs_from_trait = !docs.is_empty();
        }
        Some(Member {
            kind,
            name,
            decl,
            docs,
            notes,
            source,
            fields: vec![],
            docs_from_trait,
            provided,
        })
    }

    fn assoc_doc(&self, assoc: AssocItem) -> Doc {
        match assoc {
            AssocItem::Function(it) => self.doc(it, None),
            AssocItem::Const(it) => self.doc(it, None),
            AssocItem::TypeAlias(it) => self.doc(it, None),
        }
    }

    fn trait_decl(&self, it: hir::Trait, items: &[Member]) -> Code {
        let db = self.db;
        let mut out = Code::default();
        let where_clause = sig::trait_header(db, self.places, it, &mut out);
        let has_where = !where_clause.is_empty();
        out.extend(where_clause);
        if places::trait_is_alias(db, it) {
            out.text(";");
            return out;
        }
        if items.is_empty() {
            out.text(if has_where { "\n{}" } else { " {}" });
            return out;
        }
        out.text(if has_where { "\n{\n" } else { " {\n" });
        let groups: [(&str, &str, fn(&Member) -> bool); 5] = [
            (
                "Required associated type",
                "Required associated types",
                |m: &Member| m.kind == Kind::AssocType && !m.provided,
            ),
            (
                "Provided associated type",
                "Provided associated types",
                |m: &Member| m.kind == Kind::AssocType && m.provided,
            ),
            ("Associated constant", "Associated constants", |m: &Member| m.kind == Kind::AssocConst),
            ("Required method", "Required methods", |m: &Member| m.kind == Kind::TyMethod),
            ("Provided method", "Provided methods", |m: &Member| m.kind == Kind::Method),
        ];
        let mut first = true;
        for (one, many, wanted) in groups {
            let group: Vec<&Member> = items.iter().filter(|m| wanted(m)).collect();
            if group.is_empty() {
                continue;
            }
            if !first {
                out.text("\n");
            }
            first = false;
            out.text("    ")
                .push(Piece::Comment(format!("// {}", if group.len() == 1 { one } else { many })));
            out.text("\n");
            for member in group {
                out.text("    ");
                let last = member.decl.0.len().saturating_sub(1);
                for (at, piece) in member.decl.0.iter().enumerate() {
                    match piece {
                        Piece::Text(text) => {
                            // A where clause's last comma gives way to the `;` or `{ ... }`.
                            let text = if at == last {
                                text.strip_suffix(',').unwrap_or(text)
                            } else {
                                text
                            };
                            out.text(text.replace('\n', "\n    "));
                        }
                        other => {
                            out.push(other.clone());
                        }
                    }
                }
                out.text(if member.kind == Kind::Method { " { ... }\n" } else { ";\n" });
            }
        }
        out.text("}");
        out
    }

    /// Whether an impl is shown: not hidden, of a trait the docs can show.
    fn impl_shown(&self, imp: Impl) -> bool {
        let db = self.db;
        if !self.allowed.contains(&imp.module(db).krate(db)) {
            return false;
        }
        if let Ok(id) = hir_def::ImplId::try_from(imp)
            && hir_def::attrs::AttrFlags::query(db, id.into())
                .contains(hir_def::attrs::AttrFlags::IS_DOC_HIDDEN)
        {
            return false;
        }
        if let Some(trait_) = imp.trait_(db) {
            if trait_.attrs(db).is_doc_hidden() {
                return false;
            }
            // A trait of this crate that is not documented, or of another that is not public.
            let local =
                places::krate_of(db, ModuleDef::Trait(trait_)) == Some(self.documented().krate);
            if local {
                if self.places.item_ref(db, ModuleDef::Trait(trait_)).is_none()
                    && !self.documented().private
                {
                    return false;
                }
            } else if trait_.visibility(db) != Visibility::Public {
                return false;
            }
        }
        true
    }

    fn impl_doc(&self, imp: Impl, members: Members) -> ImplDoc {
        let db = self.db;
        let header = sig::impl_header(db, self.places, imp);
        let trait_ = imp.trait_(db);
        let private = self.documented().private;
        let mut items = vec![];
        let own = imp.items(db);
        match trait_ {
            None => {
                for assoc in own {
                    let shown = (private || assoc_visibility(db, assoc) == Visibility::Public)
                        && !assoc_hidden(db, assoc);
                    if shown
                        && let Some(member) = self.assoc_member(assoc, FnContext::Free, None, false)
                    {
                        items.push(member);
                    }
                }
            }
            Some(trait_) => {
                let trait_items = trait_.items(db);
                let mut implemented = HashSet::default();
                for assoc in own {
                    let name = assoc.name(db);
                    let trait_item = trait_items
                        .iter()
                        .copied()
                        .find(|it| it.name(db) == name && same_kind(*it, assoc));
                    if let Some(name) = name {
                        implemented.insert(name);
                    }
                    // An item the trait hides is hidden in every impl (serde's `deserialize_in_place`).
                    if assoc_hidden(db, assoc) || trait_item.is_some_and(|it| assoc_hidden(db, it))
                    {
                        continue;
                    }
                    if members == Members::TypesAndConsts {
                        if let AssocItem::TypeAlias(_) | AssocItem::Const(_) = assoc
                            && let Some(mut member) =
                                self.assoc_member(assoc, FnContext::Trait, None, false)
                        {
                            member.docs = Doc::default();
                            items.push(member);
                        }
                        continue;
                    }
                    if let Some(member) =
                        self.assoc_member(assoc, FnContext::Trait, trait_item, false)
                    {
                        items.push(member);
                    }
                }
                // The trait's provided items the impl leaves as they are.
                for assoc in trait_items {
                    if members != Members::WithProvided {
                        break;
                    }
                    let Some(name) = assoc.name(db) else { continue };
                    if implemented.contains(&name) || assoc_hidden(db, assoc) {
                        continue;
                    }
                    if let AssocItem::Function(f) = assoc
                        && !self.possible(imp, f)
                    {
                        continue;
                    }
                    let provided = match assoc {
                        AssocItem::Function(f) => f.has_body(db),
                        AssocItem::Const(c) => c.has_body(db),
                        AssocItem::TypeAlias(t) => t.has_type(db),
                    };
                    if !provided {
                        continue;
                    }
                    if let Some(mut member) =
                        self.assoc_member(assoc, FnContext::Trait, None, false)
                    {
                        member.docs_from_trait = true;
                        member.provided = true;
                        items.push(member);
                    }
                }
            }
        }
        let source = match hir_def::ImplId::try_from(imp) {
            Ok(_) => imp
                .source_view(db)
                .and_then(|src| self.files.span(db, src.map(|it| it.syntax()))),
            Err(_) => None,
        };
        let docs = self.doc(imp, None);
        let anchor = impl_anchor(&header);
        ImplDoc {
            header,
            anchor,
            trait_name: trait_.map(|it| it.name(db).as_str().to_owned()),
            items,
            docs,
            source,
            negative: imp.is_negative(db),
            synthetic: false,
        }
    }

    fn adt_impls(&self, adt: hir::Adt) -> Impls {
        let db = self.db;
        let mut out = Impls::default();
        let ty = adt.ty(db);
        let mut seen = HashSet::default();
        let by_trait_arg = self.by_trait_arg.get(&adt).into_iter().flatten().copied();
        for imp in Impl::all_for_type(db, ty).into_iter().chain(by_trait_arg) {
            if !seen.insert(imp) || !self.impl_shown(imp) {
                continue;
            }
            if imp.trait_(db).is_none() {
                let doc = self.impl_doc(imp, Members::WithProvided);
                if !doc.items.is_empty() || !doc.docs.is_empty() {
                    out.inherent.push(doc);
                }
            } else {
                if let Some(doc) = self.structural_partial_eq(imp, adt) {
                    out.traits.push(doc);
                }
                out.traits.push(self.impl_doc(imp, Members::WithProvided));
            }
        }
        out.traits.sort_by(|a, b| a.header.cmp_plain(&b.header));
        out.auto = self.auto_impls(adt);
        out.blanket = self.blanket_impls(adt);
        out.deref = self.deref_methods(adt);
        out
    }

    /// The `impl StructuralPartialEq` rustc's `#[derive(PartialEq)]` writes beside `imp`, which
    /// rust-analyzer's expansion leaves out.
    fn structural_partial_eq(&self, imp: Impl, adt: hir::Adt) -> Option<ImplDoc> {
        let db = self.db;
        let (partial_eq, structural) = (self.std.partial_eq?, self.std.structural_partial_eq?);
        if imp.trait_(db) != Some(partial_eq) || !sig::is_builtin_derive(db, imp) {
            return None;
        }
        let header = sig::derive_header(db, self.places, structural, partial_eq, adt);
        Some(ImplDoc {
            anchor: impl_anchor(&header),
            header,
            trait_name: Some("StructuralPartialEq".to_owned()),
            items: vec![],
            docs: Doc::default(),
            source: None,
            negative: false,
            synthetic: false,
        })
    }

    /// rustdoc's "Methods from Deref<Target = ..>": for a type that derefs, the methods of the
    /// target's inherent impls a `&self` reaches (`&mut self` too when it derefs mutably, `self`
    /// when the target is `Copy`), then those of the target's own target.
    fn deref_methods(&self, adt: hir::Adt) -> Vec<DerefMethods> {
        let db = self.db;
        let (Some(deref), deref_mut) = (self.std.deref, self.std.deref_mut) else { return vec![] };
        let private = self.documented().private;
        let mut out = vec![];
        let mut seen = HashSet::default();
        let mut ty = adt.ty(db);
        let mut own = Some(adt);
        while let Some(adt) = own
            && seen.insert(adt)
        {
            let impls = Impl::all_for_type(db, ty.clone());
            let of = |trait_: Option<hir::Trait>| {
                impls.iter().copied().find(|imp| {
                    imp.trait_(db) == trait_ && trait_.is_some() && !imp.is_negative(db)
                })
            };
            let Some(deref_impl) = of(Some(deref)) else { break };
            let mutable = of(deref_mut).is_some();
            let Some(alias) = deref_impl.items(db).into_iter().find_map(|it| match it {
                AssocItem::TypeAlias(it) => Some(it),
                _ => None,
            }) else {
                break;
            };
            let target = alias.ty(db);
            if target.as_type_param(db).is_some() || target.as_adt() == Some(adt) {
                break;
            }
            let Some(target_code) = sig::alias_type(db, self.places, alias) else { break };
            let copy = self.std.copy.is_some_and(|it| target.impls_trait(db, it, &[]));
            let mut members = vec![];
            for imp in Impl::all_for_type(db, target.clone()) {
                if imp.trait_(db).is_some()
                    || !self.impl_shown(imp)
                    || !target.unifies_with_any_params(db, &imp.self_ty(db))
                {
                    continue;
                }
                for assoc in imp.items(db) {
                    let AssocItem::Function(f) = assoc else { continue };
                    let Some(receiver) = f.self_param(db) else { continue };
                    let reached = match receiver.access(db) {
                        hir::Access::Shared => true,
                        hir::Access::Exclusive => mutable,
                        // `self` needs a `Copy` target; `self: Rc<Self>` is reached, `self: Box<Self>` not.
                        hir::Access::Owned => match receiver
                            .ty(db)
                            .as_adt()
                            .filter(|it| Some(*it) != imp.self_ty(db).as_adt())
                        {
                            Some(wrapper) => wrapper.name(db).as_str() != "Box",
                            None => copy,
                        },
                    };
                    let shown = (private || assoc_visibility(db, assoc) == Visibility::Public)
                        && !assoc_hidden(db, assoc);
                    if reached
                        && shown
                        && let Some(member) = self.assoc_member(assoc, FnContext::Free, None, false)
                    {
                        members.push(member);
                    }
                }
            }
            if !members.is_empty() {
                let trait_name = deref.name(db).as_str().to_owned();
                let mut trait_code = Code::default();
                match self.places.target(db, ModuleDef::Trait(deref)) {
                    Some(to) => {
                        trait_code.push(Piece::Link { text: trait_name, kind: Kind::Trait, to })
                    }
                    None => trait_code.push(Piece::Plain(trait_name)),
                };
                let anchor = small_url_encode(&format!("deref-methods-{}", target_code.plain()));
                out.push(DerefMethods { trait_: trait_code, target: target_code, anchor, members });
            }
            own = target.as_adt();
            ty = target;
        }
        out
    }

    /// The auto trait impls rustdoc works out: for each auto trait, whether the type has it
    /// when every type parameter does, and which parameters it needs it of.
    fn auto_impls(&self, adt: hir::Adt) -> Vec<ImplDoc> {
        let db = self.db;
        let adt_params = sig::adt_params(db, self.places, adt);
        let params = &adt_params.types;
        let ty = adt.ty(db);
        let good = hir::BuiltinType::i32().ty(db);
        let mut out = vec![];
        for auto in &self.std.auto {
            let not_ty = {
                let base = auto.not.0.ty(db);
                match auto.not.1 {
                    Some(inner) => base.instantiate(db, [inner.ty(db).instantiate(db, [good])]),
                    None => base.instantiate(db, [good]),
                }
            };
            let with = |bad: Option<usize>| {
                let args = (0..params.len()).map(|at| if Some(at) == bad { not_ty } else { good });
                ty.instantiate_type_params(db, args).impls_trait(db, auto.trait_, &[])
            };
            let holds = with(None);
            let needs: Vec<&String> = if holds {
                params
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| !with(Some(*at)))
                    .map(|(_, it)| it)
                    .collect()
            } else {
                vec![]
            };
            let trait_name = auto.trait_.name(db).as_str().to_owned();
            let mut header = Code::default();
            header.kw("impl");
            if !adt_params.decls.is_empty() {
                header.text("<");
                for (at, decl) in adt_params.decls.iter().enumerate() {
                    if at > 0 {
                        header.text(", ");
                    }
                    header.extend(decl.clone());
                }
                header.text(">");
            }
            header.text(" ");
            if !holds {
                header.text("!");
            }
            match self.places.target(db, ModuleDef::Trait(auto.trait_)) {
                Some(to) => {
                    header.push(Piece::Link { text: trait_name.clone(), kind: Kind::Trait, to })
                }
                None => header.push(Piece::Plain(trait_name.clone())),
            };
            header.text(" ").kw("for").text(" ");
            let name = adt.name(db).as_str().to_owned();
            match self.places.target(db, ModuleDef::Adt(adt)) {
                Some(to) => header.push(Piece::Link { text: name, kind: sig::adt_kind(adt), to }),
                None => header.push(Piece::Plain(name)),
            };
            if !adt_params.args.is_empty() {
                header.text(format!("<{}>", adt_params.args.join(", ")));
            }
            if !needs.is_empty() {
                header.text("\n").kw("where");
                for param in needs {
                    header.text(format!("\n    {param}: "));
                    match self.places.target(db, ModuleDef::Trait(auto.trait_)) {
                        Some(to) => header.push(Piece::Link {
                            text: trait_name.clone(),
                            kind: Kind::Trait,
                            to,
                        }),
                        None => header.push(Piece::Plain(trait_name.clone())),
                    };
                    header.text(",");
                }
            }
            let anchor = impl_anchor(&header);
            out.push(ImplDoc {
                header,
                anchor,
                trait_name: Some(trait_name),
                items: vec![],
                docs: Doc::default(),
                source: None,
                negative: !holds,
                synthetic: true,
            });
        }
        out
    }

    /// The blanket impls (`impl<T: Clone> ToOwned for T`) that apply to the type.
    fn blanket_impls(&self, adt: hir::Adt) -> Vec<ImplDoc> {
        let db = self.db;
        let ty = adt.ty(db);
        let mut out: Vec<ImplDoc> = vec![];
        for &imp in &self.std.blankets {
            if !self.impl_shown_blanket(imp) {
                continue;
            }
            if !blanket_applies(db, imp, &ty, self.documented().krate) {
                continue;
            }
            out.push(self.impl_doc(imp, Members::WithProvided));
        }
        out.sort_by(|a, b| a.trait_name.cmp(&b.trait_name));
        out
    }

    fn impl_shown_blanket(&self, imp: Impl) -> bool {
        let db = self.db;
        let Some(trait_) = imp.trait_(db) else { return false };
        if trait_.attrs(db).is_doc_hidden() || trait_.visibility(db) != Visibility::Public {
            return false;
        }
        self.linked.contains(&imp.module(db).krate(db))
            && self.places.item_ref(db, ModuleDef::Trait(trait_)).is_some()
    }

    fn trait_impls(&self, trait_: hir::Trait) -> Impls {
        let db = self.db;
        let mut out = Impls::default();
        let mut allowed = self.places.upstream(
            db,
            places::krate_of(db, ModuleDef::Trait(trait_)).unwrap_or(self.documented().krate),
        );
        allowed.extend(self.places.crates.iter().map(|it| it.krate));
        for imp in Impl::all_for_trait(db, trait_) {
            if !allowed.contains(&imp.module(db).krate(db)) {
                continue;
            }
            if let Ok(id) = hir_def::ImplId::try_from(imp)
                && hir_def::attrs::AttrFlags::query(db, id.into())
                    .contains(hir_def::attrs::AttrFlags::IS_DOC_HIDDEN)
            {
                continue;
            }
            if reservation(db, imp) || (!self.documented().private && self.on_stripped_type(imp)) {
                continue;
            }
            if imp.self_ty(db).as_type_param(db).is_some() {
                out.blanket_implementors.push(self.impl_doc(imp, Members::TypesAndConsts));
            } else if self.on_foreign_type(imp, trait_) {
                out.foreign.push(self.impl_doc(imp, Members::Written));
            } else {
                out.implementors.push(self.impl_doc(imp, Members::TypesAndConsts));
            }
        }
        out.implementors.sort_by(|a, b| a.header.cmp_plain(&b.header));
        out.foreign.sort_by(|a, b| a.header.cmp_plain(&b.header));
        out
    }

    /// Whether the impl can meet the where-clauses a provided method `f` puts on the trait's
    /// parameters; rustdoc leaves out a method it cannot, such as `rev` in the `Iterator` impl
    /// of a type that is not a `DoubleEndedIterator`, or `cmp` of one whose items are a type of
    /// the documented crate that is not `Ord`.
    fn possible(&self, imp: Impl, f: hir::Function) -> bool {
        !f.is_impossible_in(self.db, imp, self.documented().krate)
    }

    /// Whether the impl is for a type of a documented crate that the docs leave out, or a type
    /// with such a type as an argument, which rustdoc does not list.
    fn on_stripped_type(&self, imp: Impl) -> bool {
        let db = self.db;
        let ty = imp.self_ty(db);
        let ty = ty.as_reference_inner().unwrap_or(ty);
        std::iter::once(ty.clone()).chain(ty.type_arguments()).any(|it| {
            it.as_adt().is_some_and(|adt| {
                let def = ModuleDef::Adt(adt);
                let krate = places::krate_of(db, def);
                self.places.crates.iter().any(|it| Some(it.krate) == krate)
                    && self.places.item_ref(db, def).is_none()
            })
        })
    }

    /// rustdoc's "Implementations on Foreign Types": an impl on a type that has a home outside
    /// the crate documented and the trait's crate, unless the trait is the standard library's.
    /// An impl of a crate downstream is an implementor, as rustdoc's script adds it.
    fn on_foreign_type(&self, imp: Impl, trait_: hir::Trait) -> bool {
        let db = self.db;
        if !self.allowed.contains(&imp.module(db).krate(db)) {
            return false;
        }
        let trait_krate = places::krate_of(db, ModuleDef::Trait(trait_));
        if trait_krate.is_some_and(|it| {
            matches!(places::lang_crate_name(db, it), Some("core" | "alloc" | "std"))
        }) {
            return false;
        }
        let ty = imp.self_ty(db);
        let home = match ty.as_reference_inner() {
            Some(inner) if inner.as_type_param(db).is_some() => None,
            Some(inner) => places::type_home(db, &inner),
            None => places::type_home(db, &ty),
        };
        let core =
            Crate::all(db).into_iter().find(|it| places::lang_crate_name(db, *it) == Some("core"));
        let Some(home) = home.or(core) else { return false };
        home != self.documented().krate && Some(home) != trait_krate
    }

    fn macro_decl(&self, mac: hir::Macro, name: &str) -> Code {
        let db = self.db;
        let mut out = Code::default();
        match places::macro_kind(db, mac) {
            Some(Kind::ProcDerive) => {
                out.push(Piece::Attribute(format!("#[derive({name})]")));
                let id: hir_def::MacroId = mac.into();
                if let Some(info) = hir_def::attrs::AttrFlags::derive_info(db, id)
                    && !info.helpers.is_empty()
                {
                    out.text("\n{\n    ");
                    out.push(Piece::Comment("// Attributes available to this derive:".to_owned()));
                    for helper in &info.helpers {
                        out.text("\n    ")
                            .push(Piece::Attribute(format!("#[{}]", helper.as_str())));
                    }
                    out.text("\n}");
                }
                return out;
            }
            Some(Kind::ProcAttribute) => {
                out.push(Piece::Attribute(format!("#[{name}]")));
                return out;
            }
            _ => {}
        }
        if mac.is_proc_macro() {
            out.push(Piece::Name(name.to_owned(), Kind::Macro)).text("!() {");
            out.text(" ").push(Piece::Comment("/* proc-macro */".to_owned())).text(" }");
            return out;
        }
        let Some(source) = mac.source_view(db) else { return out };
        match source.value {
            Either::Left(ast::Macro::MacroRules(rules)) => {
                out.kw("macro_rules!")
                    .text(" ")
                    .push(Piece::Name(name.to_owned(), Kind::Macro))
                    .text(" {\n");
                if let Some(tt) = rules.token_tree() {
                    for pattern in macro_rule_patterns(tt) {
                        out.text(format!("    {pattern} => {{ ... }};\n"));
                    }
                }
                out.text("}");
            }
            Either::Left(ast::Macro::MacroDef(def)) => {
                sig::visibility(db, &mut out, mac.visibility(db), mac.module(db));
                out.kw("macro").text(" ").push(Piece::Name(name.to_owned(), Kind::Macro));
                if let Some(args) = def.args() {
                    out.text(squash(args.syntax().text()));
                    out.text(" { ... }");
                } else if let Some(body) = def.body() {
                    out.text(" {\n");
                    for pattern in macro_rule_patterns(body) {
                        out.text(format!("    {pattern} => {{ ... }},\n"));
                    }
                    out.text("}");
                }
            }
            Either::Right(_) => {}
        }
        out
    }
}

fn text_range(range: &std::ops::Range<usize>) -> TextRange {
    TextRange::new(TextSize::from(range.start as u32), TextSize::from(range.end as u32))
}

fn same_kind(a: AssocItem, b: AssocItem) -> bool {
    matches!(
        (a, b),
        (AssocItem::Function(_), AssocItem::Function(_))
            | (AssocItem::Const(_), AssocItem::Const(_))
            | (AssocItem::TypeAlias(_), AssocItem::TypeAlias(_))
    )
}

fn assoc_def(assoc: AssocItem) -> ModuleDef {
    match assoc {
        AssocItem::Function(it) => ModuleDef::Function(it),
        AssocItem::Const(it) => ModuleDef::Const(it),
        AssocItem::TypeAlias(it) => ModuleDef::TypeAlias(it),
    }
}

fn assoc_hidden(db: &Db, assoc: AssocItem) -> bool {
    match assoc {
        AssocItem::Function(it) => it.attrs(db).is_doc_hidden(),
        AssocItem::Const(it) => it.attrs(db).is_doc_hidden(),
        AssocItem::TypeAlias(it) => it.attrs(db).is_doc_hidden(),
    }
}

fn assoc_visibility(db: &Db, assoc: AssocItem) -> Visibility {
    match assoc {
        AssocItem::Function(it) => it.visibility(db),
        AssocItem::Const(it) => it.visibility(db),
        AssocItem::TypeAlias(it) => it.visibility(db),
    }
}

fn must_use(db: &Db, def: ModuleDef) -> Option<String> {
    let id: hir_def::AttrDefId = match def {
        ModuleDef::Function(it) => hir_def::FunctionId::try_from(it).ok()?.into(),
        ModuleDef::Adt(it) => hir_def::AdtId::from(it).into(),
        ModuleDef::Trait(it) => hir_def::TraitId::from(it).into(),
        _ => return None,
    };
    hir_def::attrs::AttrFlags::must_use_message(db, id).map(|it| it.unwrap_or_default().to_owned())
}

/// rustdoc's id for an impl: `impl-Trait-for-Type`, or `impl-Type` for an inherent one, URL
/// encoded as rustdoc encodes it (`impl-Debug-for-Iter%3C'a,+T%3E`).
pub(crate) fn impl_anchor(header: &Code) -> String {
    let text = header.plain();
    let first_line = text.lines().next().unwrap_or_default();
    let mut rest = first_line.strip_prefix("unsafe ").unwrap_or(first_line);
    rest = rest.strip_prefix("impl").unwrap_or(rest);
    if rest.starts_with('<') {
        rest = &rest[closing(rest).map_or(rest.len(), |at| at + 1)..];
    }
    let rest = rest.trim().trim_start_matches('!');
    let rest = rest.strip_suffix(" where").unwrap_or(rest);
    let named = match top_level_for(rest) {
        Some(at) => format!("impl-{}-for-{}", &rest[..at], &rest[at + " for ".len()..]),
        None => format!("impl-{rest}"),
    };
    small_url_encode(&without_reference_lifetimes(&named))
}

/// `&'a T` as `&T`, as rustdoc's ids of impls write references.
fn without_reference_lifetimes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("&'") {
        out.push_str(&rest[..=at]);
        let after = &rest[at + 2..];
        let end = after.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(after.len());
        rest = after[end..].strip_prefix(' ').unwrap_or(&after[end..]);
    }
    out.push_str(rest);
    out
}

/// The index of the `>` that closes the `<` `text` starts with.
fn closing(text: &str) -> Option<usize> {
    let mut depth = 0;
    for (at, c) in text.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' if at > 0 && text.as_bytes()[at - 1] == b'-' => {}
            '>' | ')' | ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// Where ` for ` splits a trait impl's header into the trait and the type, outside brackets.
fn top_level_for(text: &str) -> Option<usize> {
    let mut depth = 0i32;
    let bytes = text.as_bytes();
    for (at, &b) in bytes.iter().enumerate() {
        match b {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' if at > 0 && bytes[at - 1] == b'-' => {}
            b'>' | b')' | b']' => depth -= 1,
            b' ' if depth == 0 && text[at..].starts_with(" for ") => return Some(at),
            _ => {}
        }
    }
    None
}

/// rustdoc's `small_url_encode`: a space as `+`, what a URL needs escaped as `%XX`.
fn small_url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            ' ' => out.push('+'),
            c if c.is_ascii_alphanumeric() || "-_.,~!'()*/;:?=".contains(c) => out.push(c),
            c => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}

/// Whether a blanket impl applies to `ty`, as rustdoc decides: with the impl's parameters and
/// the type's as inference variables, each of the impl's where-clauses may hold, solved in
/// `krate`, the crate documented, which may see impls a re-exported type's crate does not.
fn blanket_applies(db: &Db, imp: Impl, ty: &hir::Type<'_>, krate: Crate) -> bool {
    imp.self_ty(db).as_type_param(db).is_some() && ty.may_have_impl(db, imp, krate)
}

/// The patterns of a `macro_rules!`'s rules, each as written on one line.
fn macro_rule_patterns(tt: ast::TokenTree<'_>) -> Vec<String> {
    let mut out = vec![];
    let mut expect_pattern = true;
    for child in tt.syntax().children_with_tokens() {
        match child {
            syntax::NodeOrToken::Node(node) => {
                if expect_pattern && let Some(pattern) = ast::TokenTree::cast(node) {
                    out.push(squash(pattern.syntax().text()));
                    expect_pattern = false;
                }
            }
            syntax::NodeOrToken::Token(token) => {
                if token.kind() == T![;] || token.kind() == T![,] {
                    expect_pattern = true;
                } else if token.kind() == SyntaxKind::FAT_ARROW
                    || token.kind() == T![=]
                    || token.kind() == T![>]
                {
                    expect_pattern = false;
                }
            }
        }
    }
    out
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
