//! `check_private_in_public`: a type or trait in the interface of an item that is less visible
//! than the item is reachable at. In the item's own interface (its type, its signature, its
//! generics' defaults and const types, an impl's types and trait) it is `private_interfaces`; in
//! its bounds and `where` clauses `private_bounds`. In an associated type a type less visible than
//! the type's nominal visibility is E0446, rustc's hard error of the same pass.
//!
//! The visibility an item is reachable at is its effective visibility at `Level::Reachable`
//! (`names::effective`), a module restriction or `pub`; an item with none is not checked.
//!
//! Types are walked as rustc's `DefIdVisitorSkeleton` walks them: each ADT and trait they name,
//! aliases expanded in place, `<T as Trait>::Assoc` as its associated type, its trait and then
//! `T`, `impl Trait` as its bounds; an impl's associated types not looked into.
//!
//! Follows `compiler/rustc_privacy/src/lib.rs` (`SearchInterfaceForPrivateItemsVisitor`,
//! `PrivateItemsInPublicInterfacesChecker`, `check_private_in_public`),
//! `compiler/rustc_middle/src/ty/mod.rs`
//! (`Visibility::to_string`) and `compiler/rustc_privacy/src/diagnostics.rs`
//! (`PrivateInterfacesOrBoundsLint`, `InPublicInterface`) (MIT or Apache-2.0).

use std::cell::OnceCell;

use blaze_hash::HashSet;
use blaze_items::Fact;
use hir::{AsAssocItem as _, AssocItemContainer, InFile, ModuleDef, PathResolution};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode, HasGenericArgs, HasTypeBounds},
};

use super::{
    def_span, visibility,
    walk::{self, Container, Item},
};
use crate::{
    cx::Cx,
    lints::names::effective::{
        self, Def, Effective, Level, Vis, greater, is_local, min, module_of, nominal,
    },
    lower::{Found, Phase},
    model::Diag,
};

/// rustc's `Visibility::to_string` of `vis` for `def`, printed only for a lint that fires.
fn vis_str(cx: &Cx<'_, '_>, vis: Vis, def: Def) -> String {
    match vis {
        Vis::Public => "pub".to_owned(), // alloc: a lint that fires
        Vis::In(module) if module.is_crate_root(cx.db) => "pub(crate)".to_owned(), // alloc: a lint that fires
        Vis::In(module) if Some(module) == module_of(cx, def) => "pub(self)".to_owned(), // alloc: a lint that fires
        Vis::In(module) => {
            let mut path = String::new(); // alloc: a lint that fires
            for it in module.path_to_root(cx.db).into_iter().rev() {
                if let Some(name) = it.name(cx.db) {
                    path.push_str("::");
                    path.push_str(name.as_str());
                }
            }
            format!("pub(in crate{path})") // alloc: a lint that fires
        }
    }
}

pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, items: &[Item<'db>], effective: &Effective) -> Vec<Found> {
    // alloc: the pass's output, once per crate
    let mut checker = Checker { cx, effective, found: Vec::new(), seen: HashSet::default() };
    let mut at = 0;
    while at < items.len() {
        let module = items[at].module;
        let end = items[at..].iter().position(|it| it.module != module).map_or(items.len(), |it| at + it);
        for (index, item) in items.iter().enumerate().take(end).skip(at) {
            if matches!(item.container, Container::Module) {
                checker.item(item, item.held(items, index));
            }
        }
        for item in &items[at..end] {
            if matches!(item.container, Container::Extern(_)) {
                checker.foreign_item(item);
            }
        }
        at = end;
    }
    checker.found
}

/// One `SearchInterfaceForPrivateItemsVisitor`: the item it checks and what it requires.
struct Check<'i, 'db> {
    item: Def,
    node: InFile<ast::Node<'db>>,
    kind: &'static str,
    /// rustc's `def_path_str` of `item`, printed when a diagnostic first needs it.
    descr: OnceCell<String>,
    required: Vis,
    required_ev: Option<Vis>,
    hard_error: bool,
    primary: bool,
    skip_assoc: bool,
    /// The item whose interface it is, for the fact the diagnostic read.
    owner: &'i Item<'db>,
}

/// The visibility a def is reachable at: its effective visibility at `Level::Reachable`.
fn reachable(effective: &Effective, def: Def) -> Option<Vis> {
    effective.get(def).map(|ev| ev.at(Level::Reachable))
}

