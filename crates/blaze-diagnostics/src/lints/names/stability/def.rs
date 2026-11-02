//! What a use names, as rustc's stability checks see it: an item, a field or a constructor;
//! its kind as rustc names it (`def_descr`), its path as rustc prints it with no trimming
//! (`with_no_trimmed_paths!(def_path_str(..))`), where it is written and the fact of it.
//!
//! Follows `compiler/rustc_middle/src/ty/mod.rs` (`def_descr`), `compiler/rustc_hir/src/def.rs`
//! (`DefKind::descr`), `compiler/rustc_middle/src/ty/print/pretty.rs` (`def_path_str`,
//! `try_print_visible_def_path`, `path_crate`) and `compiler/rustc_middle/src/ty/print/mod.rs`
//! (`default_print_def_path`, `default_print_impl_path`) (MIT or Apache-2.0).

use base_db::Db;
use blaze_items::ItemId;
use hir::{
    Adt, AsAssocItem as _, AssocItem, AssocItemContainer, HasAttrs as _, HasVisibility as _, InFile, ModuleDef,
    StructKind, Variant,
};
use hir_def::attrs::stability::{DeprecationEntry, Stability};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasName as _, Node},
};

use crate::cx::Cx;

/// What a use names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Def {
    Item(ModuleDef),
    Field(hir::Field),
    /// The constructor of a tuple or unit struct or variant, which a path in the value
    /// namespace names.
    Ctor(Variant),
}

impl Def {
    /// `variant` as the item it is: the struct, union or enum variant.
    pub(super) fn variant(variant: Variant) -> Def {
        Def::Item(match variant {
            Variant::Struct(it) => ModuleDef::Adt(Adt::Struct(it)),
            Variant::Union(it) => ModuleDef::Adt(Adt::Union(it)),
            Variant::EnumVariant(it) => ModuleDef::EnumVariant(it),
        })
    }

    /// What a path in the value namespace resolving to `def` names: the constructor of a
    /// tuple or unit struct or variant, else `def`.
    pub(super) fn value(db: &Db, def: ModuleDef) -> Def {
        let variant = match def {
            ModuleDef::Adt(Adt::Struct(it)) => Variant::Struct(it),
            ModuleDef::EnumVariant(it) => Variant::EnumVariant(it),
            _ => return Def::Item(def),
        };
        match kind(db, variant) {
            StructKind::Record => Def::Item(def),
            StructKind::Tuple | StructKind::Unit => Def::Ctor(variant),
        }
    }

    /// The item `self` is or is of.
    pub(super) fn item(self, db: &Db) -> ModuleDef {
        match self {
            Def::Item(it) => it,
            Def::Field(it) => Def::variant(it.parent_def(db)).item(db),
            Def::Ctor(it) => Def::variant(it).item(db),
        }
    }

    pub(super) fn krate(self, db: &Db) -> Option<hir::Crate> {
        match self.item(db) {
            ModuleDef::Module(it) => Some(it.krate(db)),
            it => Some(it.module(db)?.krate(db)),
        }
    }

