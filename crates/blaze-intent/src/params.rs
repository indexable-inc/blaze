//! A function's parameters and every call of it ([`blaze_api::Edit::Params`]): a parameter
//! added with the argument each call passes for it, one removed that no body reads, or the
//! list reordered. The calls are found as a rename finds references, by the front end's name
//! resolution in the files of the crates that can name the function, a name it is imported
//! under (`use m::f as g`) searched too.
//!
//! Every use is a call it rewrites or an error naming where it is: a function used as a value
//! (`map(f)`, `let g = f;`), a call a macro made, a call with an argument count the function
//! does not take, and a function whose callers it cannot see (`extern`, `#[no_mangle]`,
//! `#[export_name]`, C-variadic). A call written in a macro's input gets its new argument
//! there; one the macro made has no input to write it in. A trait's method changes in the
//! trait and every impl of it the view has.
//!
//! An argument removed or moved must be plain (a literal, a path, or `&` of one): dropping or
//! moving any other expression would drop or reorder what it runs.

use blaze_api::{ApiError, How, Param, Target};
use hir::{AsAssocItem, ModuleDef, PathResolution};
use snafu::Snafu;
use std::{
    alloc::{Allocator, Global},
    fmt,
    ops::Range,
};
use syntax::{
    Edition, NodeOrToken, SourceFile, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use crate::{
    Edits, Files,
    edit::{At, Touch},
    path,
    rename::{self, Def},
};

/// One parameter of the new list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Entry<'a> {
    /// The function's `n`th parameter, `self` not counted.
    Old(usize),
    /// A new parameter and the argument every call passes for it.
    New { param: &'a str, default: &'a str },
}

