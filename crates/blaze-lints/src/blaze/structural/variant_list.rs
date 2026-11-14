//! `hand_kept_variant_list`, `variant_list_drift` and `registry_spread`: an enum's variants
//! listed again by hand, the bugs those lists drift into, and an enum whose every variant's
//! facts are spread over many parallel lists.
//!
//! **The lists** of an enum `E` in a file:
//!
//! - in an impl of `E`: a `const` array of its variants (`ALL`), a `const` count (`COUNT`,
//!   `LEN`, `N`, `NUM`, `VARIANTS`, `NUM_VARIANTS`), a function that matches on `self` with one
//!   arm per variant giving a string (`fn name`), an integer (`fn index`) or anything else (a
//!   fact: `fn kind`, `fn help`), a function that matches strings to variants (`from_str`,
//!   `from_name`), and a `const` array of strings indexed by `E` (`[&str; Self::COUNT]`);
//! - anywhere in the file, `E` of any crate: a `const` or `static` array of strings whose
//!   length is `E::COUNT` (or `LEN`, `N`, `ALL.len()`), written out or made by a
//!   `macro_rules!` of the file (its strings are the last of each element).
//!
//! **`variant_list_drift`** (the bug form): a list of the variants that misses one or repeats
//! one, a count that is not the number of variants, a name that is not the variant's under the
//! rule the other names follow (snake case, lower case, kebab case, the variant's own name, a
//! shared prefix such as `"#"` stripped first) and is within two edits of it or another
//! variant's name, a repeated name, a table indexed by `E` that is not in declaration order,
//! a parse that does not map a name back to the variant `fn name` gives it, and a match on
//! `self` whose `_` arm stands for exactly one variant (22 of 23 named: the 24th added later
//! takes `_` silently). Fix
//! (maybe-incorrect) where the list is one array or one literal: the list regenerated from the
//! declaration.
//!
//! **`hand_kept_variant_list`** (advice): the lists that are right today but derivable from the
//! declaration (`ALL`, `COUNT`, names by one rule, `index` in declaration order), reported once
//! per enum and naming them: each is an edit with every new variant. No declarative table or
//! derive for them exists in the tree yet; the end state is `variants_of::<E>()` with
//! `Variant::NAME` and `Variant::INDEX` (docs/v2/plan/design/comptime.md).
//!
//! **`registry_spread`** (advice): an enum with five lists or more in the file, two or more of
//! them facts no rule derives: each variant's facts live at its position in every list, so a
//! new variant is an edit in each. The form that replaces them is one record per variant (one
//! `const` table of rows, or one declarative macro invocation), from which every list is
//! derived.

use std::alloc::Global;

use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, HasName as _},
};

use super::{
    Items,
    uniform::{last, single},
};
use crate::{
    blaze::{
        Lint,
        std_mistakes::{Fix, report},
    },
    check::Cx,
    diag::Applicability,
};

/// The names a count of the variants goes by.
const COUNTS: &[&str] = &["COUNT", "LEN", "N", "NUM", "VARIANTS", "NUM_VARIANTS", "SIZE"];

/// The fewest lists, and of them the fewest facts, for `registry_spread`.
const SPREAD: (usize, usize) = (5, 2);

/// One list of an enum's variants.
struct List<'t> {
    /// What it is in a message: `ALL`, `fn name`, `LIVE`.
    what: String,
    /// The item it is, for its lint levels.
    node: ast::Node<'t>,
    /// Where it is reported.
    range: TextRange,
    kind: Kind<'t>,
}