    /// rustc's `def_descr`.
    pub(super) fn descr(self, db: &Db) -> &'static str {
        match self {
            Def::Field(_) => "field",
            Def::Ctor(variant) => match (variant, kind(db, variant)) {
                (Variant::EnumVariant(_), StructKind::Tuple) => "tuple variant",
                (Variant::EnumVariant(_), _) => "unit variant",
                (_, StructKind::Tuple) => "tuple struct",
                _ => "unit struct",
            },
            Def::Item(item) => match item {
                ModuleDef::Module(it) if it.is_crate_root(db) => "crate",
                ModuleDef::Module(_) => "module",
                ModuleDef::Function(it) => match it.as_assoc_item(db) {
                    Some(_) if it.has_self_param(db) => "method",
                    Some(_) => "associated function",
                    None => "function",
                },
                ModuleDef::Adt(Adt::Struct(_)) => "struct",
                ModuleDef::Adt(Adt::Union(_)) => "union",
                ModuleDef::Adt(Adt::Enum(_)) => "enum",
                ModuleDef::EnumVariant(_) => "variant",
                ModuleDef::Const(it) => match it.as_assoc_item(db) {
                    Some(_) => "associated constant",
                    None => "constant",
                },
                ModuleDef::Static(_) => "static",
                ModuleDef::Trait(_) => "trait",
                ModuleDef::TypeAlias(it) => match it.as_assoc_item(db) {
                    Some(_) => "associated type",
                    None => "type alias",
                },
                ModuleDef::BuiltinType(_) => "builtin type",
                ModuleDef::Macro(it) => match it.kind(db) {
                    hir::MacroKind::Derive | hir::MacroKind::DeriveBuiltIn => "derive macro",
                    hir::MacroKind::Attr | hir::MacroKind::AttrBuiltIn => "attribute macro",
                    _ => "macro",
                },
            },
        }
    }

    /// Whether rustc checks the stability of `self` across crates: it is public
    /// (`skip_stability_check_due_to_privacy`).
    pub(super) fn is_public(self, db: &Db) -> bool {
        let visibility = match self {
            Def::Field(it) => it.visibility(db),
            Def::Ctor(it) => match it {
                Variant::Struct(it) => it.visibility(db),
                Variant::Union(it) => it.visibility(db),
                Variant::EnumVariant(it) => it.visibility(db),
            },
            Def::Item(item) => match item {
                ModuleDef::Module(it) => it.visibility(db),
                ModuleDef::Function(it) => it.visibility(db),
                ModuleDef::Adt(it) => it.visibility(db),
                ModuleDef::EnumVariant(it) => it.visibility(db),
                ModuleDef::Const(it) => it.visibility(db),
                ModuleDef::Static(it) => it.visibility(db),
                ModuleDef::Trait(it) => it.visibility(db),
                ModuleDef::TypeAlias(it) => it.visibility(db),
                ModuleDef::Macro(it) => it.visibility(db),
                ModuleDef::BuiltinType(_) => return true,
            },
        };
        visibility == hir::Visibility::Public
    }

    /// rustc's `lookup_deprecation_entry` of `self`, as hir-def reads it: a constructor
    /// inherits its variant's.
    pub(super) fn deprecation(self, db: &Db) -> Option<DeprecationEntry<'_>> {
        match self {
            Def::Item(it) => it.deprecation(db),
            Def::Field(it) => it.deprecation(db),
            Def::Ctor(it) => it.deprecation(db),
        }
    }

    /// rustc's `lookup_stability` of `self`, as hir-def reads it.
    pub(super) fn stability(self, db: &Db) -> Option<&Stability> {
        match self {
            Def::Item(it) => it.stability(db),
            Def::Field(it) => it.stability(db),
            Def::Ctor(it) => it.stability(db),
        }
    }

    /// The item whose signature holds what `self` is declared with, for a fact.
    pub(super) fn item_id(self, db: &Db) -> Option<ItemId> {
        Some(match self.item(db) {
            ModuleDef::Function(it) => ItemId::Function(hir_def::FunctionId::try_from(it).ok()?),
            ModuleDef::Adt(it) => ItemId::Adt(it.into()),
            ModuleDef::EnumVariant(it) => ItemId::Adt(Adt::Enum(it.parent_enum(db)).into()),
            ModuleDef::Const(it) => ItemId::Const(it.into()),
            ModuleDef::Static(it) => ItemId::Static(it.into()),
            ModuleDef::Trait(it) => ItemId::Trait(it.into()),
            ModuleDef::TypeAlias(it) => ItemId::Alias(it.into()),
            ModuleDef::Macro(it) => ItemId::Macro(it.into()),
            ModuleDef::Module(_) | ModuleDef::BuiltinType(_) => return None,
        })
    }

    /// The node `self` is written as, whose attributes are its own.
    pub(super) fn source<'db>(self, cx: &Cx<'_, 'db>) -> Option<InFile<Node<'db>>> {
        let db = cx.db;
        fn node<'db, N: ast::AstNode<'db>>(it: Option<InFile<N>>) -> Option<InFile<Node<'db>>> {
            it.map(|it| it.map(|it| it.syntax()))
        }
        match self {
            Def::Field(it) => node(cx.vsema.source(it)),
            Def::Ctor(it) => Def::variant(it).source(cx),
            Def::Item(item) => match item {
                ModuleDef::Module(it) => match it.declaration_source_view(db) {
                    Some(decl) => {
                        // records the declaring file's tree, so `cx.vsema` knows `decl`
                        cx.vsema.parse_or_expand(decl.file_id);
                        node(Some(decl))
                    }
                    None => Some(cx.vsema.module_definition_node(it)),
                },
                ModuleDef::Function(it) => node(cx.vsema.source(it)),
                ModuleDef::Adt(Adt::Struct(it)) => node(cx.vsema.source(it)),
                ModuleDef::Adt(Adt::Union(it)) => node(cx.vsema.source(it)),
                ModuleDef::Adt(Adt::Enum(it)) => node(cx.vsema.source(it)),
                ModuleDef::EnumVariant(it) => node(cx.vsema.source(it)),
                ModuleDef::Const(it) => node(cx.vsema.source(it)),
                ModuleDef::Static(it) => node(cx.vsema.source(it)),
                ModuleDef::Trait(it) => node(cx.vsema.source(it)),
                ModuleDef::TypeAlias(it) => node(cx.vsema.source(it)),
                ModuleDef::Macro(it) => node(cx.vsema.source(it)),
                ModuleDef::BuiltinType(_) => None,
            },
        }
    }
}