/// What one occurrence of the function's name is.
enum Site<'db> {
    No,
    /// The function's own name, or the name of an impl's function that implements it.
    Decl(ast::Node<'db>),
    /// A call: its argument list, the file's or a macro input's token tree, and its items.
    Call { list: List, receiver: bool },
    /// A `use` that imports it under another name, which is searched too.
    Alias(&'db str),
}

/// A parenthesized, comma-separated list in a file: where its parentheses are and the range of
/// each item.
struct List {
    open_end: TextSize,
    close_start: TextSize,
    items: Vec<TextRange, Global>,
    /// Whether each item is plain, so it can be dropped or moved.
    plain: Vec<bool, Global>,
    /// Whether the list is a macro input's token tree, where only additions are written.
    in_macro_input: bool,
}

/// One splice, in the base text of the request's `file`th file.
struct Hit {
    file: u32,
    range: TextRange,
    insert: String,
}

/// Why a change of parameters is refused, before [`params`] adds the function and the place.
/// Each variant is one function's refusal; the text is what the request's error reads.
#[derive(Debug, Snafu)]
enum Refused<'r> {
    /// [`unseen_callers`]: an fn in an `extern` block.
    #[snafu(display("is declared in an `extern` block: its callers are outside the view"))]
    ExternBlock,
    /// [`unseen_callers`]: an fn with an ABI.
    #[snafu(display("has an `extern` ABI: callers outside Rust cannot be seen"))]
    ExternAbi,
    /// [`unseen_callers`]: an fn an attribute exports.
    #[snafu(display("is exported by `{attr}`: callers outside Rust cannot be seen"))]
    Exported { attr: &'r str },
    /// [`unseen_callers`]: an fn with `...`.
    #[snafu(display("is C-variadic"))]
    CVariadic,
    /// [`entries`]: a kept name no parameter has.
    #[snafu(display("has no parameter `{name}` (it has `{}`)", Names(old)))]
    NoParam { name: &'r str, old: &'r [ast::Node<'r>] },
    /// [`entries`]: a kept name two parameters have.
    #[snafu(display("has two parameters `{name}`; name them apart first"))]
    TwoParams { name: &'r str },
    /// [`entries`]: a parameter kept twice.
    #[snafu(display("parameter `{name}` is kept twice"))]
    KeptTwice { name: &'r str },
    /// [`one_param`]: a new parameter that does not parse as one.
    #[snafu(display("`{text}` is no one parameter (`name: Type`)"))]
    NotOneParam { text: &'r str },
    /// [`one_expr`]: a new argument that does not parse as one expression.
    #[snafu(display("`{text}` is no one expression to pass as an argument"))]
    NotOneExpr { text: &'r str },
    /// [`site_of`]: the fn's name in a macro's input.
    #[snafu(display("the fn is declared in a macro's input"))]
    DeclaredInMacro,
    /// [`site_of`]: a token a macro reads two ways.
    #[snafu(display("a macro uses this token both for the fn and for something else"))]
    TwoWays,
    /// [`site_one`], [`arg_list`], [`decl_list`], [`token_tree_list`]: a tree not shaped as
    /// its kind is, named by `what`.
    #[snafu(display("{what}"))]
    Shape { what: &'static str },
    /// [`site_one`], [`use_site`]: an occurrence the front end can't resolve.
    #[snafu(display("{}", rename::UNRESOLVED))]
    Unresolved,
    /// [`site_one`]: the fn used as a value.
    #[snafu(display(
        "uses `{name}` as a value (`map(f)`, `let g = f;`, a call through parentheses), which \
         no added argument reaches"
    ))]
    AsValue { name: &'r str },
    /// [`site_one`]: a field shorthand that may be the fn.
    #[snafu(display(
        "a field shorthand whose value may be `{name}` as a value, which no added argument reaches"
    ))]
    Shorthand { name: &'r str },
    /// [`site_one`]: a name reference in a node no change of parameters places.
    #[snafu(display("a name reference in a {kind:?}, which a change of parameters can't place"))]
    NameRefIn { kind: SyntaxKind },
    /// [`site_one`]: tokens no macro expanded.
    #[snafu(display(
        "in tokens no macro expanded (a `macro_rules!` body, an attribute's arguments, or a \
         macro call the front end could not expand): a call a macro makes has no input to write \
         an argument in"
    ))]
    Unexpanded,
    /// [`site_one`]: an identifier in a node no change of parameters places.
    #[snafu(display("an identifier in a {kind:?}, which a change of parameters can't place"))]
    IdentIn { kind: SyntaxKind },
    /// [`call_site`]: a call whose argument list a macro made.
    #[snafu(display("a call a macro made, whose argument list is no text of the macro's input"))]
    MacroMade,
    /// [`call_site`]: a macro input the expansion reads as another count of arguments.
    #[snafu(display(
        "a call in a macro's input whose {written} arguments the expansion reads as {read}"
    ))]
    MacroCount { written: usize, read: usize },
    /// [`token_tree_list`]: a macro input's call not in parentheses.
    #[snafu(display("a call in a macro's input whose arguments are not in `( )`"))]
    NotParenthesized,
    /// [`rewrite`]: a removal or a move in a macro's input.
    #[snafu(display("a call in a macro's input, where only an added argument is written"))]
    OnlyAdditions,
    /// [`rewrite`]: an argument with effects that would be dropped or moved.
    #[snafu(display(
        "argument `{arg}` would be {}, and it is no literal or path whose order nothing sees",
        if *dropped { "dropped" } else { "moved" }
    ))]
    NotPlain { arg: &'r str, dropped: bool },
    /// [`body_reads_none`]: a pattern parameter removed.
    #[snafu(display("removes the pattern parameter `{pat}`; bind it to a name first"))]
    PatternParam { pat: &'r str },
    /// [`body_reads_none`]: a parameter removed that the body reads.
    #[snafu(display("removes parameter `{name}`, which the body still reads"))]
    StillRead { name: &'r str },
}

/// Parameters' names, as `a`, `b` (the outer backticks the message's).
struct Names<'r>(&'r [ast::Node<'r>]);

impl fmt::Display for Names<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, param) in self.0.iter().enumerate() {
            if at > 0 {
                f.write_str("`, `")?;
            }
            f.write_str(pattern_text(*param))?;
        }
        Ok(())
    }
}