enum Kind<'t> {
    /// The variants, as written, in an array: `[E::A, E::B]`.
    All { names: Vec<&'t str, Global>, array: ast::ArrayExpr<'t>, prefix: &'t str },
    /// A count of the variants, its literal.
    Count { value: u128, literal: TextRange },
    /// A string per variant from a match on `self`.
    Names { entries: Vec<(&'t str, &'t str), Global> },
    /// A variant per string from a match on a string.
    Parse { entries: Vec<(&'t str, &'t str), Global> },
    /// An integer per variant from a match on `self`.
    Index { entries: Vec<(&'t str, u128), Global> },
    /// A string per variant, in the variants' order, indexed by the enum.
    Table { strings: Vec<&'t str, Global>, array: Option<ast::ArrayExpr<'t>> },
    /// A match on `self` that names some variants and leaves the rest to a `_` arm at `pat`;
    /// `prefix` is the qualifier the named arms write.
    Wild { named: Vec<&'t str, Global>, pat: TextRange, prefix: &'t str },
    /// Any other fact per variant, from a match on `self`.
    Facts,
}

/// An enum and the lists of it found in the file.
struct Lists<'t> {
    adt: hir::Adt,
    name: String,
    variants: Vec<String, Global>,
    /// The enum's own item, when it is of this file.
    node: Option<ast::Enum<'t>>,
    lists: Vec<List<'t>, Global>,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, items: &Items<'t>) {
    let on = [Lint::HandKeptVariantList, Lint::VariantListDrift, Lint::RegistrySpread];
    if !on.iter().any(|it| cx.on_blaze(*it)) {
        return;
    }
    let mut all: Vec<Lists<'t>, Global> = Vec::new_in(Global);
    let db = cx.db();
    for &node in &items.enums {
        let Some(adt) = cx.adt_def(ast::Adt::Enum(node)) else { continue };
        let Some(name) = node.name() else { continue };
        let mut variants: Vec<String, Global> = Vec::new_in(Global);
        if let Some(list) = node.variant_list() {
            variants.extend(list.variants().filter_map(|it| it.name()).map(|it| it.text().to_owned()));
        }
        let mut lists = Lists {
            adt,
            name: name.text().to_owned(),
            variants,
            node: Some(node),
            lists: Vec::new_in(Global),
        };
        for &imp in &items.impls {
            let mine = cx
                .impl_def(imp)
                .and_then(|it| it.self_ty(db).as_adt())
                .is_some_and(|it| it == adt);
            if mine {
                in_impl(&lists.name.clone(), imp, &mut lists.lists);
            }
        }
        all.push(lists);
    }
    for &node in &items.consts {
        let Some((adt, list)) = table(cx, items, node) else { continue };
        match all.iter_mut().find(|it| it.adt == adt) {
            Some(it) => {
                if !it.lists.iter().any(|it| it.node == list.node) {
                    it.lists.push(list);
                }
            }
            None => {
                let hir::Adt::Enum(e) = adt else { continue };
                let mut variants: Vec<String, Global> = Vec::new_in(Global);
                variants.extend(e.variants(db).into_iter().map(|it| it.name(db).as_str().to_owned()));
                let mut lists = Lists {
                    adt,
                    name: e.name(db).as_str().to_owned(),
                    variants,
                    node: None,
                    lists: Vec::new_in(Global),
                };
                lists.lists.push(list);
                all.push(lists);
            }
        }
    }
    for lists in &all {
        judge(cx, lists);
    }
}

/// The lists of the enum `name` in its impl `imp`.
fn in_impl<'t>(name: &str, imp: ast::Impl<'t>, out: &mut Vec<List<'t>, Global>) {
    let Some(items) = imp.assoc_item_list() else { return };
    for item in items.assoc_items() {
        match item {
            ast::AssocItem::Const(c) => {
                let (Some(id), Some(body)) = (c.name(), c.body()) else { continue };
                let what = format!("`{}`", id.text());
                let range = id.syntax().text_range();
                // An array of strings is a table only by its type, `[&str; Self::COUNT]`:
                // [`table`] finds it among the file's consts.
                let kind = match peel_ref(body) {
                    ast::Expr::ArrayExpr(array) => variants_of(name, array)
                        .map(|(names, prefix)| Kind::All { names, array, prefix }),
                    ast::Expr::Literal(lit) if COUNTS.contains(&id.text()) => match lit.kind() {
                        ast::LiteralKind::IntNumber(it) => it
                            .value()
                            .ok()
                            .map(|value| Kind::Count { value, literal: lit.syntax().text_range() }),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(kind) = kind {
                    out.push(List { what, node: c.syntax(), range, kind });
                }
            }
            ast::AssocItem::Fn(f) => {
                let (Some(id), Some(body)) = (f.name(), f.body()) else { continue };
                let Some(list) = body.stmt_list() else { continue };
                if list.statements().next().is_some() {
                    continue;
                }
                let Some(ast::Expr::MatchExpr(found)) = list.tail_expr().map(|it| it.peel_parens()) else {
                    continue;
                };
                let Some(kind) = matched(name, found) else { continue };
                let what = format!("`fn {}`", id.text());
                out.push(List { what, node: f.syntax(), range: id.syntax().text_range(), kind });
            }
            _ => {}
        }
    }
}

/// `expr` without the `&` in front of it.
fn peel_ref(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    match expr {
        ast::Expr::RefExpr(it) => it.expr().map_or(expr, peel_ref),
        ast::Expr::ParenExpr(it) => it.expr().map_or(expr, peel_ref),
        _ => expr,
    }
}

/// The variant `path` names when it is `E::X` or `Self::X` of the enum `name`.
fn variant_of<'t>(name: &str, path: ast::Path<'t>) -> Option<&'t str> {
    let qualifier = path.qualifier()?;
    let of = single(qualifier)?;
    if of != "Self" && of != name {
        return None;
    }
    last(path)
}

