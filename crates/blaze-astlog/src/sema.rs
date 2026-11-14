//! The fact source over blaze's front end: the view's files and the macro expansions in them as
//! syntax trees, and the defs and types the database gives for their nodes.
//!
//! Every value is a [`Node`]: a syntax node by its tree and its place there, or a def or a type
//! interned here. Trees, defs and types are interned as the rules reach them, so a run touches
//! only what its rules ask about.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Range;

use astlog_engine::{Facts, Row, Value};
use base_db::{Db, FileId};
use blaze_hash::HashMap;
use blaze_intent::Files;
use hir::{
    AsAssocItem as _, AssocItem, AssocItemContainer, Crate, DisplayTarget, HasVisibility as _,
    HirDisplay as _, HirFileId, ModuleDef, PathResolution, Type, Visibility, view::Semantics,
};
use memchr::memmem::Finder;
use std::alloc::Global;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, NodeId},
};

use crate::error::{DefPathSnafu, Error, WrongValueSnafu};
use crate::pattern::{Bound, Pattern, TypePattern, TypeShape, node_at};
use crate::program::{Atom, Blaze, Enclosing, Filter};

/// A value of blaze's rules.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Node {
    /// A syntax node: the `tree`th tree interned, at `id`.
    Syn { tree: u32, id: NodeId },
    /// The `n`th def interned.
    Def(u32),
    /// The `n`th type interned.
    Ty(u32),
}

/// What a def value is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Def {
    Item(ModuleDef),
    Field(hir::Field),
    Impl(hir::Impl),
}

/// A syntax tree a value is in.
#[derive(Clone)]
pub struct Tree<'a> {
    pub root: ast::Node<'a>,
    pub file: HirFileId,
    /// The view's path of the file, or of the file the macro call is in.
    pub path: Option<String>,
    /// The macro call this tree is the expansion of, in the tree it is in.
    pub call: Option<Node>,
}

/// blaze's facts over one view.
pub struct Sema<'a, F: Files> {
    pub db: &'a Db,
    pub sema: Semantics<'a>,
    pub files: &'a F,
    pub view: Vec<Crate>,
    /// The view's files' trees, as tree numbers, in the order the crates' modules list them.
    targets: Vec<u32>,
    target: DisplayTarget,
    trees: RefCell<Vec<Tree<'a>>>,
    tree_of: RefCell<HashMap<HirFileId, u32, Global>>,
    expansions: RefCell<HashMap<u32, Vec<u32>, Global>>,
    defs: RefCell<Vec<Def>>,
    def_ids: RefCell<HashMap<Def, u32, Global>>,
    types: RefCell<Vec<Type<'a>>>,
    paths: RefCell<HashMap<String, Vec<ModuleDef>, Global>>,
}

type Res<T> = Result<T, Error>;