/// Changes the parameters of the function `target` names to `wanted`, and every call.
pub(crate) fn params(
    cx: &mut Edits<'_, impl Files>,
    target: &Target,
    wanted: &[Param],
) -> Result<(), ApiError> {
    let fail = |what: String| ApiError::new(what, Some(target.file.clone()));
    let home = cx.open(&target.file).map_err(fail)?;
    let node = path::find(cx.roots[home as usize], &target.item)
        .map_err(|error| fail(format!("{}: {error}", target.file)))?;
    let decl = ast::Fn::cast(node).ok_or_else(|| fail(format!("`{}` is no fn", target.item)))?;
    let function = cx.sema.to_fn_def(decl).ok_or_else(|| {
        fail(format!(
            "the front end has no item for `{}` in {}: cfg'd out under this view's configuration",
            target.item, target.file
        ))
    })?;
    let db = cx.db;
    if let Some(of) = function.as_assoc_item(db).and_then(|it| it.implemented_trait(db)) {
        return Err(fail(format!(
            "`{}` implements an item of trait `{}`: change its parameters in the trait, and its \
             impls follow",
            target.item,
            of.name(db).as_str()
        )));
    }
    unseen_callers(node).map_err(|why| fail(format!("`{}` {why}", target.item)))?;
    let mut old = Vec::new_in(Global);
    old.extend(param_nodes(node));
    let entries =
        entries(&old, wanted, Global).map_err(|why| fail(format!("`{}`: {why}", target.item)))?;
    for entry in &entries {
        if let Entry::New { param, default } = entry {
            let added = |why: Refused<'_>| fail(format!("a new parameter of `{}`: {why}", target.item));
            one_param(param).map_err(added)?;
            one_expr(default).map_err(added)?;
        }
    }
    let item = ModuleDef::Function(function);
    let bare = path::name_of(node).ok_or_else(|| fail(format!("`{}` has no name", target.item)))?;
    let bare = bare.strip_prefix("r#").unwrap_or(&bare).to_owned();

    let mut hits = Vec::new_in(Global);
    let mut names = vec![bare];
    let mut searched = 0;
    let files = cx.files;
    while let Some(name) = names.get(searched).cloned() {
        searched += 1;
        let finder = memchr::memmem::Finder::new(name.as_bytes());
        for file in rename::files_of(db, Def::Item(item), Global) {
            let path = files.path(file).ok_or_else(|| {
                fail(format!("a file of a crate that can see `{}` is outside the view", target.item))
            })?;
            let (_, bytes) =
                files.file(path).ok_or_else(|| fail(format!("{path} is not in the view")))?;
            if finder.find(bytes).is_none() {
                continue;
            }
            let index = cx.open_id(file).map_err(fail)?;
            let root = cx.roots[index as usize];
            for at in finder.find_iter(root.file().text().as_bytes()) {
                let Some(token) = rename::ident_at(root, at, name.len()) else { continue };
                let place = || {
                    let text = root.file().text();
                    let at = usize::from(token.text_range().start());
                    let line = memchr::memchr_iter(b'\n', &text.as_bytes()[..at]).count() + 1;
                    format!("{path}:{line}")
                };
                let refuse = |what: &dyn fmt::Display| {
                    let place = place();
                    ApiError::new(format!("{place}: {what}"), Some(place))
                };
                let site = site_of(cx, index, token, item, node).map_err(|why| refuse(&why))?;
                let (list, receiver, decl) = match site {
                    Site::No => continue,
                    Site::Alias(alias) => {
                        if !names.iter().any(|it| it == alias) {
                            names.push(alias.to_owned());
                        }
                        continue;
                    }
                    Site::Decl(decl) => {
                        (decl_list(decl).map_err(|why| refuse(&why))?, false, Some(decl))
                    }
                    Site::Call { list, receiver } => (list, receiver, None),
                };
                let offset = usize::from(receiver || (decl.is_some() && has_self(decl)));
                if list.items.len() != old.len() + offset {
                    return Err(refuse(&match decl {
                        Some(_) => format!(
                            "this impl's fn takes {} parameters where the trait's takes {}",
                            list.items.len() - offset,
                            old.len()
                        ),
                        None => format!(
                            "a call with {} arguments, where `{}` takes {}",
                            list.items.len(),
                            target.item,
                            old.len() + offset
                        ),
                    }));
                }
                if let Some(decl) = decl {
                    body_reads_none(cx, decl, &entries)
                        .map_err(|why| refuse(&format_args!("`{}` {why}", target.item)))?;
                    let top = path::top_of(decl).unwrap_or(decl);
                    cx.touches.push(Touch {
                        file: index,
                        at: At::Item(top.text_range().start()),
                        how: How::Signature,
                    });
                } else {
                    cx.touches.push(rename::touch_of(index, token));
                }
                let text = root.file().text();
                let spot = Spot { file: index, offset, decl: decl.is_some() };
                rewrite(&list, spot, &entries, text, &mut hits).map_err(|why| refuse(&why))?;
            }
        }
    }
    hits.sort_by(|a, b| (b.file, b.range.start(), b.range.end()).cmp(&(a.file, a.range.start(), a.range.end())));
    hits.dedup_by(|a, b| a.file == b.file && a.range == b.range && a.insert == b.insert);
    for pair in hits.windows(2) {
        let [a, b] = pair else { continue };
        if a.file == b.file && a.range.start() == b.range.start() {
            let path = &cx.working[a.file as usize].path;
            return Err(fail(format!(
                "{path}: two changes of `{}`'s parameters reach one place",
                target.item
            )));
        }
    }
    for hit in hits {
        let file = &mut cx.working[hit.file as usize];
        let now = file.to_now(hit.range).ok_or_else(|| {
            fail(format!(
                "{}: the change of `{}`'s parameters reaches text an earlier edit of this request \
                 replaced",
                file.path, target.item
            ))
        })?;
        file.splice(now, hit.insert).map_err(fail)?;
    }
    Ok(())
}