/// The variants an array lists and the qualifier written before them, when every element is a
/// variant of the enum `name`.
fn variants_of<'t>(name: &str, array: ast::ArrayExpr<'t>) -> Option<(Vec<&'t str, Global>, &'t str)> {
    let mut names: Vec<&'t str, Global> = Vec::new_in(Global);
    let mut prefix = None;
    for element in array.exprs() {
        let ast::Expr::PathExpr(it) = element else { return None };
        let path = it.path()?;
        names.push(variant_of(name, path)?);
        prefix.get_or_insert(path.qualifier()?.syntax().text());
    }
    Some((names, prefix?))
}

/// The strings an array lists, each a string literal or a macro call whose last string is
/// it (`concat!(PREFIX, "parse")`).
fn strings_of<'t>(array: ast::ArrayExpr<'t>) -> Option<Vec<&'t str, Global>> {
    let mut out: Vec<&'t str, Global> = Vec::new_in(Global);
    for element in array.exprs() {
        out.push(last_string(element.syntax())?);
    }
    (!out.is_empty()).then_some(out)
}

/// The last string literal under `node`, without its quotes.
fn last_string(node: ast::Node<'_>) -> Option<&str> {
    let token = node
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| it.kind() == SyntaxKind::STRING)
        .last()?;
    unquote(token.text())
}

fn unquote(text: &str) -> Option<&str> {
    text.strip_prefix('"')?.strip_suffix('"')
}

/// The list a `match` of a function of the enum `name` is.
fn matched<'t>(name: &str, found: ast::MatchExpr<'t>) -> Option<Kind<'t>> {
    let scrutinee = found.expr()?;
    let on_self = matches!(peel_deref(scrutinee), ast::Expr::PathExpr(it)
        if it.path().and_then(single) == Some("self"));
    let arms = found.match_arm_list()?;
    if on_self {
        let mut names: Vec<(&'t str, &'t str), Global> = Vec::new_in(Global);
        let mut index: Vec<(&'t str, u128), Global> = Vec::new_in(Global);
        let mut single_arms = true;
        let mut wild = None;
        let mut named: Vec<&'t str, Global> = Vec::new_in(Global);
        let mut prefix = None;
        for arm in arms.arms() {
            let pat = arm.pat()?;
            if let ast::Pat::WildcardPat(it) = pat {
                wild = Some(it.syntax().text_range());
                continue;
            }
            let variants = arm_variants(name, pat)?;
            named.extend(variants.iter().copied());
            if prefix.is_none() {
                prefix = pat
                    .syntax()
                    .descendants()
                    .find_map(ast::Path::cast)
                    .and_then(|it| it.qualifier())
                    .map(|it| it.syntax().text());
            }
            let one = match variants.as_slice() {
                [one] => Some(*one),
                _ => None,
            };
            let value = arm.expr()?;
            match (one, value) {
                (Some(variant), ast::Expr::Literal(lit)) => match lit.kind() {
                    ast::LiteralKind::String(it) => names.push((variant, unquote(it.syntax().text())?)),
                    ast::LiteralKind::IntNumber(it) => index.push((variant, it.value().ok()?)),
                    _ => single_arms = false,
                },
                _ => single_arms = false,
            }
        }
        if let (Some(pat), Some(prefix)) = (wild, prefix) {
            // The wildcard's own value is no entry: the names and indices below are the
            // named arms only, and the wildcard is judged on its own.
            return Some(Kind::Wild { named, pat, prefix });
        }
        let arms_count = arms.arms().count();
        if single_arms && names.len() == arms_count {
            return Some(Kind::Names { entries: names });
        }
        if single_arms && index.len() == arms_count {
            return Some(Kind::Index { entries: index });
        }
        return Some(Kind::Facts);
    }
    let mut entries: Vec<(&'t str, &'t str), Global> = Vec::new_in(Global);
    for arm in arms.arms() {
        let pat = arm.pat()?;
        if let ast::Pat::WildcardPat(_) = pat {
            continue;
        }
        let ast::Pat::LiteralPat(lit) = pat else { return None };
        let text = unquote(lit.syntax().text().trim())?;
        let variant = variant_in(name, arm.expr()?)?;
        entries.push((text, variant));
    }
    (!entries.is_empty()).then_some(Kind::Parse { entries })
}