struct Checker<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    effective: &'c Effective,
    found: Vec<Found>,
    /// rustc emits a diagnostic it already emitted once.
    seen: HashSet<String>,
}

impl<'db> Checker<'_, '_, 'db> {
    #[allow(clippy::too_many_arguments)]
    fn check<'i>(
        &self,
        owner: &'i Item<'db>,
        item: Def,
        node: InFile<ast::Node<'db>>,
        kind: &'static str,
        required: Vis,
        required_ev: Option<Vis>,
    ) -> Check<'i, 'db> {
        let descr = OnceCell::new();
        Check { item, node, kind, descr, required, required_ev, hard_error: false, primary: true, skip_assoc: false, owner }
    }

    /// `check_item`, with `held` the items it holds (its own items among them).
    fn item(&mut self, item: &Item<'db>, held: &[Item<'db>]) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let node = item.syntax();
        let vis = nominal(cx, def);
        let ev = reachable(self.effective, def);
        let file = item.node.file_id;
        match item.node.value {
            ast::Item::Const(_) | ast::Item::Static(_) | ast::Item::Fn(_) | ast::Item::TypeAlias(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                self.ty(&mut check, item.node.value, None);
            }
            ast::Item::Trait(t) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                let own = |it: &Item<'db>| matches!(it.container, Container::Trait(c) if c.file_id == file && c.value == t);
                for assoc in held.iter().filter(|it| own(it)) {
                    self.assoc_item(assoc, vis, ev, true, None);
                }
            }
            ast::Item::Enum(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                for field in walk::fields(item) {
                    self.field(item, field, |_| (vis, ev));
                }
            }
            ast::Item::Struct(_) | ast::Item::Union(_) => {
                let mut check = self.check(item, def, node, descr_kind(item, false), vis, ev);
                self.generics(&mut check, item.node.value);
                self.clauses(&mut check, item.node.value);
                let effective = self.effective;
                for field in walk::fields(item) {
                    self.field(item, field, |field| (min(cx, vis, nominal(cx, field)), reachable(effective, field)));
                }
            }
            ast::Item::Impl(imp) => {
                let Def::Impl(id) = def else { return };
                let of_trait = imp.trait_().is_some();
                let defs = effective::impl_defs(cx, id, false);
                let impl_vis = defs.iter().fold(Vis::Public, |at, it| min(cx, nominal(cx, *it), at));
                let impl_ev = self.effective.of_impl(cx, id).at(Level::Reachable);
                let mut check = self.check(item, def, node, "implementation", impl_vis, Some(impl_ev));
                if !of_trait {
                    self.generics(&mut check, item.node.value);
                    self.clauses(&mut check, item.node.value);
                }
                check.skip_assoc = true;
                check.primary = true;
                let mut aliases = HashSet::default();
                if let Some(ty) = imp.self_ty() {
                    self.visit(&check, InFile::new(file, ty.syntax()), false, &mut aliases);
                }
                if let Some(tr) = imp.trait_() {
                    self.visit(&check, InFile::new(file, tr.syntax()), false, &mut aliases);
                }
                let own = |it: &Item<'db>| matches!(it.container, Container::Impl(c) if c.file_id == file && c.value == imp);
                for assoc in held.iter().filter(|it| own(it)) {
                    let Some(assoc_def) = visibility::def(cx, assoc) else { continue };
                    let (vis, ev) = match of_trait {
                        false => (
                            min(cx, nominal(cx, assoc_def), impl_vis),
                            reachable(self.effective, assoc_def).map(|ev| min(cx, ev, impl_ev)),
                        ),
                        true => (impl_vis, Some(impl_ev)),
                    };
                    self.assoc_item(assoc, vis, ev, false, imp.self_ty());
                }
            }
            _ => {}
        }
    }

    /// `check_foreign_item`.
    fn foreign_item(&mut self, item: &Item<'db>) {
        let Some(def) = visibility::def(self.cx, item) else { return };
        let vis = nominal(self.cx, def);
        let ev = reachable(self.effective, def);
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, false), vis, ev);
        self.generics(&mut check, item.node.value);
        self.clauses(&mut check, item.node.value);
        self.ty(&mut check, item.node.value, None);
    }

    fn field(&mut self, owner: &Item<'db>, field: InFile<ast::Node<'db>>, required: impl FnOnce(Def) -> (Vis, Option<Vis>)) {
        let cx = self.cx;
        let def = match field.value.kind() {
            SyntaxKind::RECORD_FIELD => ast::RecordField::cast(field.value).and_then(|it| cx.vsema.to_def2(field.with_value(it))),
            _ => ast::TupleField::cast(field.value).and_then(|it| cx.vsema.to_def2(field.with_value(it))),
        };
        let Some(def) = def else { return };
        let def = Def::Field(def);
        let (vis, ev) = required(def);
        let file = field.file_id;
        let ty = field.value.children().find_map(ast::Type::cast);
        let mut check = self.check(owner, def, field, "field", vis, ev);
        check.primary = true;
        if let Some(ty) = ty {
            self.visit(&check, InFile::new(file, ty.syntax()), false, &mut HashSet::default());
        }
    }

    /// `check_assoc_item`: of a trait (`in_trait`) or of an impl whose self type is `impl_self`.
    fn assoc_item(&mut self, item: &Item<'db>, vis: Vis, ev: Option<Vis>, in_trait: bool, impl_self: Option<ast::Type<'db>>) {
        let cx = self.cx;
        let Some(def) = visibility::def(cx, item) else { return };
        let is_assoc_ty = matches!(item.node.value, ast::Item::TypeAlias(_));
        let mut check = self.check(item, def, item.syntax(), descr_kind(item, true), vis, ev);
        check.hard_error = is_assoc_ty;
        self.generics(&mut check, item.node.value);
        self.clauses(&mut check, item.node.value);
        let has_type_of = match item.node.value {
            ast::Item::TypeAlias(it) => !in_trait || it.ty().is_some(),
            _ => true,
        };
        if has_type_of {
            let impl_self = impl_self.map(|it| item.node.with_value(it.syntax()));
            self.ty(&mut check, item.node.value, impl_self);
        }
        if is_assoc_ty && in_trait {
            check.hard_error = false;
            check.primary = false;
            if let ast::Item::TypeAlias(it) = item.node.value {
                let mut aliases = HashSet::default();
                for bound in it.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                    self.visit(&check, item.node.with_value(bound.syntax()), false, &mut aliases);
                }
            }
        }
    }

    /// `generics()`: the types of const parameters and the defaults of parameters.
    fn generics(&mut self, check: &mut Check<'_, 'db>, node: ast::Item<'db>) {
        check.primary = true;
        let Some(params) = node.syntax().children().find_map(ast::GenericParamList::cast) else { return };
        let file = check.node.file_id;
        let mut aliases = HashSet::default();
        for param in params.generic_params() {
            let ty = match &param {
                ast::GenericParam::ConstParam(it) => it.ty(),
                ast::GenericParam::TypeParam(it) => it.default_type(),
                ast::GenericParam::LifetimeParam(_) => None,
            };
            if let Some(ty) = ty {
                self.visit(check, InFile::new(file, ty.syntax()), false, &mut aliases);
            }
        }
    }

    /// `clauses()`: a trait's supertraits, the bounds of parameters, the `where` clause, the
    /// bounds of `impl Trait` parameters; each bound's trait before the type it bounds.
    fn clauses(&mut self, check: &mut Check<'_, 'db>, node: ast::Item<'db>) {
        check.primary = false;
        let file = check.node.file_id;
        let mut aliases = HashSet::default();
        if let ast::Item::Trait(it) = node {
            for bound in it.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                self.visit(check, InFile::new(file, bound.syntax()), false, &mut aliases);
            }
        }
        let params = node.syntax().children().find_map(ast::GenericParamList::cast);
        for param in params.iter().flat_map(|it| it.generic_params()) {
            if let ast::GenericParam::TypeParam(param) = param {
                for bound in param.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                    self.visit(check, InFile::new(file, bound.syntax()), false, &mut aliases);
                }
            }
        }
        let clause = node.syntax().children().find_map(ast::WhereClause::cast);
        for pred in clause.iter().flat_map(|it| it.predicates()) {
            let Some(ty) = pred.ty() else { continue };
            for bound in pred.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                if bound.lifetime().is_none() {
                    self.visit(check, InFile::new(file, bound.syntax()), false, &mut aliases);
                }
                self.visit(check, InFile::new(file, ty.syntax()), false, &mut aliases);
            }
        }
        if let ast::Item::Fn(f) = node {
            for ty in f.param_list().into_iter().flat_map(|it| it.params()).filter_map(|it| it.ty()) {
                for apit in ty.syntax().descendants().filter_map(ast::ImplTraitType::cast) {
                    for bound in apit.type_bound_list().into_iter().flat_map(|it| it.bounds()) {
                        self.visit(check, InFile::new(file, bound.syntax()), false, &mut aliases);
                    }
                }
            }
        }
    }

    /// `ty()`: a function's signature (and its impl's self type), else the item's type.
    fn ty(&mut self, check: &mut Check<'_, 'db>, node: ast::Item<'db>, impl_self: Option<InFile<ast::Node<'db>>>) {
        check.primary = true;
        let file = check.node.file_id;
        let mut aliases = HashSet::default();
        match node {
            ast::Item::Fn(f) => {
                if let Some(list) = f.param_list() {
                    if let Some(ty) = list.self_param().and_then(|it| it.ty()) {
                        self.visit(check, InFile::new(file, ty.syntax()), true, &mut aliases);
                    }
                    for ty in list.params().filter_map(|it| it.ty()) {
                        self.visit(check, InFile::new(file, ty.syntax()), true, &mut aliases);
                    }
                }
                if let Some(ty) = f.ret_type().and_then(|it| it.ty()) {
                    self.visit(check, InFile::new(file, ty.syntax()), false, &mut aliases);
                }
                if let Some(ty) = impl_self {
                    self.visit(check, ty, false, &mut aliases);
                }
            }
            ast::Item::Const(it) => {
                if let Some(ty) = it.ty() {
                    self.visit(check, InFile::new(file, ty.syntax()), false, &mut aliases);
                }
            }
            ast::Item::Static(it) => {
                if let Some(ty) = it.ty() {
                    self.visit(check, InFile::new(file, ty.syntax()), false, &mut aliases);
                }
            }
            ast::Item::TypeAlias(it) => {
                if let Some(ty) = it.ty() {
                    self.visit(check, InFile::new(file, ty.syntax()), false, &mut aliases);
                }
            }
            _ => {}
        }
    }

    /// The defs a type (or a bound) names, in the order rustc's skeleton visits them.
    fn visit(&mut self, check: &Check<'_, 'db>, node: InFile<ast::Node<'db>>, skip_impl_trait: bool, aliases: &mut HashSet<hir::TypeAlias>) {
        let cx = self.cx;
        let mut walk = node.value.preorder();
        while let Some(event) = walk.next() {
            let syntax::WalkEvent::Enter(it) = event else { continue };
            let kind = it.kind();
            if kind == SyntaxKind::CONST_ARG || ast::Expr::can_cast(kind) {
                walk.skip_subtree();
                continue;
            }
            if skip_impl_trait && kind == SyntaxKind::IMPL_TRAIT_TYPE {
                walk.skip_subtree();
                continue;
            }
            let Some(ty) = ast::PathType::cast(it) else { continue };
            let Some(path) = ty.path() else { continue };
            match cx.vsema.resolve_path(path) {
                Some(PathResolution::Def(ModuleDef::TypeAlias(alias))) => {
                    walk.skip_subtree();
                    if alias.as_assoc_item(cx.db).is_some() {
                        if !check.skip_assoc {
                            self.projection(check, node.with_value(path), alias, aliases);
                        }
                        continue;
                    }
                    if aliases.insert(alias)
                        && let Some(alias_node) = cx.vsema.source(alias)
                        && let Some(aliased) = alias_node.value.ty()
                    {
                        self.visit(check, alias_node.with_value(aliased.syntax()), skip_impl_trait, aliases);
                    }
                    if let Some(args) = path.segment().and_then(|it| it.generic_arg_list()) {
                        self.visit(check, node.with_value(args.syntax()), skip_impl_trait, aliases);
                    }
                }
                Some(PathResolution::Def(def @ ModuleDef::Adt(_))) => {
                    let descr = || match cx.vsema.resolve_type(ast::Type::PathType(ty)) {
                        Some(it) => cx.ty(&it),
                        None => cx.def_path(def),
                    };
                    self.def_id(check, Def::Item(def), "type", descr);
                }
                Some(PathResolution::Def(ModuleDef::Trait(tr))) => {
                    self.def_id(check, Def::Item(ModuleDef::Trait(tr)), "trait", || trait_descr(cx, tr, path));
                }
                _ => {}
            }
        }
    }

    /// `<T as Trait>::Assoc` (or `T::Assoc`): the associated type, its trait, then `T` and the
    /// trait's arguments.
    fn projection(&mut self, check: &Check<'_, 'db>, path: InFile<ast::Path<'db>>, alias: hir::TypeAlias, aliases: &mut HashSet<hir::TypeAlias>) {
        let cx = self.cx;
        let def = ModuleDef::TypeAlias(alias);
        self.def_id(check, Def::Item(def), "associated type", || cx.def_path(def));
        let Some(AssocItemContainer::Trait(tr)) = alias.as_assoc_item(cx.db).map(|it| it.container(cx.db)) else {
            return;
        };
        let anchor = path.value.qualifier().and_then(|it| it.segment()).and_then(|it| it.type_anchor());
        let mut types = anchor.iter().flat_map(|it| it.syntax().children().filter_map(ast::Type::cast));
        let self_ty = types.next();
        let trait_path = match types.next() {
            Some(ast::Type::PathType(it)) => it.path(),
            _ => None,
        };
        let descr = || match trait_path {
            Some(it) => trait_descr(cx, tr, it),
            None => cx.def_path(ModuleDef::Trait(tr)),
        };
        self.def_id(check, Def::Item(ModuleDef::Trait(tr)), "trait", descr);
        if let Some(self_ty) = &self_ty {
            self.visit(check, path.with_value(self_ty.syntax()), false, aliases);
        }
        if let Some(args) = trait_path.and_then(|it| it.segment()).and_then(|it| it.generic_arg_list()) {
            self.visit(check, path.with_value(args.syntax()), false, aliases);
        }
    }

    /// `check_def_id`: the error or the lint of a def named in the interface of `check.item`.
    /// What a lint must print is printed only once it fires.
    fn def_id(&mut self, check: &Check<'_, 'db>, def: Def, kind: &str, descr: impl FnOnce() -> String) {
        let cx = self.cx;
        if !is_local(cx, def) {
            return;
        }
        let vis = nominal(cx, def);
        let hard_error = check.hard_error && greater(cx, check.required, vis);
        if !hard_error && !check.required_ev.is_some_and(|ev| greater(cx, ev, vis)) {
            return;
        }
        let Some(def_node) = source(cx, def) else { return };
        let item_span = cx.span(check.node.map(def_span));
        let vis_span = cx.span(def_node.map(def_span));
        let descr = descr();
        if hard_error {
            let vis_descr = match vis {
                Vis::Public => "public",
                Vis::In(module) if Some(module) == module_of(cx, def) => "private",
                Vis::In(module) if module.is_crate_root(cx.db) => "crate-private",
                Vis::In(_) => "restricted",
            };
            // alloc: the message of an error that fires
            let message = format!("{vis_descr} {kind} `{descr}` in public interface");
            let mut diag = Diag::error(Some("E0446"), message, item_span.clone()); // clone: the span of an error that fires
            diag.span_label(item_span, format!("can't leak {vis_descr} {kind}")); // alloc: an error that fires
            diag.span_label(vis_span, format!("`{descr}` declared as {vis_descr}")); // alloc: an error that fires
            self.fact(check, &mut diag);
            self.push(Found::new(diag, Phase::Late));
            return;
        }
        let Some(ev) = check.required_ev else { return };
        let item_descr = check.descr.get_or_init(|| self.descr(check.item));
        let lint = if check.primary { "private_interfaces" } else { "private_bounds" };
        // alloc: the message of a lint that fires
        let message = format!("{kind} `{descr}` is more private than the item `{item_descr}`");
        let mut diag = Diag::lint(lint, message, item_span.clone()); // clone: the span of a lint that fires
        let item_vis = vis_str(cx, ev, check.item);
        // alloc: a lint that fires
        diag.span_label(item_span, format!("{} `{item_descr}` is reachable at visibility `{item_vis}`", check.kind));
        let def_vis = vis_str(cx, vis, def);
        // alloc: a lint that fires
        diag.span_note(vis_span, format!("but {kind} `{descr}` is only usable at visibility `{def_vis}`"));
        self.fact(check, &mut diag);
        let at = check.node.map(ast::ptr);
        self.push(Found::lint(diag, Phase::Late, at));
    }

    fn fact(&self, check: &Check<'_, 'db>, diag: &mut Diag) {
        if let Some((id, name)) = super::item_id(self.cx, check.owner) {
            diag.fact(Fact::Signature(self.cx.key(id)), format!("the interface of `{name}`")); // alloc: a lint that fires
        }
    }

    fn push(&mut self, found: Found) {
        let diag = &found.diag;
        // alloc: the key of a diagnostic that fires, to emit it once
        let key = format!("{}|{}|{:?}|{:?}", diag.message, diag.code.as_ref().map_or("", |it| it.as_str()), diag.span, diag.children);
        if self.seen.insert(key) {
            self.found.push(found);
        }
    }

    /// rustc's `def_path_str` of the item checked: a field by its parent's, an impl as its type
    /// (or `<Type as Trait>`). Printed only for a lint that fires.
    fn descr(&self, def: Def) -> String {
        let cx = self.cx;
        let db = cx.db;
        match def {
            Def::Item(it) => cx.def_path(it),
            Def::Field(field) => {
                let parent = match field.parent_def(db) {
                    hir::Variant::Struct(it) => cx.def_path(ModuleDef::Adt(it.into())),
                    hir::Variant::Union(it) => cx.def_path(ModuleDef::Adt(it.into())),
                    hir::Variant::EnumVariant(it) => cx.def_path(ModuleDef::EnumVariant(it)),
                };
                format!("{parent}::{}", field.name(db).as_str()) // alloc: a lint that fires
            }
            Def::Impl(imp) => {
                let self_ty = cx.ty(&imp.self_ty(db));
                match imp.trait_(db) {
                    Some(tr) => format!("<{self_ty} as {}>", cx.def_path(ModuleDef::Trait(tr))), // alloc: a lint that fires
                    None => self_ty,
                }
            }
            // rustc's anonymous `DefPathData::Use` segment, `{use}`, after its module's path.
            Def::Use(_) => match module_of(cx, def) {
                Some(module) if !module.is_crate_root(db) => {
                    format!("{}::{{use}}", cx.def_path(ModuleDef::Module(module))) // alloc: a lint that fires
                }
                _ => "{use}".to_owned(), // alloc: a lint that fires
            },
        }
    }
}