/// Why no rewrite can reach every caller of `node`, an fn: one outside the view's Rust, or one
/// a parameter list it cannot extend.
fn unseen_callers(node: ast::Node<'_>) -> Result<(), Refused<'_>> {
    if node.parent().is_some_and(|it| it.kind() == SyntaxKind::EXTERN_ITEM_LIST) {
        return Err(Refused::ExternBlock);
    }
    if node.child_where(|it| it == SyntaxKind::ABI).is_some() {
        return Err(Refused::ExternAbi);
    }
    for attr in node.children().filter(|it| it.kind() == SyntaxKind::ATTR) {
        let text = attr.text();
        if text.contains("no_mangle") || text.contains("export_name") {
            return Err(Refused::Exported { attr: text });
        }
    }
    if param_nodes(node).any(|it| it.child_token(SyntaxKind::DOT3).is_some()) {
        return Err(Refused::CVariadic);
    }
    Ok(())
}

/// An fn's parameters, `self` not among them.
fn param_nodes(node: ast::Node<'_>) -> impl Iterator<Item = ast::Node<'_>> {
    node.child_where(|it| it == SyntaxKind::PARAM_LIST)
        .into_iter()
        .flat_map(|list| list.children().filter(|it| it.kind() == SyntaxKind::PARAM))
}

fn has_self(decl: Option<ast::Node<'_>>) -> bool {
    decl.and_then(|it| it.child_where(|kind| kind == SyntaxKind::PARAM_LIST))
        .is_some_and(|it| it.child_where(|kind| kind == SyntaxKind::SELF_PARAM).is_some())
}

/// A parameter's name: its pattern as written.
fn pattern_text<'t>(param: ast::Node<'t>) -> &'t str {
    param.first_child().map_or("", |it| it.text())
}

/// `wanted` as entries over `old`, each old parameter named at most once.
fn entries<'r, A: Allocator>(
    old: &'r [ast::Node<'r>],
    wanted: &'r [Param],
    alloc: A,
) -> Result<Vec<Entry<'r>, A>, Refused<'r>> {
    let mut out = Vec::with_capacity_in(wanted.len(), alloc);
    for param in wanted {
        match param {
            Param::Added { param, default } => out.push(Entry::New { param, default }),
            Param::Kept { name } => {
                let name = name.as_str();
                let mut at = old.iter().enumerate().filter(|(_, it)| pattern_text(**it) == name);
                let (Some((index, _)), None) = (at.next(), at.next()) else {
                    return Err(match old.iter().filter(|it| pattern_text(**it) == name).count() {
                        0 => Refused::NoParam { name, old },
                        _ => Refused::TwoParams { name },
                    });
                };
                if out.contains(&Entry::Old(index)) {
                    return Err(Refused::KeptTwice { name });
                }
                out.push(Entry::Old(index));
            }
        }
    }
    Ok(out)
}

/// `text` is one parameter, `name: Type`.
fn one_param(text: &str) -> Result<(), Refused<'_>> {
    let source = format!("fn __astlog({text}) {{}}");
    let parse = SourceFile::parse(&source, Edition::CURRENT);
    let root = parse.root();
    let list = root.descendants().find(|it| it.kind() == SyntaxKind::PARAM_LIST);
    let one = list.is_some_and(|it| {
        it.children().filter(|it| it.kind() == SyntaxKind::PARAM).count() == 1
            && it.child_where(|kind| kind == SyntaxKind::SELF_PARAM).is_none()
    });
    if !parse.errors_with_bodies().is_empty() || !one {
        return Err(Refused::NotOneParam { text });
    }
    Ok(())
}