fn peel_deref(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    match expr {
        ast::Expr::PrefixExpr(it) => it.expr().map_or(expr, peel_deref),
        ast::Expr::ParenExpr(it) => it.expr().map_or(expr, peel_deref),
        _ => expr,
    }
}

/// The variants a match arm's pattern names (`E::A`, `E::A(..)`, `E::A { .. }`, `E::A | E::B`);
/// `None` for any other pattern, a wildcard too (the arm then lists no variant).
fn arm_variants<'t>(name: &str, pat: ast::Pat<'t>) -> Option<Vec<&'t str, Global>> {
    let mut out: Vec<&'t str, Global> = Vec::new_in(Global);
    let mut stack: Vec<ast::Pat<'t>, Global> = Vec::new_in(Global);
    stack.push(pat);
    while let Some(pat) = stack.pop() {
        let path = match pat {
            ast::Pat::OrPat(it) => {
                stack.extend(it.pats());
                continue;
            }
            ast::Pat::PathPat(it) => it.path(),
            ast::Pat::TupleStructPat(it) => it.path(),
            ast::Pat::RecordPat(it) => it.path(),
            _ => return None,
        };
        out.push(variant_of(name, path?)?);
    }
    Some(out)
}

/// The variant an arm gives: `E::A`, `Some(E::A)`, `Ok(E::A)`.
fn variant_in<'t>(name: &str, expr: ast::Expr<'t>) -> Option<&'t str> {
    match expr.peel_parens() {
        ast::Expr::PathExpr(it) => variant_of(name, it.path()?),
        ast::Expr::CallExpr(call) => {
            let callee = match call.expr()? {
                ast::Expr::PathExpr(it) => single(it.path()?)?,
                _ => return None,
            };
            if callee != "Some" && callee != "Ok" {
                return None;
            }
            let mut args = ast::HasArgList::arg_list(&call)?.args();
            match (args.next(), args.next()) {
                (Some(arg), None) => variant_in(name, arg),
                _ => None,
            }
        }
        _ => None,
    }
}

/// A `const` or `static` array of strings whose length is an enum's count: the enum and the
/// list.
fn table<'t>(cx: &Cx<'_, 't>, items: &Items<'t>, node: ast::Node<'t>) -> Option<(hir::Adt, List<'t>)> {
    let (name, ty, body) = if let Some(it) = ast::Const::cast(node) {
        (it.name()?, it.ty()?, it.body()?)
    } else {
        let it = ast::Static::cast(node)?;
        (it.name()?, it.ty()?, it.body()?)
    };
    let ast::Type::ArrayType(array) = ty else { return None };
    let length = array.const_arg()?.syntax().descendants().find_map(ast::Path::cast)?;
    let qualifier = length.qualifier()?;
    let counted = last(length).is_some_and(|it| COUNTS.contains(&it) || it == "ALL");
    if !counted {
        return None;
    }
    let adt = match cx.path(qualifier)? {
        hir::PathResolution::Def(hir::ModuleDef::Adt(adt @ hir::Adt::Enum(_))) => adt,
        hir::PathResolution::SelfType(imp) => match imp.self_ty(cx.db()).as_adt()? {
            adt @ hir::Adt::Enum(_) => adt,
            _ => return None,
        },
        _ => return None,
    };
    let (strings, array) = match peel_ref(body) {
        ast::Expr::ArrayExpr(it) => (strings_of(it)?, Some(it)),
        ast::Expr::MacroExpr(it) => {
            let call = it.macro_call()?;
            let called = last(call.path()?)?;
            let rules = items.macros.iter().find(|it| it.name().is_some_and(|it| it.text() == called))?;
            (macro_strings(*rules)?, None)
        }
        _ => return None,
    };
    let what = format!("`{}`", name.text());
    let range = name.syntax().text_range();
    Some((adt, List { what, node, range, kind: Kind::Table { strings, array } }))
}