/// `print_only_trait_path`: the trait's path and the arguments written to it, printed only
/// for a lint that fires.
fn trait_descr<'db>(cx: &Cx<'_, 'db>, tr: hir::Trait, path: ast::Path<'db>) -> String {
    let mut descr = cx.def_path(ModuleDef::Trait(tr));
    let Some(args) = path.segment().and_then(|it| it.generic_arg_list()) else { return descr };
    let mut open = false;
    for arg in args.generic_args() {
        descr.push_str(if open { ", " } else { "<" });
        open = true;
        let resolved = match &arg {
            ast::GenericArg::TypeArg(it) => it.ty().and_then(|ty| cx.vsema.resolve_type(ty)),
            _ => None,
        };
        match resolved {
            Some(ty) => descr.push_str(&cx.ty(&ty)),
            None => descr.push_str(arg.syntax().text()),
        }
    }
    if open {
        descr.push('>');
    }
    descr
}

/// rustc's `def_descr` of a walked item, `assoc` when it is an item of a trait or an impl.
fn descr_kind(item: &Item<'_>, assoc: bool) -> &'static str {
    match &item.node.value {
        ast::Item::Fn(it) if assoc => match it.param_list().and_then(|it| it.self_param()) {
            Some(_) => "method",
            None => "associated function",
        },
        ast::Item::Fn(_) => "function",
        ast::Item::Const(_) if assoc => "associated constant",
        ast::Item::Const(_) => "constant",
        ast::Item::Static(_) => "static",
        ast::Item::TypeAlias(_) if assoc => "associated type",
        ast::Item::TypeAlias(_) if matches!(item.container, Container::Extern(_)) => "foreign type",
        ast::Item::TypeAlias(_) => "type alias",
        ast::Item::Struct(_) => "struct",
        ast::Item::Enum(_) => "enum",
        ast::Item::Union(_) => "union",
        ast::Item::Trait(it) if it.eq_token().is_some() => "trait alias",
        ast::Item::Trait(_) => "trait",
        ast::Item::Impl(_) => "implementation",
        _ => "item",
    }
}

/// The syntax of a def, for its `def_span`, read only for a diagnostic that fires.
fn source<'db>(cx: &Cx<'_, 'db>, def: Def) -> Option<InFile<ast::Node<'db>>> {
    match def {
        Def::Item(ModuleDef::EnumVariant(it)) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        Def::Item(ModuleDef::Module(it)) => it.declaration_source_view(cx.db).map(|it| it.map(|it| it.syntax())),
        _ => visibility::interface_node(cx, def),
    }
}