/// An item-like of this crate as hir reads its attributes: what a use is in
/// (`hir_get_parent_item`).
#[derive(Clone, Copy)]
pub(super) enum ItemLike {
    Def(ModuleDef),
    Field(hir::Field),
    Impl(hir::Impl),
    Use(hir::UseDecl),
    ExternBlock(hir::ExternBlock),
    ExternCrate(hir::ExternCrateDecl),
}

impl ItemLike {
    /// The item-like written at `node`: an item, an associated or foreign item, a field, or the
    /// crate root's file. None for any other node.
    pub(super) fn at<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<ItemLike> {
        macro_rules! def {
            ($ast:ident) => {
                cx.vsema.to_def2(node.with_value(ast::$ast::cast(node.value)?))?
            };
        }
        Some(match node.value.kind() {
            SyntaxKind::SOURCE_FILE => ItemLike::Def(ModuleDef::Module(def!(SourceFile))),
            SyntaxKind::MODULE => ItemLike::Def(ModuleDef::Module(def!(Module))),
            SyntaxKind::FN => ItemLike::Def(ModuleDef::Function(def!(Fn))),
            SyntaxKind::STRUCT | SyntaxKind::ENUM | SyntaxKind::UNION => ItemLike::Def(ModuleDef::Adt(def!(Adt))),
            SyntaxKind::VARIANT => ItemLike::Def(ModuleDef::EnumVariant(def!(Variant))),
            SyntaxKind::CONST => ItemLike::Def(ModuleDef::Const(def!(Const))),
            SyntaxKind::STATIC => ItemLike::Def(ModuleDef::Static(def!(Static))),
            SyntaxKind::TRAIT => ItemLike::Def(ModuleDef::Trait(def!(Trait))),
            SyntaxKind::TYPE_ALIAS => ItemLike::Def(ModuleDef::TypeAlias(def!(TypeAlias))),
            SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => ItemLike::Def(ModuleDef::Macro(def!(Macro))),
            SyntaxKind::RECORD_FIELD => ItemLike::Field(def!(RecordField)),
            SyntaxKind::TUPLE_FIELD => ItemLike::Field(def!(TupleField)),
            SyntaxKind::IMPL => ItemLike::Impl(def!(Impl)),
            SyntaxKind::USE => ItemLike::Use(def!(Use)),
            SyntaxKind::EXTERN_BLOCK => ItemLike::ExternBlock(def!(ExternBlock)),
            SyntaxKind::EXTERN_CRATE => ItemLike::ExternCrate(def!(ExternCrate)),
            _ => return None,
        })
    }