/// The strings a one-rule `macro_rules!` lists: its body's first bracketed group, the last
/// string of each element.
fn macro_strings<'t>(rules: ast::MacroRules<'t>) -> Option<Vec<&'t str, Global>> {
    let tree = rules.token_tree()?;
    let mut groups = tree.syntax().children().filter(|it| it.kind() == SyntaxKind::TOKEN_TREE);
    let (_, body) = (groups.next()?, groups.next()?);
    if groups.next().is_some() {
        return None;
    }
    let bracket = body.descendants().skip(1).find(|it| {
        it.kind() == SyntaxKind::TOKEN_TREE && it.first_token().is_some_and(|it| it.kind() == SyntaxKind::L_BRACK)
    })?;
    let mut out: Vec<&'t str, Global> = Vec::new_in(Global);
    let mut current: Option<&'t str> = None;
    for element in bracket.descendants_with_tokens().skip(1) {
        let Some(token) = element.into_token() else { continue };
        // The token trees between the token and the bracket: 0 for an element's own comma.
        let depth = token.ancestors().take_while(|it| *it != bracket).count();
        match token.kind() {
            SyntaxKind::STRING => current = unquote(token.text()),
            SyntaxKind::COMMA | SyntaxKind::R_BRACK if depth == 0 => {
                if let Some(it) = current.take() {
                    out.push(it);
                }
            }
            _ => {}
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The ways a variant's name is written, the rule of each: its own name, snake case, lower
/// case, kebab case, screaming snake case.
fn spellings(variant: &str) -> [String; 5] {
    let snake = snake(variant);
    [
        variant.to_owned(),
        snake.clone(),
        variant.to_lowercase(),
        snake.replace('_', "-"),
        snake.to_uppercase(),
    ]
}

/// `MagicString` as `magic_string`, `IoError` as `io_error`, `HTTPServer` as `http_server`.
fn snake(name: &str) -> String {
    let chars: Vec<char, Global> = {
        let mut it: Vec<char, Global> = Vec::new_in(Global);
        it.extend(name.chars());
        it
    };
    let mut out = String::new();
    for (at, &c) in chars.iter().enumerate() {
        if c.is_uppercase() && at > 0 {
            let before = chars[at - 1];
            let after = chars.get(at + 1).copied();
            let starts = before.is_lowercase()
                || before.is_ascii_digit()
                || (before.is_uppercase() && after.is_some_and(char::is_lowercase));
            if starts && before != '_' {
                out.push('_');
            }
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// The prefix every string shares, cut back to just after a character that is not part of a
/// name (`"tag live bytes: "`, `"#"`).
fn shared_prefix<'a>(strings: &[&'a str]) -> &'a str {
    let Some(first) = strings.first() else { return "" };
    if strings.len() < 2 {
        return "";
    }
    let mut len = first.len();
    for it in strings {
        len = len.min(first.bytes().zip(it.bytes()).take_while(|(a, b)| a == b).count());
    }
    while !first.is_char_boundary(len) {
        len -= 1;
    }
    let cut = first[..len]
        .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(0, |at| at + first[at..].chars().next().map_or(1, char::len_utf8));
    &first[..cut]
}

/// The edits from `a` to `b`, counted up to `bound + 1`.
fn distance(a: &str, b: &str, bound: usize) -> usize {
    let a: Vec<char, Global> = {
        let mut it: Vec<char, Global> = Vec::new_in(Global);
        it.extend(a.chars());
        it
    };
    let mut row: Vec<usize, Global> = Vec::new_in(Global);
    row.extend(0..=a.len());
    for (i, cb) in b.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for j in 0..a.len() {
            let above = row[j + 1];
            row[j + 1] = (above + 1).min(row[j] + 1).min(diagonal + usize::from(a[j] != cb));
            diagonal = above;
        }
        if row.iter().min().is_some_and(|&it| it > bound) {
            return bound + 1;
        }
    }
    row[a.len()]
}

/// The rule (an index into [`spellings`]) the most `(variant, name)` pairs follow, and how many.
fn rule_of(pairs: &[(&str, &str)]) -> (usize, usize) {
    let mut best = (0, 0);
    for rule in 0..5 {
        let follows = pairs.iter().filter(|(variant, name)| spellings(variant)[rule] == *name).count();
        if follows > best.1 {
            best = (rule, follows);
        }
    }
    best
}

/// A drift found in a list: what is wrong, and the list regenerated where it is one array.
struct Drift {
    message: String,
    fix: Option<Fix>,
}

fn judge<'t>(cx: &mut Cx<'_, 't>, lists: &Lists<'t>) {
    let n = lists.variants.len();
    let e = &lists.name;
    let mut derivable: Vec<&str, Global> = Vec::new_in(Global);
    let mut facts = 0;
    let names_list = lists.lists.iter().find_map(|it| match &it.kind {
        Kind::Names { entries } => Some(entries),
        _ => None,
    });
    for list in &lists.lists {
        let drift = match &list.kind {
            Kind::All { names, array, prefix } => all(lists, list, names, *array, prefix),
            Kind::Count { value, literal } => (usize::try_from(*value).ok() != Some(n)).then(|| Drift {
                message: format!("{} of `{e}` is {value}, but `{e}` has {n} variants", list.what),
                fix: Some(Fix {
                    message: "count the variants",
                    parts: one_part(*literal, n.to_string()),
                    applicability: Applicability::MaybeIncorrect,
                }),
            }),
            Kind::Names { entries } => names(lists, list, entries).map(|message| Drift { message, fix: None }),
            Kind::Parse { entries } => parse(lists, list, entries, names_list).map(|message| Drift { message, fix: None }),
            Kind::Index { entries } => {
                let mut seen: Vec<u128, Global> = Vec::new_in(Global);
                let repeated = entries.iter().find(|(_, it)| {
                    let again = seen.contains(it);
                    seen.push(*it);
                    again
                });
                repeated.map(|(variant, value)| Drift {
                    message: format!("{} of `{e}` gives `{variant}` the index {value}, which another variant has too", list.what),
                    fix: None,
                })
            }
            Kind::Table { strings, array } => table_drift(lists, list, strings, *array),
            Kind::Wild { named, pat, prefix } => wild(lists, list, named, *pat, prefix),
            Kind::Facts => None,
        };
        if let Some(drift) = drift {
            report(
                cx,
                Lint::VariantListDrift,
                list.node,
                list.range,
                drift.message,
                &format!(
                    "a list kept by hand beside `{e}` drifts from it when a variant is added, \
                     removed or renamed; regenerate it from the declaration (the end state is \
                     `variants_of::<{e}>()`, with `Variant::NAME` and `Variant::INDEX`)"
                ),
                drift.fix,
            );
            continue;
        }
        let rule_derives = match &list.kind {
            Kind::All { .. } | Kind::Count { .. } => true,
            Kind::Names { entries } => {
                let stripped = strip(entries.iter().map(|(_, it)| *it));
                let pairs = pairs_of(entries.iter().map(|(it, _)| *it), &stripped);
                rule_of(&pairs).1 == pairs.len()
            }
            Kind::Index { entries } => entries.iter().all(|(variant, value)| {
                lists.variants.iter().position(|it| it == variant).and_then(|at| u128::try_from(at).ok()) == Some(*value)
            }),
            Kind::Table { .. } | Kind::Parse { .. } => true,
            Kind::Wild { .. } | Kind::Facts => false,
        };
        if rule_derives {
            derivable.push(&list.what);
        } else {
            facts += 1;
        }
    }
    if !derivable.is_empty() && cx.on_blaze(Lint::HandKeptVariantList) {
        let (node, range) = match lists.node {
            Some(it) => (it.syntax(), it.name().map_or(it.syntax().text_range(), |it| it.syntax().text_range())),
            None => match lists.lists.first() {
                Some(it) => (it.node, it.range),
                None => return,
            },
        };
        let message = format!(
            "`{e}`'s {n} variants are listed again by hand in {}",
            derivable.join(", "),
        );
        let note = format!(
            "each is derivable from the declaration and is an edit with every new variant; no \
             declarative table or derive for it exists in the tree yet, and the end state is \
             `variants_of::<{e}>()` with `Variant::NAME` and `Variant::INDEX`"
        );
        report(cx, Lint::HandKeptVariantList, node, range, message, &note, None);
    }
    let total = lists.lists.len();
    if let Some(node) = lists.node
        && total >= SPREAD.0
        && facts >= SPREAD.1
        && cx.on_blaze(Lint::RegistrySpread)
    {
        let mut said: Vec<&str, Global> = Vec::new_in(Global);
        said.extend(lists.lists.iter().map(|it| it.what.as_str()));
        let message = format!(
            "each of `{e}`'s {n} variants has its facts spread over {total} parallel lists ({}): \
             a new variant is an edit in each",
            said.join(", "),
        );
        let range = node.name().map_or(node.syntax().text_range(), |it| it.syntax().text_range());
        report(
            cx,
            Lint::RegistrySpread,
            node.syntax(),
            range,
            message,
            "give each variant one record (a row of one `const` table, or one entry of one \
             declarative macro invocation) holding all its facts, and derive every list from the \
             rows, so a new variant is one row",
            None,
        );
    }
}

