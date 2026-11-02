//! Rename: the item's name and every reference to it, each found by the front end's name
//! resolution (paths, method calls, fields, record fields and patterns, tokens a macro
//! expands), never by text search. Text search only picks which files to look in: those of the
//! item's crate, and of the crates that depend on it when it is public, whose bytes hold the
//! name.
//!
//! An occurrence of the name the front end can't place (unresolved, cfg'd out under the view's
//! configuration, in a `macro_rules!` body or in tokens no macro expanded) is an error naming
//! where it is: the rename would otherwise miss a reference without saying so.

use base_db::{Db, FileId};
use blaze_api::{ApiError, How, Target};
use blaze_hash::HashSet;
use blaze_ids::radix;
use blaze_vfs::Op;
use either::Either;
use hir::{
    AsAssocItem, Crate, Field, HasVisibility, ModuleDef, PathResolution, Trait, Visibility,
    view::Semantics,
};
use syntax::{
    LexedStr, NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use crate::{
    Edits, Files,
    edit::{At, Touch},
    path,
};

/// What a rename renames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Def {
    Item(ModuleDef),
    Field(Field),
}

/// What an occurrence of the old name is to the renamed item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    No,
    /// The name, or a reference by it: replaced.
    Name,
    /// A field shorthand (`S { x }`) of the renamed field: the new name and a colon go before
    /// it.
    Shorthand,
    /// A field shorthand whose value is the renamed item: the field's name and a colon stay
    /// before the new name.
    ShorthandValue,
}

/// One rewrite, in the base text of the request's `file`th file.
struct Hit {
    file: u32,
    range: TextRange,
    insert: String,
}

/// Renames the item `target` names to `to`.
pub(crate) fn rename(cx: &mut Edits<'_, impl Files>, target: &Target, to: &str) -> Result<(), ApiError> {
    let fail = |what: String| ApiError::new(what, Some(target.file.clone()));
    let home = cx.open(&target.file).map_err(fail)?;
    let node = path::find(cx.roots[home as usize], &target.item)
        .map_err(|error| fail(format!("{}: {error}", target.file)))?;
    let edition = cx.working[home as usize].edition;
    let old = path::name_of(node)
        .filter(|_| node.child_where(|it| it == SyntaxKind::NAME).is_some())
        .ok_or_else(|| fail(format!("`{}` has no name to rename", target.item)))?;
    let bare = old.strip_prefix("r#").unwrap_or(&old).to_owned();
    match LexedStr::single_token(edition, to) {
        Some((SyntaxKind::IDENT, None)) if to.strip_prefix("r#").unwrap_or(to) != bare => {}
        Some((SyntaxKind::IDENT, None)) => {
            return Err(fail(format!("`{}` is already named `{to}`", target.item)));
        }
        _ => return Err(fail(format!("`{to}` is no identifier"))),
    }
    let def = def_of(&cx.sema, node).ok_or_else(|| {
        fail(format!(
            "the front end has no item for `{}` in {}: cfg'd out under this view's configuration",
            target.item, target.file
        ))
    })?;
    if let Def::Item(item) = def
        && let Some(of) = item.as_assoc_item(cx.db).and_then(|it| it.implemented_trait(cx.db))
    {
        return Err(fail(format!(
            "`{}` implements an item of trait `{}`: rename it in the trait, and its impls follow",
            target.item,
            of.name(cx.db).as_str()
        )));
    }

    let mut hits = Vec::new();
    let finder = memchr::memmem::Finder::new(bare.as_bytes());
    let files = cx.files;
    for file in files_of(cx.db, def) {
        let path = files
            .path(file)
            .ok_or_else(|| fail(format!("a file of a crate that can see `{}` is outside the view", target.item)))?;
        let (_, bytes) = files.file(path).ok_or_else(|| fail(format!("{path} is not in the view")))?;
        if finder.find(bytes).is_none() {
            continue;
        }
        let index = cx.open_id(file).map_err(fail)?;
        let root = cx.roots[index as usize];
        // Each match of the name goes straight to its token (a binary search a level), so the
        // cost is the matches', not the file's nodes'.
        for at in finder.find_iter(root.file().text().as_bytes()) {
            let Some(token) = ident_at(root, at, bare.len()) else { continue };
            let class = classify(cx, token, def, node).map_err(|what| {
                let text = cx.working[index as usize].text();
                let at = usize::from(token.text_range().start());
                let line = memchr::memchr_iter(b'\n', &text.as_bytes()[..at]).count() + 1;
                ApiError::new(format!("{path}:{line}: {what}"), Some(format!("{path}:{line}")))
            })?;
            let range = token.text_range();
            // alloc: one replacement a reference rewritten, which its splice owns
            let (range, insert) = match class {
                Class::No => continue,
                Class::Name => (range, to.to_owned()),
                Class::Shorthand => (TextRange::empty(range.start()), format!("{to}: ")),
                Class::ShorthandValue => (range, format!("{}: {to}", token.text())),
            };
            cx.touches.push(touch_of(index, token));
            hits.push(Hit { file: index, range, insert });
        }
    }
    if let Some(top) = path::top_of(node).filter(|top| *top == node) {
        let at = match path::scope_of(top) {
            Some(scope) => At::Item(scope.text_range().start()),
            None => At::File,
        };
        cx.touches.push(Touch { file: home, at, how: How::Scope });
    }

    radix::sort_by_key(&mut hits, |it| !radix::pack_32(it.file, it.range.start().into()));
    for hit in hits {
        let file = &mut cx.working[hit.file as usize];
        let now = file.to_now(hit.range).ok_or_else(|| {
            fail(format!(
                "{}: the rename of `{}` reaches text an earlier rename of this request replaced",
                file.path, target.item
            ))
        })?;
        file.splice(now, hit.insert).map_err(fail)?;
    }
    if let Def::Item(ModuleDef::Module(module)) = def {
        moves(cx, module, node, &bare, to).map_err(fail)?;
    }
    Ok(())
}