/// `text` is one expression, which an argument list never needs parenthesized.
fn one_expr(text: &str) -> Result<(), Refused<'_>> {
    let source = format!("fn __astlog() {{ __astlog({text}); }}");
    let parse = SourceFile::parse(&source, Edition::CURRENT);
    let root = parse.root();
    let list = root.descendants().find(|it| it.kind() == SyntaxKind::ARG_LIST);
    let one = list.is_some_and(|it| it.children().count() == 1);
    let comment = root.descendants_with_tokens().any(|it| it.kind() == SyntaxKind::COMMENT);
    if !parse.errors_with_bodies().is_empty() || !one || comment {
        return Err(Refused::NotOneExpr { text });
    }
    Ok(())
}

/// What the occurrence `token` of the name, in the request's `index`th file, is to `item`
/// (declared by `node`), across every macro expansion it is in.
fn site_of<'db>(
    cx: &Edits<'db, impl Files>,
    index: u32,
    token: ast::Token<'db>,
    item: ModuleDef,
    node: ast::Node<'db>,
) -> Result<Site<'db>, Refused<'db>> {
    let mut found: Option<Site<'db>> = None;
    let mut other = false;
    for mapped in cx.sema.descend_into_macros_exact(token) {
        let expanded = mapped != token;
        match site_one(cx, index, mapped, item, node, expanded)? {
            Site::No => other = true,
            Site::Decl(_) if expanded => return Err(Refused::DeclaredInMacro),
            site => found = Some(site),
        }
    }
    match found {
        Some(_) if other => Err(Refused::TwoWays),
        Some(site) => Ok(site),
        None => Ok(Site::No),
    }
}

/// A tree not shaped as its kind is, named by `what`.
const fn shape(what: &'static str) -> Refused<'static> {
    Refused::Shape { what }
}

fn site_one<'db>(
    cx: &Edits<'db, impl Files>,
    index: u32,
    token: ast::Token<'db>,
    item: ModuleDef,
    node: ast::Node<'db>,
    expanded: bool,
) -> Result<Site<'db>, Refused<'db>> {
    let sema = &cx.sema;
    let db = cx.db;
    let parent = token.parent().ok_or(shape("a token outside any node"))?;
    match parent.kind() {
        SyntaxKind::NAME => {
            let owner = parent.parent().ok_or(shape("a name outside any node"))?;
            if owner == node {
                return Ok(Site::Decl(owner));
            }
            let Some(decl) = ast::Fn::cast(owner) else { return Ok(Site::No) };
            let implements = sema.to_fn_def(decl).is_some_and(|it| {
                let it = ModuleDef::Function(it);
                it != item && rename::is(db, it, item)
            });
            Ok(if implements { Site::Decl(owner) } else { Site::No })
        }
        SyntaxKind::NAME_REF => {
            let context = parent.parent().ok_or(shape("a name reference outside any node"))?;
            match context.kind() {
                SyntaxKind::PATH_SEGMENT => {
                    let path = context.parent().and_then(ast::Path::cast).ok_or(shape("a path segment outside a path"))?;
                    if let Some(tree) = path.syntax().ancestors().find(|it| it.kind() == SyntaxKind::USE_TREE) {
                        return use_site(cx, tree, path, item);
                    }
                    match sema.resolve_path(path) {
                        Some(PathResolution::Def(found)) if rename::is(db, found, item) => {}
                        Some(_) => return Ok(Site::No),
                        None => return Err(Refused::Unresolved),
                    }
                    let callee = path.syntax().parent().filter(|it| it.kind() == SyntaxKind::PATH_EXPR);
                    let call = callee
                        .and_then(|it| it.parent().filter(|call| call.kind() == SyntaxKind::CALL_EXPR && call.first_child() == Some(it)));
                    let Some(call) = call else {
                        return Err(Refused::AsValue { name: token.text() });
                    };
                    let receiver = has_self(sema.source(match item {
                        ModuleDef::Function(it) => it,
                        _ => return Ok(Site::No),
                    }).map(|it| it.value.syntax()));
                    call_site(cx, index, call, receiver, expanded)
                }
                SyntaxKind::METHOD_CALL_EXPR => {
                    let call = ast::MethodCallExpr::cast(context).ok_or(shape("a method call"))?;
                    match sema.resolve_method_call(call) {
                        Some(found) if rename::is(db, found.into(), item) => {
                            call_site(cx, index, context, false, expanded)
                        }
                        Some(_) => Ok(Site::No),
                        None => Err(Refused::Unresolved),
                    }
                }
                SyntaxKind::FIELD_EXPR | SyntaxKind::RECORD_PAT_FIELD | SyntaxKind::EXTERN_CRATE => Ok(Site::No),
                SyntaxKind::RECORD_EXPR_FIELD => {
                    // `S { f: v }` names a field; `S { f }` is a value too.
                    let field = ast::RecordExprField::cast(context).ok_or(shape("a record field"))?;
                    match field.expr() {
                        Some(_) if context.child_token(SyntaxKind::COLON).is_some() => Ok(Site::No),
                        _ => Err(Refused::Shorthand { name: token.text() }),
                    }
                }
                kind => Err(Refused::NameRefIn { kind }),
            }
        }
        SyntaxKind::TOKEN_TREE => Err(Refused::Unexpanded),
        kind => Err(Refused::IdentIn { kind }),
    }
}