    /// rustc's `lookup_deprecation_entry`, as hir-def reads it.
    pub(super) fn deprecation(self, db: &Db) -> Option<DeprecationEntry<'_>> {
        match self {
            ItemLike::Def(it) => it.deprecation(db),
            ItemLike::Field(it) => it.deprecation(db),
            ItemLike::Impl(it) => it.deprecation(db),
            ItemLike::Use(it) => it.deprecation(db),
            ItemLike::ExternBlock(it) => it.deprecation(db),
            ItemLike::ExternCrate(it) => it.deprecation(db),
        }
    }

    /// rustc's `lookup_stability`, as hir-def reads it.
    pub(super) fn stability(self, db: &Db) -> Option<&Stability> {
        match self {
            ItemLike::Def(it) => it.stability(db),
            ItemLike::Field(it) => it.stability(db),
            ItemLike::Impl(it) => it.stability(db),
            ItemLike::Use(it) => it.stability(db),
            ItemLike::ExternBlock(it) => it.stability(db),
            ItemLike::ExternCrate(it) => it.stability(db),
        }
    }
}

/// The shape of `variant`'s fields.
pub(super) fn kind(db: &Db, variant: Variant) -> StructKind {
    match variant {
        Variant::Struct(it) => it.kind(db),
        Variant::Union(it) => it.kind(db),
        Variant::EnumVariant(it) => it.kind(db),
    }
}

/// The item `item` is, if it is the item of a trait impl: the trait's item of its name, whose
/// stability and deprecation are the ones that count (`AssocContainer::TraitImpl`'s
/// `trait_item_def_id`).
pub(super) fn trait_item(db: &Db, item: ModuleDef) -> ModuleDef {
    let assoc = match item {
        ModuleDef::Function(it) => it.as_assoc_item(db),
        ModuleDef::Const(it) => it.as_assoc_item(db),
        ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
        _ => None,
    };
    let Some(assoc) = assoc else { return item };
    let AssocItemContainer::Impl(imp) = assoc.container(db) else { return item };
    let Some(trait_) = imp.trait_(db) else { return item };
    let Some(name) = assoc.name(db) else { return item };
    let found = trait_.items_iter(db).find(|it| {
        it.name(db).as_ref() == Some(&name)
            && matches!(
                (it, assoc),
                (AssocItem::Function(_), AssocItem::Function(_))
                    | (AssocItem::Const(_), AssocItem::Const(_))
                    | (AssocItem::TypeAlias(_), AssocItem::TypeAlias(_))
            )
    });
    match found {
        Some(AssocItem::Function(it)) => ModuleDef::Function(it),
        Some(AssocItem::Const(it)) => ModuleDef::Const(it),
        Some(AssocItem::TypeAlias(it)) => ModuleDef::TypeAlias(it),
        None => item,
    }
}

/// rustc's `with_no_trimmed_paths!(def_path_str(def))`: an item of this crate by its path
/// from the crate root, with no prefix; another crate's by the path it is visible through
/// from here; an associated item by what it is of, an inherent impl by its type when the
/// impl is next to the type and as `<impl T>` in its parent else; a field or variant by
/// what it is of; a constructor as its struct or variant.
///
/// Called only for a diagnostic that fires, as are the printers below it: their strings are
/// the edge.
pub(super) fn path(cx: &Cx<'_, '_>, def: Def) -> Option<String> {
    let db = cx.db;
    // alloc: a segment of the path a firing diagnostic prints
    let name = |it: hir::Name| it.display(db, cx.edition).to_string();
    match def {
        Def::Ctor(it) => path(cx, Def::variant(it)),
        Def::Field(it) => {
            // alloc: the path a firing diagnostic prints
            Some(format!("{}::{}", path(cx, Def::variant(it.parent_def(db)))?, name(it.name(db))))
        }
        Def::Item(ModuleDef::EnumVariant(it)) => {
            let parent = path(cx, Def::Item(ModuleDef::Adt(Adt::Enum(it.parent_enum(db)))))?;
            // alloc: the path a firing diagnostic prints
            Some(format!("{parent}::{}", name(it.name(db))))
        }
        Def::Item(item) => {
            let assoc = match item {
                ModuleDef::Function(it) => it.as_assoc_item(db),
                ModuleDef::Const(it) => it.as_assoc_item(db),
                ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
                _ => None,
            };
            if let Some(assoc) = assoc {
                let own = name(assoc.name(db)?);
                let value = !matches!(assoc, AssocItem::TypeAlias(_));
                let parent = match assoc.container(db) {
                    AssocItemContainer::Trait(it) => path(cx, Def::Item(ModuleDef::Trait(it)))?,
                    AssocItemContainer::Impl(it) => impl_path(cx, it, value)?,
                };
                // alloc: the path a firing diagnostic prints
                return Some(format!("{parent}::{own}"));
            }
            if def.krate(db)? == cx.krate {
                return node_path(cx, def.source(cx)?);
            }
            Some(visible_path(cx, item))
        }
    }
}