/// The identifier token whose name, `r#` cut, is the `len` bytes at `at` of `root`'s text;
/// `None` when they are part of another token or of a longer name. A name never overlaps
/// itself at a token's start (what comes before a token's start is no identifier character),
/// so the matches of a non-overlapping search reach every such token.
pub(crate) fn ident_at(root: ast::Node<'_>, at: usize, len: usize) -> Option<ast::Token<'_>> {
    let range = TextRange::at(TextSize::try_from(at).ok()?, TextSize::try_from(len).ok()?);
    let NodeOrToken::Token(token) = root.covering_element(range) else { return None };
    let own = token.text_range();
    let start = match token.text().starts_with("r#") {
        true => own.start() + TextSize::from(2),
        false => own.start(),
    };
    (token.kind() == SyntaxKind::IDENT && start == range.start() && own.end() == range.end()).then_some(token)
}

/// The front end's item for the node a path named.
fn def_of<'db>(sema: &Semantics<'db>, node: ast::Node<'db>) -> Option<Def> {
    let item: ModuleDef = match node.kind() {
        SyntaxKind::FN => sema.to_fn_def(ast::Fn::cast(node)?)?.into(),
        SyntaxKind::STRUCT => hir::Adt::from(sema.to_struct_def(ast::Struct::cast(node)?)?).into(),
        SyntaxKind::ENUM => hir::Adt::from(sema.to_enum_def(ast::Enum::cast(node)?)?).into(),
        SyntaxKind::UNION => hir::Adt::from(sema.to_def(ast::Union::cast(node)?)?).into(),
        SyntaxKind::TRAIT => sema.to_trait_def(ast::Trait::cast(node)?)?.into(),
        SyntaxKind::MODULE => sema.to_module_def(ast::Module::cast(node)?)?.into(),
        SyntaxKind::CONST => sema.to_const_def(ast::Const::cast(node)?)?.into(),
        SyntaxKind::STATIC => sema.to_static_def(ast::Static::cast(node)?)?.into(),
        SyntaxKind::TYPE_ALIAS => sema.to_def(ast::TypeAlias::cast(node)?)?.into(),
        SyntaxKind::MACRO_RULES => {
            sema.to_macro_def(ast::Macro::MacroRules(ast::MacroRules::cast(node)?))?.into()
        }
        SyntaxKind::MACRO_DEF => sema.to_macro_def(ast::Macro::MacroDef(ast::MacroDef::cast(node)?))?.into(),
        SyntaxKind::VARIANT => sema.to_enum_variant_def(ast::Variant::cast(node)?)?.into(),
        SyntaxKind::RECORD_FIELD => return Some(Def::Field(sema.to_def(ast::RecordField::cast(node)?)?)),
        _ => return None,
    };
    Some(Def::Item(item))
}

/// The trait `item` is an item of, when it is one.
fn trait_of(db: &Db, item: ModuleDef) -> Option<Trait> {
    item.as_assoc_item(db)?.container_trait(db)
}