impl<'a, F: Files> Sema<'a, F> {
    /// The facts of the view whose front end is `db` and whose files are `files`.
    ///
    /// # Errors
    ///
    /// A view with no crate the front end lowered.
    pub fn new(db: &'a Db, files: &'a F) -> Res<Sema<'a, F>> {
        let sema = Semantics::new(db);
        // alloc: the view's crates, once a run
        let view: Vec<Crate> =
            Crate::all(db).into_iter().filter(|it| files.path(it.root_file(db)).is_some()).collect();
        let Some(first) = view.first() else {
            return Err(crate::error::SourceError::Edit {
                what: "the view has no crate the front end lowered".to_owned(),
                at: None,
            }
            .into());
        };
        let target = first.to_display_target(db);
        let facts = Sema {
            db,
            sema,
            files,
            target,
            targets: Vec::new(),
            trees: RefCell::new(Vec::new()),
            tree_of: RefCell::new(HashMap::new_in(Global)),
            expansions: RefCell::new(HashMap::new_in(Global)),
            defs: RefCell::new(Vec::new()),
            def_ids: RefCell::new(HashMap::new_in(Global)),
            types: RefCell::new(Vec::new()),
            paths: RefCell::new(HashMap::new_in(Global)),
            view,
        };
        let mut seen: Vec<FileId> = Vec::new(); // alloc: the view's files, once a run
        let mut targets = Vec::new();
        for krate in &facts.view {
            for module in krate.modules(db) {
                let Some(file) = module.as_source_file_id(db) else { continue };
                let file = file.file_id(db);
                if seen.contains(&file) || files.path(file).is_none() {
                    continue;
                }
                seen.push(file);
                let root = facts.sema.parse(facts.sema.attach_first_edition(file)).syntax();
                targets.push(facts.tree(root));
            }
        }
        Ok(Sema { targets, ..facts })
    }

    /// The tree a node of `root`'s tree is in, interned.
    pub fn tree(&self, node: ast::Node<'a>) -> u32 {
        let file = self.sema.hir_file_for(node);
        if let Some(&at) = self.tree_of.borrow().get(&file) {
            return at;
        }
        let path = file
            .file_id()
            .and_then(|it| self.files.path(it.file_id(self.db)))
            .map(str::to_owned);
        let mut trees = self.trees.borrow_mut();
        let at = u32::try_from(trees.len()).expect("fewer than 2^32 trees");
        trees.push(Tree { root: node.tree_top(), file, path, call: None });
        self.tree_of.borrow_mut().insert(file, at);
        at
    }

    pub fn tree_at(&self, tree: u32) -> Tree<'a> {
        self.trees.borrow()[tree as usize].clone() // clone: a few words, the tree's handle
    }

    /// A syntax node as a value.
    pub fn syn(&self, node: ast::Node<'a>) -> Node {
        Node::Syn { tree: self.tree(node), id: node.id() }
    }

    /// The syntax node a value is, or `None` for a def or a type.
    pub fn view_of(&self, node: Node) -> Option<ast::Node<'a>> {
        let Node::Syn { tree, id } = node else { return None };
        Some(self.syn_at(tree, id))
    }

    /// The syntax node at `id` of the `tree`th tree.
    pub fn syn_at(&self, tree: u32, id: NodeId) -> ast::Node<'a> {
        node_at(self.trees.borrow()[tree as usize].root, id)
    }

    pub fn def(&self, def: Def) -> Node {
        if let Some(&at) = self.def_ids.borrow().get(&def) {
            return Node::Def(at);
        }
        let mut defs = self.defs.borrow_mut();
        let at = u32::try_from(defs.len()).expect("fewer than 2^32 defs");
        defs.push(def);
        self.def_ids.borrow_mut().insert(def, at);
        Node::Def(at)
    }

    pub fn def_at(&self, node: Node) -> Option<Def> {
        let Node::Def(at) = node else { return None };
        Some(self.defs.borrow()[at as usize])
    }

    fn ty(&self, ty: Type<'a>) -> Node {
        let mut types = self.types.borrow_mut();
        if let Some(at) = types.iter().position(|it| *it == ty) {
            return Node::Ty(u32::try_from(at).expect("fewer than 2^32 types"));
        }
        types.push(ty);
        Node::Ty(u32::try_from(types.len() - 1).expect("fewer than 2^32 types"))
    }

    fn ty_at(&self, node: Node) -> Option<Type<'a>> {
        let Node::Ty(at) = node else { return None };
        Some(self.types.borrow()[at as usize])
    }

    /// The trees the patterns search: each view file holding `literal` (or every one), then
    /// the macro expansions in it, depth first.
    fn searched(&self, literal: Option<&str>) -> Vec<u32> {
        let finder = literal.map(|it| Finder::new(it.as_bytes()));
        let mut out = Vec::new(); // alloc: the trees one atom searches
        for &tree in &self.targets {
            let root = self.tree_at(tree).root;
            if finder.as_ref().is_some_and(|it| it.find(root.text().as_bytes()).is_none()) {
                continue;
            }
            let mut stack = vec![tree];
            while let Some(tree) = stack.pop() {
                out.push(tree);
                let inner = self.expansions_of(tree);
                stack.extend(inner.iter().rev());
            }
        }
        out
    }

    /// The expansions of the macro calls written in `tree`, interned once.
    fn expansions_of(&self, tree: u32) -> Vec<u32> {
        if let Some(seen) = self.expansions.borrow().get(&tree) {
            return seen.clone(); // clone: a few tree numbers
        }
        let root = self.tree_at(tree).root;
        let mut out = Vec::new();
        for call in root.descendants().filter_map(ast::MacroCall::cast) {
            let Some(id) = self.sema.to_def(call) else { continue };
            let expanded = self.sema.expand(id).value;
            let at = self.tree(expanded);
            self.trees.borrow_mut()[at as usize].call = Some(self.syn(call.syntax()));
            let path = self.tree_at(tree).path;
            self.trees.borrow_mut()[at as usize].path = path;
            out.push(at);
        }
        self.expansions.borrow_mut().insert(tree, out.clone()); // clone: a few tree numbers
        out
    }

    /// `node` and its ancestors, out through the macro calls whose expansions hold them.
    pub fn ancestors_out(&self, node: ast::Node<'a>) -> Vec<ast::Node<'a>> {
        let mut out = Vec::new(); // alloc: one node's ancestors
        let mut at = Some(node);
        while let Some(node) = at {
            out.extend(node.ancestors());
            let tree = self.tree(node);
            at = self.tree_at(tree).call.and_then(|it| self.view_of(it));
        }
        out
    }

    /// The items a def path names: [`blaze_intent::replace::items_at`], then an associated
    /// item by name of the type or trait the rest of the path names.
    pub fn items_at(&self, path: &str, line: usize) -> Res<Vec<ModuleDef>> {
        let path = path.trim().trim_start_matches("::");
        if let Some(seen) = self.paths.borrow().get(path) {
            return Ok(seen.clone()); // clone: a few defs
        }
        let found = match blaze_intent::replace::items_at(self.db, &self.view, path) {
            Ok(found) => found,
            Err(why) => {
                let assoc = path.rsplit_once("::").and_then(|(owner, name)| {
                    let owners = blaze_intent::replace::items_at(self.db, &self.view, owner).ok()?;
                    let found: Vec<ModuleDef> = owners
                        .into_iter()
                        .flat_map(|owner| self.assoc_items(owner))
                        .filter(|it| it.name(self.db).is_some_and(|it| it.as_str() == name))
                        .collect();
                    (!found.is_empty()).then_some(found)
                });
                assoc.ok_or_else(|| {
                    DefPathSnafu { line, path: path.to_owned(), message: why }.build()
                })?
            }
        };
        self.paths.borrow_mut().insert(path.to_owned(), found.clone()); // clone: a few defs
        Ok(found)
    }

    fn assoc_items(&self, owner: ModuleDef) -> Vec<ModuleDef> {
        let db = self.db;
        let items: Vec<AssocItem> = match owner {
            ModuleDef::Trait(it) => it.items(db),
            ModuleDef::Adt(adt) => self
                .view
                .iter()
                .flat_map(|krate| hir::Impl::all_for_type(db, *krate, adt.ty(db)))
                .filter(|it| it.trait_(db).is_none())
                .flat_map(|it| it.items(db))
                .collect(),
            _ => Vec::new(),
        };
        let mut out: Vec<ModuleDef> = items
            .into_iter()
            .map(|it| match it {
                AssocItem::Function(it) => ModuleDef::Function(it),
                AssocItem::Const(it) => ModuleDef::Const(it),
                AssocItem::TypeAlias(it) => ModuleDef::TypeAlias(it),
            })
            .collect();
        out.dedup();
        out
    }

    /// The def a path, name, method call or field names.
    pub fn resolve(&self, node: ast::Node<'a>) -> Option<Def> {
        let sema = &self.sema;
        match node.kind() {
            SyntaxKind::PATH => match sema.resolve_path(ast::Path::cast(node)?)? {
                PathResolution::Def(def) => Some(Def::Item(def)),
                PathResolution::SelfType(it) => Some(Def::Impl(it)),
                _ => None,
            },
            SyntaxKind::PATH_EXPR | SyntaxKind::PATH_TYPE | SyntaxKind::PATH_PAT => {
                self.resolve(node.children().find(|it| it.kind() == SyntaxKind::PATH)?)
            }
            SyntaxKind::NAME_REF => {
                let parent = node.parent()?;
                match parent.kind() {
                    SyntaxKind::PATH_SEGMENT => self.resolve(parent.parent()?),
                    SyntaxKind::METHOD_CALL_EXPR | SyntaxKind::FIELD_EXPR => self.resolve(parent),
                    _ => None,
                }
            }
            SyntaxKind::METHOD_CALL_EXPR => sema
                .resolve_method_call(ast::MethodCallExpr::cast(node)?)
                .map(|it| Def::Item(ModuleDef::Function(it))),
            SyntaxKind::FIELD_EXPR => match sema.resolve_field(ast::FieldExpr::cast(node)?)? {
                either::Either::Left(field) => Some(Def::Field(field)),
                either::Either::Right(_) => None,
            },
            _ => None,
        }
    }

    /// The fn a call or method call runs.
    fn callee(&self, node: ast::Node<'a>) -> Option<Def> {
        match node.kind() {
            SyntaxKind::METHOD_CALL_EXPR => self.resolve(node),
            SyntaxKind::CALL_EXPR => {
                let callee = node.children().next()?;
                match self.resolve(callee)? {
                    def @ Def::Item(ModuleDef::Function(_)) => Some(def),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// The def an item's syntax declares.
    pub fn defines(&self, node: ast::Node<'a>) -> Option<Def> {
        let sema = &self.sema;
        let item = |def: ModuleDef| Some(Def::Item(def));
        match node.kind() {
            SyntaxKind::FN => item(sema.to_fn_def(ast::Fn::cast(node)?)?.into()),
            SyntaxKind::STRUCT => item(ModuleDef::Adt(sema.to_def(ast::Struct::cast(node)?)?.into())),
            SyntaxKind::ENUM => item(ModuleDef::Adt(sema.to_def(ast::Enum::cast(node)?)?.into())),
            SyntaxKind::UNION => item(ModuleDef::Adt(sema.to_def(ast::Union::cast(node)?)?.into())),
            SyntaxKind::TRAIT => item(sema.to_def(ast::Trait::cast(node)?)?.into()),
            SyntaxKind::CONST => item(sema.to_def(ast::Const::cast(node)?)?.into()),
            SyntaxKind::STATIC => item(sema.to_def(ast::Static::cast(node)?)?.into()),
            SyntaxKind::TYPE_ALIAS => item(sema.to_def(ast::TypeAlias::cast(node)?)?.into()),
            SyntaxKind::MODULE => item(sema.to_def(ast::Module::cast(node)?)?.into()),
            SyntaxKind::VARIANT => item(sema.to_def(ast::Variant::cast(node)?)?.into()),
            SyntaxKind::RECORD_FIELD => Some(Def::Field(sema.to_def(ast::RecordField::cast(node)?)?)),
            SyntaxKind::IMPL => Some(Def::Impl(sema.to_impl_def(ast::Impl::cast(node)?)?)),
            _ => None,
        }
    }

    /// The syntax that declares a def, if the front end has it.
    pub fn source_of(&self, def: Def) -> Option<ast::Node<'a>> {
        let sema = &self.sema;
        Some(match def {
            Def::Item(ModuleDef::Function(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::Adt(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::EnumVariant(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::Const(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::Static(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::Trait(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::TypeAlias(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::Macro(it)) => sema.source(it)?.value.syntax(),
            Def::Item(ModuleDef::Module(it)) => {
                let decl = it.declaration_source_view(self.db)?;
                let node = decl.value.syntax();
                self.sema.parse_or_expand(decl.file_id);
                node
            }
            Def::Item(ModuleDef::BuiltinType(_)) => return None,
            Def::Field(it) => sema.source(it)?.value.syntax(),
            Def::Impl(it) => sema.source(it)?.value.syntax(),
        })
    }

    fn name_of(&self, def: Def) -> Option<String> {
        let db = self.db;
        match def {
            Def::Item(it) => it.name(db).map(|it| it.as_str().to_owned()),
            Def::Field(it) => Some(it.name(db).as_str().to_owned()),
            Def::Impl(_) => None,
        }
    }

    /// The def path of `def` from its crate's name: `core::option::Option::unwrap` for an
    /// associated item, under its type's or trait's name.
    pub fn def_path(&self, def: Def) -> Option<String> {
        let db = self.db;
        let (module, last) = match def {
            Def::Item(ModuleDef::Module(it)) => (it, None),
            Def::Item(it) => (it.module(db)?, Some(it)),
            Def::Field(it) => (it.parent_def(db).module(db), None),
            Def::Impl(it) => (it.module(db), None),
        };
        let krate = module.krate(db).base().rust_name(db).ok()?.as_str().to_owned();
        let mut segments = vec![krate];
        segments.extend(module.path_segments(db).map(|it| it.as_str().to_owned()));
        let owner = match last {
            Some(ModuleDef::Function(it)) => it.as_assoc_item(db).map(|it| it.container(db)),
            Some(ModuleDef::Const(it)) => it.as_assoc_item(db).map(|it| it.container(db)),
            Some(ModuleDef::TypeAlias(it)) => it.as_assoc_item(db).map(|it| it.container(db)),
            _ => None,
        };
        match owner {
            Some(AssocItemContainer::Trait(it)) => segments.push(it.name(db).as_str().to_owned()),
            Some(AssocItemContainer::Impl(it)) => {
                segments.push(it.self_ty(db).as_adt()?.name(db).as_str().to_owned());
            }
            None => {}
        }
        if let Def::Field(field) = def {
            segments.push(hir::Variant::from(field.parent_def(db)).name(db).as_str().to_owned());
        }
        if let Some(name) = self.name_of(def) {
            segments.push(name);
        }
        Some(segments.join("::"))
    }

    fn display(&self, ty: Type<'a>) -> String {
        ty.display(self.db, self.target).to_string()
    }

    fn type_matches(
        &self,
        pattern: &TypePattern,
        node: ast::Node<'_>,
        ty: Type<'a>,
        out: &mut Vec<Option<Node>>,
    ) -> Res<bool> {
        let db = self.db;
        let Some(shape) = pattern.shape(node) else { return Ok(false) };
        Ok(match shape {
            TypeShape::Wild => true,
            TypeShape::Var(var) => {
                let value = self.ty(ty);
                match out[var] {
                    Some(seen) => seen == value,
                    None => {
                        out[var] = Some(value);
                        true
                    }
                }
            }
            TypeShape::Def(path, args) => {
                let defs = self.items_at(&pattern.paths[path], pattern.line)?;
                let is = defs.iter().any(|def| match def {
                    ModuleDef::Adt(adt) => ty.as_adt() == Some(*adt),
                    ModuleDef::BuiltinType(it) => ty.as_builtin() == Some(*it),
                    _ => false,
                });
                if !is {
                    return Ok(false);
                }
                let have: Vec<Type<'a>> = ty.type_arguments().collect();
                if args.len() > have.len() {
                    return Ok(false);
                }
                for (arg, have) in args.into_iter().zip(have) {
                    if !self.type_matches(pattern, arg, have, out)? {
                        return Ok(false);
                    }
                }
                true
            }
            TypeShape::Name(name) => {
                if let Some(builtin) = ty.as_builtin() {
                    builtin.name().as_str() == name
                } else if let Some(adt) = ty.as_adt() {
                    adt.name(db).as_str() == name
                } else {
                    self.display(ty) == name
                }
            }
            TypeShape::Ref { mutable, inner } => match ty.as_reference() {
                Some((pointee, mutability)) if mutability.is_mut() == mutable => {
                    self.type_matches(pattern, inner, pointee, out)?
                }
                _ => false,
            },
            TypeShape::Tuple(items) => {
                let fields = ty.tuple_fields(db);
                if !ty.is_tuple() && !(items.is_empty() && ty.is_unit()) || fields.len() != items.len() {
                    return Ok(false);
                }
                for (item, field) in items.into_iter().zip(fields) {
                    if !self.type_matches(pattern, item, field, out)? {
                        return Ok(false);
                    }
                }
                true
            }
            TypeShape::Slice(inner) => match ty.as_slice() {
                Some(elem) => self.type_matches(pattern, inner, elem, out)?,
                None => false,
            },
            TypeShape::Array(inner) => match ty.as_array(db) {
                Some((elem, _)) => self.type_matches(pattern, inner, elem, out)?,
                None => false,
            },
        })
    }

    /// Whether any attribute of `item` is `name` (its path's first segment).
    fn has_attribute(item: ast::Node<'_>, name: &str) -> bool {
        item.children().filter(|it| it.kind() == SyntaxKind::ATTR).any(|attr| {
            attr.descendants()
                .find(|it| it.kind() == SyntaxKind::PATH_SEGMENT)
                .is_some_and(|segment| segment.text() == name)
        })
    }

    /// Whether `item` carries `#[cfg(test)]` or a `#[test]`-like attribute (`test`, `tokio::test`).
    fn test_item(item: ast::Node<'_>) -> bool {
        item.children().filter(|it| it.kind() == SyntaxKind::ATTR).any(|attr| {
            let text: String = attr.text().chars().filter(|c| !c.is_whitespace()).collect();
            text == "#[test]" || text.ends_with("::test]") || text == "#[cfg(test)]"
        })
    }

    fn filter(&self, filter: Filter, node: ast::Node<'a>) -> bool {
        let tree = self.tree_at(self.tree(node));
        match filter {
            // The two split an expansion's nodes: those whose text is the call's input, which
            // an edit can reach, and those the macro made, which it can't.
            Filter::InMacroExpansion => tree.call.is_some() && self.sema.original_range_opt(node).is_none(),
            Filter::InMacroInput => tree.call.is_some() && self.sema.original_range_opt(node).is_some(),
            Filter::InUnsafe => self.ancestors_out(node).into_iter().any(|it| {
                matches!(it.kind(), SyntaxKind::BLOCK_EXPR | SyntaxKind::FN)
                    && it.child_token(SyntaxKind::UNSAFE_KW).is_some()
            }),
            Filter::InTest => self.ancestors_out(node).into_iter().any(Self::test_item),
            Filter::CfgOff => self.ancestors_out(node).into_iter().any(|it| {
                Self::has_attribute(it, "cfg") && self.defines(it).is_none() && Self::declares(it)
            }),
        }
    }

    /// Whether `node` is an item [`Sema::defines`] knows how to map.
    fn declares(node: ast::Node<'_>) -> bool {
        matches!(
            node.kind(),
            SyntaxKind::FN
                | SyntaxKind::STRUCT
                | SyntaxKind::ENUM
                | SyntaxKind::UNION
                | SyntaxKind::TRAIT
                | SyntaxKind::CONST
                | SyntaxKind::STATIC
                | SyntaxKind::TYPE_ALIAS
                | SyntaxKind::MODULE
                | SyntaxKind::IMPL
        )
    }

    fn enclosing(&self, kind: Enclosing, node: ast::Node<'a>) -> Option<Def> {
        let mut up = self.ancestors_out(node).into_iter().skip(1);
        match kind {
            Enclosing::Fn => up.find(|it| it.kind() == SyntaxKind::FN).and_then(|it| self.defines(it)),
            Enclosing::Impl => {
                up.find(|it| it.kind() == SyntaxKind::IMPL).and_then(|it| self.defines(it))
            }
            Enclosing::Module => {
                Some(Def::Item(ModuleDef::Module(self.sema.scope(node)?.module())))
            }
        }
    }

    fn crate_of(&self, value: Node) -> Option<String> {
        let krate = match value {
            Node::Syn { .. } => self.sema.scope(self.view_of(value)?)?.krate(),
            Node::Def(_) => match self.def_at(value)? {
                Def::Item(it) => it.module(self.db)?.krate(self.db),
                Def::Field(it) => it.parent_def(self.db).module(self.db).krate(self.db),
                Def::Impl(it) => it.module(self.db).krate(self.db),
            },
            Node::Ty(_) => return None,
        };
        Some(krate.base().rust_name(self.db).ok()?.as_str().to_owned())
    }

    fn def_kind(&self, def: Def) -> &'static str {
        match def {
            Def::Item(ModuleDef::Function(_)) => "fn",
            Def::Item(ModuleDef::Adt(hir::Adt::Struct(_))) => "struct",
            Def::Item(ModuleDef::Adt(hir::Adt::Enum(_))) => "enum",
            Def::Item(ModuleDef::Adt(hir::Adt::Union(_))) => "union",
            Def::Item(ModuleDef::EnumVariant(_)) => "variant",
            Def::Item(ModuleDef::Const(_)) => "const",
            Def::Item(ModuleDef::Static(_)) => "static",
            Def::Item(ModuleDef::Trait(_)) => "trait",
            Def::Item(ModuleDef::TypeAlias(_)) => "type",
            Def::Item(ModuleDef::Macro(_)) => "macro",
            Def::Item(ModuleDef::Module(_)) => "module",
            Def::Item(ModuleDef::BuiltinType(_)) => "builtin",
            Def::Field(_) => "field",
            Def::Impl(_) => "impl",
        }
    }

    fn visibility(&self, def: Def) -> Option<&'static str> {
        let db = self.db;
        let (visibility, home) = match def {
            Def::Item(ModuleDef::BuiltinType(_)) | Def::Impl(_) => return None,
            Def::Item(it) => (it.visibility(db), it.module(db)),
            Def::Field(it) => (it.visibility(db), Some(it.parent_def(db).module(db))),
        };
        Some(match visibility {
            Visibility::Public => "pub",
            Visibility::PubCrate(_) => "pub(crate)",
            Visibility::Module(module, _) => {
                if home.is_some_and(|home| hir_def::ModuleId::from(home) == module) {
                    "private"
                } else {
                    "pub(in)"
                }
            }
        })
    }

    /// Every name, path and method call of the searched trees that resolves to `def`.
    fn references(&self, def: Def, out: &mut Vec<Node>) {
        let Some(name) = self.name_of(def) else { return };
        let finder = Finder::new(name.as_bytes());
        for &tree in &self.targets {
            let root = self.tree_at(tree).root;
            let text = root.text().as_bytes();
            for start in finder.find_iter(text) {
                let at = syntax::TextSize::from(u32::try_from(start).expect("a file under 4 GiB"));
                let Some(token) = root.token_at_offset(at).right_biased() else { continue };
                if token.kind() != SyntaxKind::IDENT || token.text() != name {
                    continue;
                }
                for mapped in self.sema.descend_into_macros_exact(token) {
                    let Some(name_ref) = mapped.parent().filter(|it| it.kind() == SyntaxKind::NAME_REF)
                    else {
                        continue;
                    };
                    if self.resolve(name_ref) != Some(def) {
                        continue;
                    }
                    let shown = match name_ref.parent() {
                        Some(segment) if segment.kind() == SyntaxKind::PATH_SEGMENT => {
                            segment.parent().unwrap_or(name_ref)
                        }
                        _ => name_ref,
                    };
                    out.push(self.syn(shown));
                }
            }
        }
    }

    fn need_syn(&self, value: &Option<Value<Node>>, name: &'static str, line: usize) -> Res<ast::Node<'a>> {
        match value {
            Some(Value::Node(node)) => self.view_of(*node).ok_or_else(|| {
                WrongValueSnafu { line, name, needs: "a syntax node", arg: "its first argument", what: "a def or a type" }
                    .build()
                    .into()
            }),
            other => WrongValueSnafu {
                line,
                name,
                needs: "a syntax node",
                arg: "its first argument",
                what: other.as_ref().map_or("unbound", Value::what),
            }
            .fail()
            .map_err(Error::from),
        }
    }

    fn need_def(&self, value: &Option<Value<Node>>, name: &'static str, line: usize) -> Res<Option<Def>> {
        match value {
            Some(Value::Node(node @ Node::Def(_))) => Ok(self.def_at(*node)),
            Some(Value::Node(node @ Node::Syn { .. })) => {
                Ok(self.view_of(*node).and_then(|it| self.defines(it)))
            }
            other => WrongValueSnafu {
                line,
                name,
                needs: "a def",
                arg: "its first argument",
                what: other.as_ref().map_or("unbound", |it| match it {
                    Value::Node(_) => "a type",
                    it => it.what(),
                }),
            }
            .fail()
            .map_err(Error::from),
        }
    }

    fn pattern_rows(
        &self,
        pattern: &Pattern,
        inputs: &[Option<Value<Node>>],
        out: &mut Vec<Row<Node>>,
    ) -> Res<()> {
        let candidates: Vec<ast::Node<'a>> = match &inputs[0] {
            Some(_) => vec![self.need_syn(&inputs[0], "a pattern", pattern.line)?],
            None => self
                .searched(pattern.literal.as_deref())
                .into_iter()
                .flat_map(|tree| {
                    let root = self.tree_at(tree).root;
                    root.descendants().filter(|it| it.kind() == pattern.root_kind).collect::<Vec<_>>()
                })
                .collect(),
        };
        let mut resolves = |path: usize, target: ast::Node<'a>| -> Res<bool> {
            let defs = self.items_at(&pattern.paths[path], pattern.line)?;
            Ok(match self.resolve(target) {
                Some(Def::Item(def)) => defs.contains(&def),
                _ => false,
            })
        };
        for node in candidates {
            let Some(bound) = pattern.matches(node, &mut resolves)? else { continue };
            let mut row = Vec::with_capacity(1 + bound.len());
            row.push(Value::Node(self.syn(node)));
            for value in bound {
                row.push(match value {
                    Bound::Node(node) => Value::Node(self.syn(node)),
                    Bound::Run(parent, range) => {
                        let start = usize::from(range.start() - parent.text_range().start());
                        let text = &parent.text()[start..start + usize::from(range.len())];
                        Value::Text(std::sync::Arc::from(text))
                    }
                });
            }
            out.push(row);
        }
        Ok(())
    }
}

impl<'a, F: Files> Facts for Sema<'a, F> {
    type Lang = Blaze;
    type Node = Node;

    fn solve(
        &self,
        atom: &Atom,
        line: usize,
        inputs: &[Option<Value<Node>>],
        out: &mut Vec<Row<Node>>,
    ) -> Res<()> {
        let node = |value: Node| Value::Node(value);
        let text = |value: &str| Value::Text(std::sync::Arc::from(value));
        match atom {
            Atom::Pattern(pattern) => self.pattern_rows(pattern, inputs, out)?,
            Atom::Syntax(kind) => match &inputs[0] {
                Some(_) => {
                    let at = self.need_syn(&inputs[0], "syntax", line)?;
                    if at.kind() == *kind {
                        out.push(vec![node(self.syn(at))]);
                    }
                }
                None => {
                    for tree in self.searched(None) {
                        let root = self.tree_at(tree).root;
                        for at in root.descendants().filter(|it| it.kind() == *kind) {
                            out.push(vec![node(self.syn(at))]);
                        }
                    }
                }
            },
            Atom::Resolves => {
                let at = self.need_syn(&inputs[0], "resolves", line)?;
                if let Some(def) = self.resolve(at) {
                    out.push(vec![node(self.syn(at)), node(self.def(def))]);
                }
            }
            Atom::Callee => {
                let at = self.need_syn(&inputs[0], "callee", line)?;
                if let Some(def) = self.callee(at) {
                    out.push(vec![node(self.syn(at)), node(self.def(def))]);
                }
            }
            Atom::DefPath => match (&inputs[0], &inputs[1]) {
                (Some(_), _) => {
                    let Some(def) = self.need_def(&inputs[0], "def-path", line)? else { return Ok(()) };
                    if let Some(path) = self.def_path(def) {
                        out.push(vec![node(self.def(def)), text(&path)]);
                    }
                }
                (None, Some(Value::Text(path))) => {
                    for def in self.items_at(path, line)? {
                        out.push(vec![node(self.def(Def::Item(def))), text(path)]);
                    }
                }
                (None, other) => {
                    return WrongValueSnafu {
                        line,
                        name: "def-path",
                        needs: "a def or a path's text",
                        arg: "its second argument",
                        what: other.as_ref().map_or("unbound", Value::what),
                    }
                    .fail()
                    .map_err(Error::from);
                }
            },
            Atom::TypeOf => {
                let at = self.need_syn(&inputs[0], "type-of", line)?;
                let ty = if let Some(expr) = ast::Expr::cast(at) {
                    self.sema.type_of_expr(expr).map(|it| it.original)
                } else if let Some(pat) = ast::Pat::cast(at) {
                    self.sema.type_of_pat(pat).map(|it| it.original)
                } else {
                    None
                };
                if let Some(ty) = ty {
                    out.push(vec![node(self.syn(at)), node(self.ty(ty))]);
                }
            }
            Atom::TypeMatches(pattern) => {
                let Some(Value::Node(at)) = inputs[0] else {
                    return WrongValueSnafu {
                        line,
                        name: "type-matches",
                        needs: "a type",
                        arg: "its first argument",
                        what: inputs[0].as_ref().map_or("unbound", Value::what),
                    }
                    .fail()
                    .map_err(Error::from);
                };
                let Some(ty) = self.ty_at(at) else { return Ok(()) };
                let mut bound = vec![None; pattern.vars.len()];
                if self.type_matches(pattern, pattern.root(), ty, &mut bound)? {
                    let mut row = vec![node(at)];
                    row.extend(bound.into_iter().map(|it| node(it.expect("a match binds every variable"))));
                    out.push(row);
                }
            }
            Atom::Implements(path) => {
                let Some(Value::Node(at)) = inputs[0] else { return Ok(()) };
                let Some(ty) = self.ty_at(at) else { return Ok(()) };
                let traits: Vec<hir::Trait> = self
                    .items_at(path, line)?
                    .into_iter()
                    .filter_map(|it| match it {
                        ModuleDef::Trait(it) => Some(it),
                        _ => None,
                    })
                    .collect();
                if traits.is_empty() {
                    return DefPathSnafu { line, path: path.clone(), message: "names no trait".to_owned() }
                        .fail()
                        .map_err(Error::from);
                }
                if traits.iter().any(|it| ty.impls_trait(self.db, *it, &[])) {
                    out.push(vec![node(at)]);
                }
            }
            Atom::Enclosing(kind) => {
                let at = self.need_syn(&inputs[0], "enclosing-*", line)?;
                if let Some(def) = self.enclosing(*kind, at) {
                    out.push(vec![node(self.syn(at)), node(self.def(def))]);
                }
            }
            Atom::CrateOf => {
                let Some(Value::Node(at)) = inputs[0] else { return Ok(()) };
                if let Some(name) = self.crate_of(at) {
                    out.push(vec![node(at), text(&name)]);
                }
            }
            Atom::FnParam | Atom::FnReturn => {
                let Some(def) = self.need_def(&inputs[0], "fn-param", line)? else { return Ok(()) };
                let Def::Item(ModuleDef::Function(function)) = def else { return Ok(()) };
                let d = node(self.def(def));
                if matches!(atom, Atom::FnReturn) {
                    out.push(vec![d, node(self.ty(function.ret_type(self.db)))]);
                } else {
                    for (index, param) in function.assoc_fn_params(self.db).into_iter().enumerate() {
                        let index = i64::try_from(index).expect("fewer than 2^63 parameters");
                        out.push(vec![d.clone(), Value::Int(index), node(self.ty(*param.ty()))]);
                    }
                }
            }
            Atom::DefKind => {
                let Some(def) = self.need_def(&inputs[0], "def-kind", line)? else { return Ok(()) };
                out.push(vec![node(self.def(def)), text(self.def_kind(def))]);
            }
            Atom::Visibility => {
                let Some(def) = self.need_def(&inputs[0], "visibility", line)? else { return Ok(()) };
                if let Some(shown) = self.visibility(def) {
                    out.push(vec![node(self.def(def)), text(shown)]);
                }
            }
            Atom::Attribute(name) => {
                let item = match &inputs[0] {
                    Some(Value::Node(at @ Node::Syn { .. })) => self.view_of(*at),
                    Some(Value::Node(at @ Node::Def(_))) => {
                        self.def_at(*at).and_then(|it| self.source_of(it))
                    }
                    _ => None,
                };
                if item.is_some_and(|it| Self::has_attribute(it, name))
                    && let Some(value) = inputs[0].clone()
                {
                    out.push(vec![value]);
                }
            }
            Atom::Defines => match (&inputs[0], &inputs[1]) {
                (Some(_), _) => {
                    let at = self.need_syn(&inputs[0], "defines", line)?;
                    if let Some(def) = self.defines(at) {
                        out.push(vec![node(self.syn(at)), node(self.def(def))]);
                    }
                }
                (None, Some(Value::Node(def @ Node::Def(_)))) => {
                    if let Some(source) = self.def_at(*def).and_then(|it| self.source_of(it)) {
                        out.push(vec![node(self.syn(source)), node(*def)]);
                    }
                }
                _ => {}
            },
            Atom::References => {
                let Some(def) = self.need_def(&inputs[0], "references", line)? else { return Ok(()) };
                let mut found = Vec::new();
                self.references(def, &mut found);
                let d = node(self.def(def));
                out.extend(found.into_iter().map(|it| vec![d.clone(), node(it)]));
            }
            Atom::Filter(filter) => {
                let at = self.need_syn(&inputs[0], "a filter", line)?;
                if self.filter(*filter, at) {
                    out.push(vec![node(self.syn(at))]);
                }
            }
        }
        Ok(())
    }

    fn parent(&self, node: Node) -> Option<Node> {
        let view = self.view_of(node)?;
        view.parent().map(|it| self.syn(it))
    }

    fn kind(&self, node: Node) -> Cow<'_, str> {
        match node {
            Node::Syn { .. } => match self.view_of(node) {
                Some(view) => Cow::Owned(format!("{:?}", view.kind())),
                None => Cow::Borrowed("syntax"),
            },
            Node::Def(_) => Cow::Borrowed(self.def_at(node).map_or("def", |it| self.def_kind(it))),
            Node::Ty(_) => Cow::Borrowed("type"),
        }
    }

    fn text(&self, node: Node) -> Cow<'_, str> {
        match node {
            Node::Syn { .. } => self.view_of(node).map_or(Cow::Borrowed(""), |it| Cow::Borrowed(it.text())),
            Node::Def(_) => Cow::Owned(
                self.def_at(node).and_then(|it| self.def_path(it)).unwrap_or_default(),
            ),
            Node::Ty(_) => Cow::Owned(self.ty_at(node).map(|it| self.display(it)).unwrap_or_default()),
        }
    }

    fn span(&self, node: Node) -> Option<(usize, Range<usize>)> {
        let Node::Syn { tree, .. } = node else { return None };
        let range = self.view_of(node)?.text_range();
        Some((tree as usize, usize::from(range.start())..usize::from(range.end())))
    }

    fn path(&self, file: usize) -> Cow<'_, str> {
        let tree = self.trees.borrow()[file].clone();
        match (tree.path, tree.call) {
            (Some(path), None) => Cow::Owned(path),
            (Some(path), Some(_)) => Cow::Owned(format!("{path} (in a macro expansion)")),
            (None, _) => Cow::Borrowed("(outside the view)"),
        }
    }
}