/// The path an item of another crate is visible through from this crate's root
/// (`try_print_visible_def_path`): through the name a dependency is known by here, else
/// the item's own path in its crate.
fn visible_path(cx: &Cx<'_, '_>, item: ModuleDef) -> String {
    let db = cx.db;
    let cfg = hir::FindPathConfig { prefer_no_std: false, prefer_prelude: false, prefer_absolute: false, allow_unstable: true };
    let root = cx.krate.root_module(db);
    let deps = cx.krate.dependencies(db);
    if let ModuleDef::Module(it) = item
        && it.is_crate_root(db)
        && let Some(dep) = deps.iter().find(|dep| dep.krate == it.krate(db))
    {
        // alloc: the path a firing diagnostic prints
        return dep.name.display(db, cx.edition).to_string();
    }
    if let Some(found) = root.find_use_path(db, hir::ItemInNs::from(item), hir::PrefixKind::Plain, cfg)
        && found.kind == hir::PathKind::Plain
        && found.segments().first().is_some_and(|first| deps.iter().any(|dep| &dep.name == first))
    {
        // alloc: the path a firing diagnostic prints
        return found.display(db, cx.edition).to_string();
    }
    let krate = item.module(db).map(|it| it.krate(db));
    // alloc: the crate segment of the path a firing diagnostic prints
    let crate_name = krate.and_then(|it| it.display_name(db)).map(|it| it.crate_name().to_string()).unwrap_or_default();
    match item {
        ModuleDef::Module(it) if it.is_crate_root(db) => crate_name,
        _ => match item.canonical_path(db, cx.edition) {
            // alloc: the path a firing diagnostic prints
            Some(path) => format!("{crate_name}::{path}"),
            None => crate_name,
        },
    }
}

/// The path of an impl's items' parent: the impl's type when it is an ADT next to the impl
/// (`Type::<Args>` in a value path), else `parent::<impl Type>`.
fn impl_path(cx: &Cx<'_, '_>, imp: hir::Impl, value: bool) -> Option<String> {
    let db = cx.db;
    let self_ty = imp.self_ty(db);
    let source = cx.vsema.source(imp);
    if let Some(adt) = self_ty.as_adt()
        && adt.module(db) == imp.module(db)
    {
        let mut printed = path(cx, Def::Item(ModuleDef::Adt(adt)))?;
        let args = source.and_then(|it| match it.value.self_ty()? {
            ast::Type::PathType(it) => it.path()?.segment()?.generic_arg_list(),
            _ => None,
        });
        if let Some(args) = args {
            printed.push_str(if value { "::<" } else { "<" });
            for (i, arg) in args.generic_args().enumerate() {
                if i > 0 {
                    printed.push_str(", ");
                }
                printed.push_str(arg.syntax().text());
            }
            printed.push('>');
        }
        return Some(printed);
    }
    let ty = match self_ty.as_adt() {
        Some(adt) => path(cx, Def::Item(ModuleDef::Adt(adt)))?,
        None => cx.ty(&self_ty),
    };
    let parent = match source.filter(|_| imp.module(db).krate(db) == cx.krate) {
        Some(source) => match def_parent(cx, source.map(|it| it.syntax())) {
            Some(parent) => Some(node_path(cx, parent)?),
            None => None,
        },
        None => {
            let module = imp.module(db);
            (!module.is_crate_root(db) || module.krate(db) != cx.krate)
                .then(|| visible_path(cx, ModuleDef::Module(module)))
        }
    };
    Some(match parent {
        // alloc: the path a firing diagnostic prints
        Some(parent) => format!("{parent}::<impl {ty}>"),
        // alloc: the path a firing diagnostic prints
        None => format!("<impl {ty}>"),
    })
}