/// Whether `found` is an impl's item implementing the trait item `item`.
fn implements(db: &Db, found: ModuleDef, item: ModuleDef) -> bool {
    trait_of(db, item).is_some_and(|of| found.as_assoc_item(db).and_then(|it| it.implemented_trait(db)) == Some(of))
}

/// Whether `found`, a reference resolved, is `item` or implements it.
fn is(db: &Db, found: ModuleDef, item: ModuleDef) -> bool {
    found == item || implements(db, found, item)
}

/// The files of the crates that can name `def`: its own, and those depending on it when it is
/// public.
fn files_of(db: &Db, def: Def) -> Vec<FileId> {
    let (krate, public) = match def {
        Def::Item(item) => {
            let module = match item {
                ModuleDef::Module(it) => it,
                _ => item.module(db).expect("an item a path names is in a module"),
            };
            (module.krate(db), item.visibility(db) == Visibility::Public)
        }
        Def::Field(field) => {
            (field.parent_def(db).module(db).krate(db), field.visibility(db) == Visibility::Public)
        }
    };
    let mut crates: Vec<Crate> = vec![krate];
    if public {
        crates.extend(krate.transitive_reverse_dependencies(db));
    }
    let mut seen_crates = HashSet::default();
    let mut seen = HashSet::default();
    let mut files = Vec::new();
    for krate in crates {
        if !seen_crates.insert(krate) {
            continue;
        }
        for module in krate.modules(db) {
            if let Some(file) = module.as_source_file_id(db) {
                let file = file.file_id(db);
                if seen.insert(file) {
                    files.push(file);
                }
            }
        }
    }
    files
}

/// What `token`, the old name, is to `def`, across every macro expansion it is in.
fn classify<'db>(cx: &Edits<'db, impl Files>, token: ast::Token<'db>, def: Def, node: ast::Node<'db>) -> Result<Class, String> {
    let mut found = Class::No;
    let mut other = false;
    for mapped in cx.sema.descend_into_macros_exact(token) {
        let expanded = mapped != token;
        match classify_one(cx, mapped, def, node)? {
            Class::No => other = true,
            Class::Shorthand | Class::ShorthandValue if expanded => {
                return Err("a field shorthand in a macro's input: write it out (`x: x`) to rename it".to_owned());
            }
            class => found = class,
        }
    }
    if found != Class::No && other {
        return Err("a macro uses this token both for the renamed item and for something else".to_owned());
    }
    Ok(found)
}

fn unresolved() -> String {
    "the front end can't resolve this occurrence (cfg'd out under this view's configuration, or \
     unresolved), so can't tell whether it names the renamed item"
        .to_owned()
}