/// A `use` of the name: nothing to rewrite, but a rename (`as g`) of the fn is searched too.
fn use_site<'db>(
    cx: &Edits<'db, impl Files>,
    tree: ast::Node<'db>,
    path: ast::Path<'db>,
    item: ModuleDef,
) -> Result<Site<'db>, Refused<'db>> {
    let per_ns = cx.sema.resolve_path_per_ns(path).ok_or(Refused::Unresolved)?;
    let hit = [per_ns.type_ns, per_ns.value_ns, per_ns.macro_ns]
        .into_iter()
        .flatten()
        .any(|it| matches!(it, PathResolution::Def(found) if rename::is(cx.db, found, item)));
    // The path's last segment names the fn only in the tree that imports it.
    let own = tree.child_where(|it| it == SyntaxKind::PATH) == Some(path.syntax());
    if !hit || !own {
        return Ok(Site::No);
    }
    let alias = tree
        .child_where(|it| it == SyntaxKind::RENAME)
        .and_then(|it| it.child_where(|kind| kind == SyntaxKind::NAME))
        .map(|it| it.text().strip_prefix("r#").unwrap_or(it.text()));
    Ok(alias.map_or(Site::No, Site::Alias))
}

/// The call `call` (a call or a method call) as a list in the file: the file's own argument
/// list, or, for a call in a macro's expansion, the token tree of the input it came from.
fn call_site<'db>(
    cx: &Edits<'db, impl Files>,
    index: u32,
    call: ast::Node<'db>,
    receiver: bool,
    expanded: bool,
) -> Result<Site<'db>, Refused<'db>> {
    let args = call.child_where(|it| it == SyntaxKind::ARG_LIST).ok_or(shape("a call with no argument list"))?;
    if !expanded {
        return Ok(Site::Call { list: arg_list(args)?, receiver });
    }
    let range = cx.sema.original_range_opt(args).ok_or(Refused::MacroMade)?;
    if range.file_id.file_id(cx.db) != cx.ids[index as usize] {
        return Err(Refused::MacroMade);
    }
    let root = cx.roots[index as usize];
    let NodeOrToken::Node(tree) = root.covering_element(range.range) else {
        return Err(Refused::MacroMade);
    };
    if tree.kind() != SyntaxKind::TOKEN_TREE || tree.text_range() != range.range {
        return Err(Refused::MacroMade);
    }
    let mut list = token_tree_list(tree)?;
    let read = args.children().count();
    if list.items.len() != read {
        return Err(Refused::MacroCount { written: list.items.len(), read });
    }
    list.in_macro_input = true;
    Ok(Site::Call { list, receiver })
}