/// The path of the definition of this crate written at `node`, from the crate root.
fn node_path<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<String> {
    let own = match ast::Impl::cast(node.value) {
        Some(it) => {
            let imp = cx.vsema.to_def(it)?;
            return impl_path(cx, imp, false);
        }
        None => segment(cx, node)?,
    };
    match def_parent(cx, node) {
        // alloc: the path a firing diagnostic prints
        Some(parent) => Some(format!("{}::{own}", node_path(cx, parent)?)),
        None => Some(own),
    }
}

/// The segment the definition at `node` adds to its parent's path.
fn segment<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<String> {
    if ast::ClosureExpr::can_cast(node.value.kind()) {
        let parent = def_parent(cx, node);
        let index = closure_index(cx, parent, node);
        // alloc: the path a firing diagnostic prints
        return Some(format!("{{closure#{index}}}"));
    }
    if let Some(it) = ast::Const::cast(node.value)
        && it.name().is_none()
    {
        // alloc: the path a firing diagnostic prints
        return Some("_".to_owned());
    }
    let named = ast::AnyHasName::cast(node.value)?;
    // alloc: the path a firing diagnostic prints
    Some(named.name()?.text().to_string())
}

/// Which of the closures of `parent` (the crate root when `None`) `closure` is, in the order
/// rustc numbers them: as they are written.
fn closure_index<'db>(cx: &Cx<'_, 'db>, parent: Option<InFile<Node<'db>>>, closure: InFile<Node<'db>>) -> usize {
    let Some(parent) = parent.filter(|it| it.file_id == closure.file_id) else { return 0 };
    let mut index = 0;
    for node in parent.value.descendants() {
        if node == closure.value {
            return index;
        }
        if ast::ClosureExpr::can_cast(node.kind())
            && def_parent(cx, closure.with_value(node)).is_some_and(|it| it.value == parent.value)
        {
            index += 1;
        }
    }
    index
}

/// Whether a definition at `node` is the parent of the definitions written in it.
pub(super) fn is_def_parent(node: Node<'_>) -> bool {
    use syntax::SyntaxKind::*;
    matches!(
        node.kind(),
        MODULE | FN | CONST | STATIC | STRUCT | ENUM | UNION | VARIANT | TRAIT | IMPL | TYPE_ALIAS | CLOSURE_EXPR
    )
}

/// The definition `node`'s is in (`opt_local_parent`): the nearest enclosing one, through a
/// macro's expansion to where it was called and from a module's file to its `mod`; `None`
/// at the crate root.
pub(super) fn def_parent<'db>(cx: &Cx<'_, 'db>, node: InFile<Node<'db>>) -> Option<InFile<Node<'db>>> {
    parent(cx, node, is_def_parent)
}

fn parent<'db>(
    cx: &Cx<'_, 'db>,
    node: InFile<Node<'db>>,
    stop: impl Fn(Node<'db>) -> bool,
) -> Option<InFile<Node<'db>>> {
    let (mut file, mut at) = (node.file_id, node.value.parent());
    loop {
        while let Some(node) = at {
            if stop(node) {
                return Some(InFile::new(file, node));
            }
            at = node.parent();
        }
        match file.macro_file() {
            Some(_) => {
                let call = cx.vsema.find_parent_file(file)?;
                (file, at) = (call.file_id, call.value.parent());
            }
            None => return cx.vsema.find_parent_file(file),
        }
    }
}