fn classify_one<'db>(cx: &Edits<'db, impl Files>, token: ast::Token<'db>, def: Def, node: ast::Node<'db>) -> Result<Class, String> {
    let sema = &cx.sema;
    let db = cx.db;
    let parent = token.parent().ok_or("a token outside any node")?;
    match parent.kind() {
        SyntaxKind::NAME => {
            let owner = parent.parent().ok_or("a name outside any node")?;
            if owner == node {
                return Ok(Class::Name);
            }
            if let Some(pat) = ast::IdentPat::cast(owner) {
                if let Some(field) = owner.parent().and_then(ast::RecordPatField::cast)
                    && field.name_ref().is_none()
                {
                    let Def::Field(def) = def else { return Ok(Class::No) };
                    return match sema.resolve_record_pat_field(field) {
                        Some((it, _)) if it == def => Ok(Class::Shorthand),
                        Some(_) => Ok(Class::No),
                        None => Err(unresolved()),
                    };
                }
                let Def::Item(item) = def else { return Ok(Class::No) };
                return Ok(match sema.resolve_bind_pat_to_const(pat) {
                    Some(it) if it == item => Class::Name,
                    _ => Class::No,
                });
            }
            let Def::Item(item) = def else { return Ok(Class::No) };
            if trait_of(db, item).is_none() {
                return Ok(Class::No);
            }
            let found: Option<ModuleDef> = match owner.kind() {
                SyntaxKind::FN => ast::Fn::cast(owner).and_then(|it| sema.to_fn_def(it)).map(Into::into),
                SyntaxKind::CONST => ast::Const::cast(owner).and_then(|it| sema.to_const_def(it)).map(Into::into),
                SyntaxKind::TYPE_ALIAS => ast::TypeAlias::cast(owner).and_then(|it| sema.to_def(it)).map(Into::into),
                _ => None,
            };
            Ok(if found.is_some_and(|it| implements(db, it, item)) { Class::Name } else { Class::No })
        }
        SyntaxKind::NAME_REF => {
            let name_ref = ast::NameRef::cast(parent).expect("a name reference");
            if let Some(field) = ast::RecordExprField::for_field_name(name_ref) {
                return record_expr_field(cx, field, def);
            }
            let context = parent.parent().ok_or("a name reference outside any node")?;
            match context.kind() {
                SyntaxKind::PATH_SEGMENT => {
                    let Def::Item(item) = def else { return Ok(Class::No) };
                    let path = context.parent().and_then(ast::Path::cast).ok_or("a path segment outside a path")?;
                    let in_use = path.syntax().ancestors().any(|it| it.kind() == SyntaxKind::USE_TREE);
                    let hit = if in_use {
                        let per_ns = sema.resolve_path_per_ns(path).ok_or_else(unresolved)?;
                        [per_ns.type_ns, per_ns.value_ns, per_ns.macro_ns]
                            .into_iter()
                            .flatten()
                            .any(|it| matches!(it, PathResolution::Def(found) if is(db, found, item)))
                    } else {
                        match sema.resolve_path(path) {
                            Some(PathResolution::Def(found)) => is(db, found, item),
                            Some(_) => false,
                            None => return Err(unresolved()),
                        }
                    };
                    Ok(if hit { Class::Name } else { Class::No })
                }
                SyntaxKind::METHOD_CALL_EXPR => {
                    let Def::Item(item @ ModuleDef::Function(_)) = def else { return Ok(Class::No) };
                    let call = ast::MethodCallExpr::cast(context).expect("a method call");
                    match sema.resolve_method_call(call) {
                        Some(found) => Ok(if is(db, found.into(), item) { Class::Name } else { Class::No }),
                        None => Err(unresolved()),
                    }
                }
                SyntaxKind::FIELD_EXPR => {
                    let Def::Field(def) = def else { return Ok(Class::No) };
                    let expr = ast::FieldExpr::cast(context).expect("a field expression");
                    match sema.resolve_field(expr) {
                        Some(Either::Left(it)) => Ok(if it == def { Class::Name } else { Class::No }),
                        Some(Either::Right(_)) => Ok(Class::No),
                        None => Err(unresolved()),
                    }
                }
                SyntaxKind::RECORD_PAT_FIELD => {
                    let Def::Field(def) = def else { return Ok(Class::No) };
                    let field = ast::RecordPatField::cast(context).expect("a record pattern field");
                    match sema.resolve_record_pat_field(field) {
                        Some((it, _)) => Ok(if it == def { Class::Name } else { Class::No }),
                        None => Err(unresolved()),
                    }
                }
                SyntaxKind::ASSOC_TYPE_ARG => match def {
                    Def::Item(ModuleDef::TypeAlias(_)) => {
                        Err("an associated type's binding (`Item = …`), which the front end can't resolve to its trait's item".to_owned())
                    }
                    _ => Ok(Class::No),
                },
                SyntaxKind::OFFSET_OF_EXPR => match def {
                    Def::Field(_) => Err("a field named in `offset_of!`, which the front end can't resolve".to_owned()),
                    Def::Item(_) => Ok(Class::No),
                },
                SyntaxKind::EXTERN_CRATE => Ok(Class::No),
                kind => Err(format!("a name reference in a {kind:?}, which a rename can't place")),
            }
        }
        SyntaxKind::TOKEN_TREE => Err(
            "in tokens no macro expanded (a `macro_rules!` body, an attribute's arguments, or a \
             macro call the front end could not expand)"
                .to_owned(),
        ),
        kind => Err(format!("an identifier in a {kind:?}, which a rename can't place")),
    }
}

/// What a record expression's field name is to `def`: the field, and in a shorthand
/// (`S { x }`) the value too, a path of one segment.
fn record_expr_field<'db>(cx: &Edits<'db, impl Files>, field: ast::RecordExprField<'db>, def: Def) -> Result<Class, String> {
    let shorthand = field.name_ref().is_none();
    let resolved = cx.sema.resolve_record_field(field);
    if let Some((it, _, _)) = &resolved
        && def == Def::Field(*it)
    {
        return Ok(if shorthand { Class::Shorthand } else { Class::Name });
    }
    if !shorthand {
        return match (resolved, def) {
            (None, Def::Field(_)) => Err(unresolved()),
            _ => Ok(Class::No),
        };
    }
    if let Some((_, Some(_), _)) = resolved {
        return Ok(Class::No);
    }
    let Def::Item(item) = def else {
        return if resolved.is_some() { Ok(Class::No) } else { Err(unresolved()) };
    };
    let Some(ast::Expr::PathExpr(value)) = field.expr() else {
        return Err("a field shorthand with no value path".to_owned());
    };
    let path = value.path().ok_or("a field shorthand with no value path")?;
    match cx.sema.resolve_path(path) {
        Some(PathResolution::Def(found)) if is(cx.db, found, item) => Ok(Class::ShorthandValue),
        Some(_) => Ok(Class::No),
        None => Err(unresolved()),
    }
}