fn one_part(range: TextRange, text: String) -> Vec<(TextRange, String)> {
    let mut parts: Vec<(TextRange, String), Global> = Vec::new_in(Global);
    parts.push((range, text));
    parts
}

/// The strings with the prefix they all share stripped.
fn strip<'a>(strings: impl Iterator<Item = &'a str>) -> Vec<&'a str, Global> {
    let mut all: Vec<&'a str, Global> = Vec::new_in(Global);
    all.extend(strings);
    let prefix = shared_prefix(&all);
    let mut out: Vec<&'a str, Global> = Vec::new_in(Global);
    out.extend(all.iter().map(|it| &it[prefix.len()..]));
    out
}

fn pairs_of<'a>(variants: impl Iterator<Item = &'a str>, names: &[&'a str]) -> Vec<(&'a str, &'a str), Global> {
    let mut out: Vec<(&'a str, &'a str), Global> = Vec::new_in(Global);
    out.extend(variants.zip(names.iter().copied()));
    out
}

/// A list of the variants that misses or repeats one.
fn all<'t>(
    lists: &Lists<'t>,
    list: &List<'t>,
    names: &[&str],
    array: ast::ArrayExpr<'t>,
    prefix: &str,
) -> Option<Drift> {
    let e = &lists.name;
    let mut missing: Vec<&str, Global> = Vec::new_in(Global);
    missing.extend(lists.variants.iter().map(String::as_str).filter(|it| !names.contains(it)));
    let mut repeated: Vec<&str, Global> = Vec::new_in(Global);
    for (at, it) in names.iter().enumerate() {
        if names[..at].contains(it) && !repeated.contains(it) {
            repeated.push(it);
        }
    }
    if missing.is_empty() && repeated.is_empty() {
        return None;
    }
    let mut said: Vec<String, Global> = Vec::new_in(Global);
    if !missing.is_empty() {
        said.push(format!("misses {}", quoted(&missing)));
    }
    if !repeated.is_empty() {
        said.push(format!("repeats {}", quoted(&repeated)));
    }
    let message = format!(
        "{} lists {} of `{e}`'s {} variants: it {}",
        list.what,
        names.len(),
        lists.variants.len(),
        said.join(" and "),
    );
    let mut text = String::from("[");
    for (at, variant) in lists.variants.iter().enumerate() {
        if at > 0 {
            text.push_str(", ");
        }
        text.push_str(&format!("{prefix}::{variant}"));
    }
    text.push(']');
    let fix = Fix {
        message: "list every variant once, in declaration order",
        parts: one_part(array.syntax().text_range(), text),
        applicability: Applicability::MaybeIncorrect,
    };
    Some(Drift { message, fix: Some(fix) })
}