/// A call's argument list.
fn arg_list(args: ast::Node<'_>) -> Result<List, Refused<'static>> {
    let open = args.child_token(SyntaxKind::L_PAREN).ok_or(shape("an argument list with no `(`"))?;
    let close = args.child_token(SyntaxKind::R_PAREN).ok_or(shape("an argument list with no `)`"))?;
    let mut items = Vec::new_in(Global);
    let mut plains = Vec::new_in(Global);
    for item in args.children() {
        items.push(item.text_range());
        plains.push(plain(item));
    }
    Ok(List {
        open_end: open.text_range().end(),
        close_start: close.text_range().start(),
        plain: plains,
        items,
        in_macro_input: false,
    })
}

/// A declaration's parameter list, `self` its first item when it has one.
fn decl_list(decl: ast::Node<'_>) -> Result<List, Refused<'static>> {
    let params = decl.child_where(|it| it == SyntaxKind::PARAM_LIST).ok_or(shape("an fn with no parameter list"))?;
    let open = params.child_token(SyntaxKind::L_PAREN).ok_or(shape("a parameter list with no `(`"))?;
    let close = params.child_token(SyntaxKind::R_PAREN).ok_or(shape("a parameter list with no `)`"))?;
    let mut items = Vec::new_in(Global);
    items.extend(
        params
            .children()
            .filter(|it| matches!(it.kind(), SyntaxKind::PARAM | SyntaxKind::SELF_PARAM))
            .map(|it| it.text_range()),
    );
    let mut plains = Vec::new_in(Global);
    plains.resize(items.len(), true);
    Ok(List {
        open_end: open.text_range().end(),
        close_start: close.text_range().start(),
        plain: plains,
        items,
        in_macro_input: false,
    })
}

/// A macro input's `( ... )` token tree as a list split at its top-level commas.
fn token_tree_list(tree: ast::Node<'_>) -> Result<List, Refused<'static>> {
    let (Some(first), Some(last)) = (tree.children_with_tokens().next(), tree.children_with_tokens().last()) else {
        return Err(shape("an empty token tree"));
    };
    if first.kind() != SyntaxKind::L_PAREN || last.kind() != SyntaxKind::R_PAREN {
        return Err(Refused::NotParenthesized);
    }
    let mut items = Vec::new_in(Global);
    let mut current: Option<TextRange> = None;
    let inner = tree.children_with_tokens().skip(1).filter(|it| it.text_range() != last.text_range());
    for element in inner {
        match element.kind() {
            SyntaxKind::WHITESPACE | SyntaxKind::COMMENT => {}
            SyntaxKind::COMMA => items.extend(current.take()),
            _ => {
                let range = element.text_range();
                current = Some(current.map_or(range, |it| it.cover(range)));
            }
        }
    }
    items.extend(current);
    let mut plains = Vec::new_in(Global);
    plains.resize(items.len(), false);
    Ok(List {
        open_end: first.text_range().end(),
        close_start: last.text_range().start(),
        plain: plains,
        items,
        in_macro_input: true,
    })
}

/// Whether an argument can be dropped or moved without dropping or moving what it runs.
fn plain(expr: ast::Node<'_>) -> bool {
    match expr.kind() {
        SyntaxKind::LITERAL | SyntaxKind::PATH_EXPR => true,
        SyntaxKind::REF_EXPR => expr.first_child().is_some_and(plain),
        _ => false,
    }
}

/// Where a list's splices go: the request's `file`th file, past the list's first `offset`
/// items, and whether the list declares (each new item its parameter) or calls (its argument).
#[derive(Clone, Copy)]
struct Spot {
    file: u32,
    offset: usize,
    decl: bool,
}

/// Pushes onto `out` the splices that make `list` (whose first `spot.offset` items stay first)
/// the entries'. Only additions keep every old item's text where it is: each new item is
/// inserted. A removal or a move rewrites the list between its parentheses.
fn rewrite<'t, A: Allocator>(
    list: &List,
    spot: Spot,
    entries: &[Entry<'_>],
    text: &'t str,
    out: &mut Vec<Hit, A>,
) -> Result<(), Refused<'t>> {
    let Spot { file, offset, decl } = spot;
    let new_text = |entry: &Entry<'_>| match entry {
        Entry::New { param, default } => (if decl { *param } else { *default }).to_owned(),
        Entry::Old(at) => text[range(list.items[offset + at])].to_owned(),
    };
    let olds = || entries.iter().filter_map(|it| match it { Entry::Old(at) => Some(*at), Entry::New { .. } => None });
    let in_order = olds().zip(olds().skip(1)).all(|(a, b)| a < b);
    let all = olds().count() == list.items.len() - offset;
    if in_order && all {
        let mut before = 0;
        let mut pending: Vec<String> = Vec::new();
        let mut flush = |at: usize, pending: &mut Vec<String>| {
            if pending.is_empty() {
                return;
            }
            let joined = pending.join(", ");
            pending.clear();
            let k = offset + at;
            let (range, insert) = if k < list.items.len() {
                (TextRange::empty(list.items[k].start()), format!("{joined}, "))
            } else if let Some(last) = list.items.last() {
                (TextRange::empty(last.end()), format!(", {joined}"))
            } else {
                (TextRange::empty(list.close_start), joined)
            };
            out.push(Hit { file, range, insert });
        };
        for entry in entries {
            match entry {
                Entry::Old(at) => {
                    flush(before, &mut pending);
                    before = at + 1;
                }
                Entry::New { .. } => pending.push(new_text(entry)),
            }
        }
        flush(before, &mut pending);
        return Ok(());
    }
    if list.in_macro_input {
        return Err(Refused::OnlyAdditions);
    }
    let moved = !in_order;
    for (at, is_plain) in list.plain.iter().enumerate().skip(offset) {
        let dropped = !olds().any(|it| it == at - offset);
        if (dropped || moved) && !is_plain {
            return Err(Refused::NotPlain { arg: &text[range(list.items[at])], dropped });
        }
    }
    let mut parts: Vec<String> = list.items[..offset].iter().map(|it| text[range(*it)].to_owned()).collect();
    parts.extend(entries.iter().map(new_text));
    let range = TextRange::new(list.open_end, list.close_start);
    out.push(Hit { file, range, insert: parts.join(", ") });
    Ok(())
}

fn range(range: TextRange) -> Range<usize> {
    usize::from(range.start())..usize::from(range.end())
}

/// No parameter the entries drop is read by `decl`'s body.
fn body_reads_none<'db>(
    cx: &Edits<'db, impl Files>,
    decl: ast::Node<'db>,
    entries: &[Entry<'_>],
) -> Result<(), Refused<'db>> {
    let body = decl.child_where(|it| it == SyntaxKind::BLOCK_EXPR);
    for (at, param) in param_nodes(decl).enumerate() {
        if entries.contains(&Entry::Old(at)) {
            continue;
        }
        let Some(pat) = param.first_child() else { continue };
        match pat.kind() {
            SyntaxKind::WILDCARD_PAT => continue,
            SyntaxKind::IDENT_PAT => {}
            _ => return Err(Refused::PatternParam { pat: pat.text() }),
        }
        let Some(body) = body else { continue };
        let local = ast::IdentPat::cast(pat).and_then(|it| cx.sema.to_def(it));
        let name = pat.child_where(|it| it == SyntaxKind::NAME).map_or("", |it| it.text());
        for token in body.descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.kind() != SyntaxKind::IDENT || token.text() != name {
                continue;
            }
            // A token of a macro's input reads the parameter when an expansion of it does.
            if cx.sema.descend_into_macros_exact(token).into_iter().any(|it| reads(cx, it, local)) {
                return Err(Refused::StillRead { name });
            }
        }
    }
    Ok(())
}