/// The item a rewritten occurrence touches, and how: the renamed item's own name renames it; one
/// in a body changes that body; in a use or a module's macro call, the scope; anywhere else, the
/// item's signature, or its fields.
pub(crate) fn touch_of(file: u32, token: ast::Token<'_>) -> Touch {
    let parent = token.parent().expect("a token has a parent");
    let Some(top) = path::top_of(parent) else {
        return Touch { file, at: At::File, how: How::Scope };
    };
    let at = At::Item(top.text_range().start());
    let range = token.text_range();
    let own_name = parent.kind() == SyntaxKind::NAME && parent.parent().is_some_and(|it| it == top);
    let how = if own_name {
        How::Renamed
    } else if body_of(top).is_some_and(|body| body.contains_range(range)) {
        How::Body
    } else if top.kind() == SyntaxKind::MODULE {
        How::Scope
    } else if parent.ancestors().take_while(|it| *it != top).any(|it| {
        matches!(
            it.kind(),
            SyntaxKind::RECORD_FIELD_LIST | SyntaxKind::TUPLE_FIELD_LIST | SyntaxKind::VARIANT_LIST
        )
    }) {
        How::Fields
    } else {
        How::Signature
    };
    Touch { file, at, how }
}

/// The range of a top item's body: a fn's block, a const's or static's value.
fn body_of(top: ast::Node<'_>) -> Option<TextRange> {
    match top.kind() {
        SyntaxKind::FN => Some(ast::Fn::cast(top)?.body()?.syntax().text_range()),
        SyntaxKind::CONST => Some(ast::Const::cast(top)?.body()?.syntax().text_range()),
        SyntaxKind::STATIC => Some(ast::Static::cast(top)?.body()?.syntax().text_range()),
        _ => None,
    }
}

/// The moves of a renamed module's own file, and of its directory when its children are in it:
/// `m.rs` to `n.rs` (and `m/` to `n/`), or `m/mod.rs`'s `m/` to `n/`. A module with a `#[path]`
/// keeps its files.
fn moves(cx: &mut Edits<'_, impl Files>, module: hir::Module, node: ast::Node<'_>, old: &str, to: &str) -> Result<(), String> {
    let decl = ast::Module::cast(node).expect("a module's node");
    if decl.item_list().is_some() {
        return Ok(());
    }
    if decl.attrs().any(|it| it.path().is_some_and(|path| path.syntax().text() == "path")) {
        return Ok(());
    }
    let file = module
        .as_source_file_id(cx.db)
        .ok_or_else(|| format!("`mod {old};` has no file the front end knows"))?
        .file_id(cx.db);
    let path = cx.files.path(file).ok_or_else(|| format!("`mod {old};`'s file is outside the view"))?;
    let split = |path: &str| match path.rsplit_once('/') {
        Some((parent, name)) => (format!("{parent}/"), name.to_owned()),
        None => (String::new(), path.to_owned()),
    };
    if let Some(dir) = path.strip_suffix("/mod.rs") {
        let (parent, name) = split(dir);
        if name != old {
            return Err(format!(
                "`mod {old};` is in {path}, which its name does not lead to: give it a `#[path]` first"
            ));
        }
        cx.moves.push(Op::Rename { to: format!("{parent}{to}"), from: dir.to_owned() });
        return Ok(());
    }
    let (parent, name) = split(path);
    if name != format!("{old}.rs") {
        return Err(format!(
            "`mod {old};` is in {path}, which its name does not lead to: give it a `#[path]` first"
        ));
    }
    let dir = format!("{parent}{old}/");
    let children_inside = module.children(cx.db).any(|child| {
        child
            .as_source_file_id(cx.db)
            .and_then(|it| cx.files.path(it.file_id(cx.db)))
            .is_some_and(|it| it.starts_with(&dir))
    });
    cx.moves.push(Op::Rename { to: format!("{parent}{to}.rs"), from: path.to_owned() });
    if children_inside {
        cx.moves.push(Op::Rename { to: format!("{parent}{to}"), from: format!("{parent}{old}") });
    }
    Ok(())
}