/// A match on `self` whose `_` arm stands for exactly one variant: that arm is the variant
/// written as "anything else", so a variant added later takes it silently instead of failing
/// to compile. A `_` over two variants or more is a grouping, and is left alone.
fn wild<'t>(lists: &Lists<'t>, list: &List<'t>, named: &[&str], pat: TextRange, prefix: &str) -> Option<Drift> {
    let e = &lists.name;
    let n = lists.variants.len();
    let mut missing = lists.variants.iter().filter(|it| !named.contains(&it.as_str()));
    let (Some(only), None) = (missing.next(), missing.next()) else { return None };
    if named.len() < 2 {
        return None;
    }
    let message = format!(
        "{} of `{e}` names {} of its {n} variants and leaves `{only}` to `_`: a variant added \
         later takes that arm silently",
        list.what,
        n - 1,
    );
    // The pattern for `only`: a unit variant is its path; one with fields is left to the
    // author, who knows whether `(..)` or `{ .. }` is meant.
    let unit = lists.node.and_then(|it| it.variant_list()).is_some_and(|it| {
        it.variants().any(|it| it.name().is_some_and(|name| name.text() == only) && it.field_list().is_none())
    });
    let fix = unit.then(|| Fix {
        message: "name the variant",
        parts: one_part(pat, format!("{prefix}::{only}")),
        applicability: Applicability::MaybeIncorrect,
    });
    Some(Drift { message, fix })
}

fn quoted(names: &[&str]) -> String {
    let mut out: Vec<String, Global> = Vec::new_in(Global);
    out.extend(names.iter().map(|it| format!("`{it}`")));
    out.join(", ")
}

/// The name a list gives a variant that is not its own under the list's rule.
fn names<'t>(lists: &Lists<'t>, list: &List<'t>, entries: &[(&str, &str)]) -> Option<String> {
    let e = &lists.name;
    let stripped = strip(entries.iter().map(|(_, it)| *it));
    let pairs = pairs_of(entries.iter().map(|(it, _)| *it), &stripped);
    for (at, (_, name)) in pairs.iter().enumerate() {
        if let Some((other, _)) = pairs[..at].iter().find(|(_, it)| it == name) {
            return Some(format!("{} of `{e}` names `{other}` and `{}` both \"{name}\"", list.what, pairs[at].0));
        }
    }
    misspelled(e, &list.what, &pairs)
}