/// Whether `token`, spelled as a parameter, may read it: a path that resolves to the parameter
/// (to any local when the front end has none for it), and, unable to tell, a name in tokens no
/// macro expanded or a field shorthand. A field's or a method's name, or a new binding, is not.
fn reads<'db>(
    cx: &Edits<'db, impl Files>,
    token: ast::Token<'db>,
    local: Option<hir::Local<'db>>,
) -> bool {
    let Some(parent) = token.parent() else { return true };
    match parent.kind() {
        SyntaxKind::TOKEN_TREE => true,
        SyntaxKind::NAME_REF => match parent.parent().map(|it| it.kind()) {
            Some(SyntaxKind::PATH_SEGMENT) => {
                let found = parent
                    .parent()
                    .and_then(|it| it.parent())
                    .and_then(ast::Path::cast)
                    .and_then(|it| cx.sema.resolve_path(it));
                match (found, local) {
                    (Some(PathResolution::Local(found)), Some(local)) => found == local,
                    (Some(PathResolution::Local(_)), None) | (None, _) => true,
                    (Some(_), _) => false,
                }
            }
            Some(SyntaxKind::FIELD_EXPR | SyntaxKind::METHOD_CALL_EXPR) => false,
            _ => true,
        },
        _ => false,
    }
}