/// The pair that breaks a rule the rest follow: a name within two edits of what the rule makes
/// of its variant, or that is another variant's.
fn misspelled(e: &str, what: &str, pairs: &[(&str, &str)]) -> Option<String> {
    let (rule, follows) = rule_of(pairs);
    let n = pairs.len();
    if n < 3 || follows == n || follows * 5 < n * 4 {
        return None;
    }
    for (variant, name) in pairs {
        let expected = &spellings(variant)[rule];
        if expected == name {
            continue;
        }
        if let Some((owner, _)) = pairs.iter().find(|(other, _)| spellings(other)[rule] == *name) {
            return Some(format!(
                "{what} of `{e}` names `{variant}` \"{name}\", which is `{owner}`'s name; by the rule \
                 the others follow it is \"{expected}\""
            ));
        }
        if distance(expected, name, 2) <= 2 {
            return Some(format!(
                "{what} of `{e}` names `{variant}` \"{name}\"; by the rule the others follow it is \
                 \"{expected}\""
            ));
        }
    }
    None
}

/// A parse that does not give back the variant `fn name` names, or misspells one.
fn parse<'t>(
    lists: &Lists<'t>,
    list: &List<'t>,
    entries: &[(&str, &str)],
    names: Option<&Vec<(&str, &str), Global>>,
) -> Option<String> {
    let e = &lists.name;
    if let Some(names) = names {
        for (variant, name) in names {
            match entries.iter().find(|(text, _)| text == name) {
                Some((_, parsed)) if parsed != variant => {
                    return Some(format!(
                        "{} of `{e}` parses \"{name}\" as `{parsed}`, but `{variant}` is named \"{name}\"",
                        list.what
                    ));
                }
                None if entries.iter().any(|(_, it)| it == variant) => {
                    return Some(format!(
                        "{} of `{e}` does not parse \"{name}\", the name of `{variant}`",
                        list.what
                    ));
                }
                _ => {}
            }
        }
        return None;
    }
    let mut pairs: Vec<(&str, &str), Global> = Vec::new_in(Global);
    pairs.extend(entries.iter().map(|(text, variant)| (*variant, *text)));
    misspelled(e, &list.what, &pairs)
}

/// A table indexed by the enum whose length, names or order are not the declaration's.
fn table_drift<'t>(
    lists: &Lists<'t>,
    list: &List<'t>,
    strings: &[&str],
    array: Option<ast::ArrayExpr<'t>>,
) -> Option<Drift> {
    let e = &lists.name;
    let n = lists.variants.len();
    let prefix = shared_prefix(strings);
    let stripped = strip(strings.iter().copied());
    let mut variants: Vec<&str, Global> = Vec::new_in(Global);
    variants.extend(lists.variants.iter().map(String::as_str));
    let regenerate = |rule: usize| {
        let array = array?;
        let mut text = String::from("[");
        for (at, variant) in variants.iter().enumerate() {
            if at > 0 {
                text.push_str(", ");
            }
            text.push_str(&format!("\"{prefix}{}\"", spellings(variant)[rule]));
        }
        text.push(']');
        Some(Fix {
            message: "one name per variant, in declaration order",
            parts: one_part(array.syntax().text_range(), text),
            applicability: Applicability::MaybeIncorrect,
        })
    };
    if strings.len() != n {
        let pairs = pairs_of(variants.iter().copied(), &stripped);
        let (rule, _) = rule_of(&pairs);
        return Some(Drift {
            message: format!("{} has {} names for `{e}`'s {n} variants", list.what, strings.len()),
            fix: regenerate(rule),
        });
    }
    let pairs = pairs_of(variants.iter().copied(), &stripped);
    let (rule, follows) = rule_of(&pairs);
    if follows == n {
        return None;
    }
    // In another order: every name is some variant's under the rule.
    let as_set = stripped.iter().all(|name| variants.iter().any(|it| spellings(it)[rule] == *name));
    if as_set && follows * 5 < n * 4 {
        return Some(Drift {
            message: format!("{} names `{e}`'s variants in another order than it declares them", list.what),
            fix: regenerate(rule),
        });
    }
    misspelled(e, &list.what, &pairs).map(|message| Drift { message, fix: regenerate(rule) })
}
