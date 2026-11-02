//! `non_snake_case` and `non_upper_case_globals` of the names hir's `IncorrectCase` says
//! nothing of (`crates/hir-ty/src/diagnostics/decl_check.rs`, lowered in
//! `lower/incorrect_case.rs`), so that each name rustc lints is linted once:
//!
//! - `IncorrectCase`: modules, functions and methods (not of trait impls, not `#[no_mangle]`
//!   of a foreign ABI), the bindings and parameters of functions' bodies, fields, constants and
//!   associated constants (not of trait impls), statics (not `extern`, not `#[no_mangle]`).
//! - here: the crate's name; lifetime parameters; const parameters; the parameters named in
//!   function pointer types; the bindings of the bodies of constants, statics and enum
//!   discriminants; a single-segment path pattern naming a constant ("constant in pattern");
//!   a trait's methods that are `#[no_mangle]` of a foreign ABI, which rustc lints and
//!   `IncorrectCase` passes over.
//!
//! `non_camel_case_types` is rustc's early lint pass's, linted with the syntax lints.
//!
//! Follows `compiler/rustc_lint/src/nonstandard_style.rs` (`NonSnakeCase`,
//! `NonUpperCaseGlobals`) and `compiler/rustc_lint/src/lints.rs` (`NonSnakeCaseDiag`,
//! `NonUpperCaseGlobal`) (MIT or Apache-2.0).

use std::cell::OnceCell;

use base_db::TargetKind;
use blaze_items::Fact;
use hir::{InFile, ModuleDef, PathResolution};
use span::Edition;
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self as ast, AstNode, AstToken as _, HasGenericArgs, HasName},
};

use super::walk::{self, Container, Item};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Code, Diag, Level, MultiSpan, Span, SuggestionStyle},
};

/// What a name's lint read: the item's signature or code, by where the name is. Made only
/// for a lint that fires.
type Read = Vec<(Fact, String)>;

/// rustc's `check_mod` of the crate root: the crate's name, unless every crate type it is
/// built as is an executable (#45127).
pub(super) fn krate(cx: &Cx<'_, '_>) -> Option<Found> {
    let executable = matches!(
        cx.krate.target_kind(cx.db),
        TargetKind::Bin | TargetKind::Example | TargetKind::Test | TargetKind::Bench | TargetKind::BuildScript
    );
    if executable {
        return None;
    }
    let root = cx.krate.root_module(cx.db);
    let file = root.definition_source_file_id(cx.db);
    let source_file = ast::SourceFile::cast(cx.root(file))?;
    let at = InFile::new(file, ast::ptr(source_file.syntax()));
    // alloc: the fact of a lint that fires, made only then
    let read = || vec![(Fact::Name(cx.module_key(root)), "the name of the crate".to_owned())];
    // `#![crate_name = ".."]`: the span of the name between its quotes.
    if let Some((ast::Meta::KeyValueMeta(kv), _)) = walk::attr(cx, source_file, "crate_name")
        && let Some(ast::Expr::Literal(literal)) = kv.expr()
    {
        let token = literal.token();
        let text = token.text();
        let (Some(left), Some(right)) = (text.find('"'), text.rfind('"')) else { return None };
        if right <= left {
            return None;
        }
        let start = token.text_range().start();
        let range = TextRange::new(start + TextSize::from(left as u32 + 1), start + TextSize::from(right as u32));
        return snake(cx, "crate", &text[left + 1..right], Some(InFile::new(file, range)), at, read);
    }
    // As cargo runs rustc, `--crate-name` names it, with no span.
    let name = cx.krate.display_name(cx.db)?;
    snake(cx, "crate", name.crate_name().symbol().as_str(), None, at, read)
}

/// What rustc's `NonSnakeCase` and `NonUpperCaseGlobals` lint of `item` that `IncorrectCase`
/// does not, in the order rustc visits its nodes. Each node is told apart by its kind, and a
/// name is tested for its case before anything resolves it.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    // alloc: empty, no allocation, unless a name fires a lint
    let mut found = Vec::new();
    let def = OnceCell::new();
    let read = |fact: fn(blaze_items::ItemKey) -> Fact, what: &'static str| {
        let def = &def;
        move || -> Read {
            match def.get_or_init(|| super::item_id(cx, item)) {
                // alloc: the fact of a lint that fires, made only then
                Some((id, name)) => vec![(fact(cx.key(*id)), format!("the {what} of `{name}`"))],
                None => Vec::new(), // alloc: empty, no allocation
            }
        }
    };
    if let ast::Item::Fn(it) = item.node.value
        && let Container::Trait(_) = item.container
        && !is_rust_abi(it.abi())
        && walk::attr(cx, it, "no_mangle").is_some()
        && let Some(name) = it.name()
    {
        let range = item.node.with_value(name.syntax().text_range());
        found.extend(snake(cx, "trait method", name.text(), Some(range), item.ptr(), read(Fact::Name, "name")));
    }
    let bindings = matches!(&item.node.value, ast::Item::Const(_) | ast::Item::Static(_) | ast::Item::Enum(_));
    for &node in item.nodes() {
        let at = || node.map(ast::ptr);
        match node.value.kind() {
            SyntaxKind::LIFETIME_PARAM => {
                let Some(lifetime) = ast::LifetimeParam::cast(node.value).and_then(|it| it.lifetime()) else {
                    continue;
                };
                let range = node.with_value(lifetime.syntax().text_range());
                found.extend(snake(cx, "lifetime", lifetime.text(), Some(range), at(), read(Fact::Signature, "signature")));
            }
            SyntaxKind::CONST_PARAM => {
                let Some(param) = ast::ConstParam::cast(node.value) else { continue };
                let Some(name) = param.name() else { continue };
                let range = node.with_value(name.syntax().text_range());
                let usages = || const_param_usages(cx, item, node.with_value(param), name.text());
                let read = read(Fact::Signature, "signature");
                found.extend(upper(cx, "const parameter", name.text(), range, Some(usages), at(), read));
            }
            SyntaxKind::FN_PTR_TYPE => {
                let Some(ty) = ast::FnPtrType::cast(node.value) else { continue };
                for param in ty.param_list().into_iter().flat_map(|it| it.params()) {
                    if let Some(ast::Pat::IdentPat(pat)) = param.pat()
                        && let Some(name) = pat.name()
                    {
                        let range = node.with_value(name.syntax().text_range());
                        let at = node.with_value(ast::ptr(pat.syntax()));
                        found.extend(snake(cx, "variable", name.text(), Some(range), at, read(Fact::Signature, "signature")));
                    }
                }
            }
            SyntaxKind::IDENT_PAT => {
                let Some(pat) = ast::IdentPat::cast(node.value) else { continue };
                found.extend(ident_pat(cx, node.with_value(pat), bindings, read(Fact::Code, "code")));
            }
            SyntaxKind::PATH_PAT => {
                if let Some(pat) = ast::PathPat::cast(node.value)
                    && let Some(path) = pat.path()
                    && path.qualifier().is_none()
                    && let Some(segment) = path.segment()
                    && segment.generic_arg_list().is_none()
                    && let Some(name) = segment.name_ref()
                    && !is_upper_case(name.text())
                    && let Some(PathResolution::Def(ModuleDef::Const(_))) = cx.vsema.resolve_path(path)
                {
                    let range = node.with_value(name.syntax().text_range());
                    found.extend(upper(cx, "constant in pattern", name.text(), range, NO_USAGES, at(), read(Fact::Code, "code")));
                }
            }
            _ => {}
        }
    }
    found
}

/// No usages to rename: the name is exported, or not a parameter's.
const NO_USAGES: Option<fn() -> Vec<Span>> = None;

/// An identifier pattern: a constant it names, or a binding of a body `IncorrectCase` does
/// not look into (`bindings`: the item is a constant, a static or an enum's discriminants).
/// The name's case is tested first: most names neither lint could fire for are never
/// resolved.
fn ident_pat<'db>(cx: &Cx<'_, 'db>, pat: InFile<ast::IdentPat<'db>>, bindings: bool, read: impl FnOnce() -> Read) -> Option<Found> {
    let name = pat.value.name()?;
    let text = name.text();
    let upper_case = is_upper_case(text);
    // A constant lints only when its name is not upper case, a binding only when it is not
    // snake case.
    if upper_case && (!bindings || is_snake_case(text)) {
        return None;
    }
    // The parameters of a function pointer type are linted with it.
    if pat.value.syntax().ancestors().any(|it| ast::FnPtrType::can_cast(it.kind())) {
        return None;
    }
    let range = pat.with_value(name.syntax().text_range());
    let at = pat.map(|it| ast::ptr(it.syntax()));
    let plain = pat.value.pat().is_none() && pat.value.ref_token().is_none() && pat.value.mut_token().is_none();
    if plain && let Some(def) = cx.vsema.resolve_bind_pat_to_const(pat.value) {
        return match def {
            ModuleDef::Const(_) => upper(cx, "constant in pattern", text, range, NO_USAGES, at, read),
            _ => None,
        };
    }
    if !bindings {
        return None;
    }
    // A shorthand field pattern names the field, not a new binding: the field is linted.
    let shorthand =
        pat.value.syntax().parent().and_then(ast::RecordPatField::cast).is_some_and(|it| it.name_ref().is_none());
    if shorthand {
        return None;
    }
    snake(cx, "variable", text, Some(range), at, read)
}

/// The paths in `item` naming the const parameter `param`, which the suggestion renames too.
fn const_param_usages<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, param: InFile<ast::ConstParam<'db>>, name: &str) -> Vec<Span> {
    // alloc: the renames of a lint that fires, read only then
    let Some(def) = cx.vsema.to_def2(param) else { return Vec::new() };
    let mut usages = Vec::new(); // alloc: as above
    for path in item.own::<ast::Path<'db>>() {
        if path.value.qualifier().is_some() {
            continue;
        }
        let Some(name_ref) = path.value.segment().and_then(|it| it.name_ref()) else { continue };
        if name_ref.text() != name {
            continue;
        }
        if let Some(PathResolution::ConstParam(it)) = cx.vsema.resolve_path(path.value)
            && it == def
        {
            usages.push(cx.span(path.with_value(name_ref.syntax().text_range())));
        }
    }
    usages
}

/// Whether a function of `abi` is of the Rust ABI: no `extern`, or `extern "Rust"`.
pub(super) fn is_rust_abi(abi: Option<ast::Abi<'_>>) -> bool {
    match abi {
        None => true,
        Some(abi) => abi.abi_string().is_some_and(|it| it.text().trim_matches('"') == "Rust"),
    }
}

/// rustc's `check_snake_case`: the lint of `name`, of the kind `sort`, when it is not snake
/// case. `range` is `None` for the crate's name given on the command line.
fn snake(
    cx: &Cx<'_, '_>,
    sort: &str,
    name: &str,
    range: Option<InFile<TextRange>>,
    at: InFile<SyntaxNodePtr>,
    read: impl FnOnce() -> Read,
) -> Option<Found> {
    if is_snake_case(name) {
        return None;
    }
    let span = range.map(|it| cx.span(it));
    // alloc: the message of a lint that fires
    let message = format!("{sort} `{name}` should have a snake case name");
    let sc = to_snake_case(name);
    let mut diag = match &span {
        Some(span) => Diag::lint("non_snake_case", message, span.clone()), // clone: the span of a lint that fires
        None => {
            // alloc: the code of a lint that fires
            let mut diag = Diag::new(Level::Warning, Some(Code::Lint("non_snake_case".to_owned())), message);
            diag.span = MultiSpan::default();
            diag
        }
    };
    match span {
        Some(span) if name == sc => {
            diag.span_label(span, "should have a snake_case name");
        }
        None if name == sc => {}
        None => {
            diag.help(format!("convert the identifier to snake case: `{sc}`")); // alloc: a lint that fires
        }
        Some(span) if is_reserved(&sc, cx.edition) => {
            if can_be_raw(&sc) {
                diag.suggestion(
                    span,
                    "rename the identifier or convert it to a snake case raw identifier",
                    format!("r#{sc}"), // alloc: a lint that fires
                    Applicability::MaybeIncorrect,
                );
            } else {
                diag.note(format!("`{sc}` cannot be used as a raw identifier")); // alloc: a lint that fires
                diag.suggestion(span, "rename the identifier", "", Applicability::MaybeIncorrect);
            }
        }
        Some(span) => {
            diag.suggestion(span, "convert the identifier to snake case", sc, Applicability::MaybeIncorrect);
        }
    }
    for (fact, what) in read() {
        diag.fact(fact, what);
    }
    Some(Found::lint(diag, Phase::Late, at))
}

/// rustc's `check_upper_case`: the lint of `name`, of the kind `sort`, when it is not upper
/// case; with `usages` when it is not exported, and then the paths naming it renamed too.
fn upper(
    cx: &Cx<'_, '_>,
    sort: &str,
    name: &str,
    range: InFile<TextRange>,
    usages: Option<impl FnOnce() -> Vec<Span>>,
    at: InFile<SyntaxNodePtr>,
    read: impl FnOnce() -> Read,
) -> Option<Found> {
    if is_upper_case(name) {
        return None;
    }
    let span = cx.span(range);
    let uc = to_snake_case(name).to_uppercase();
    // alloc: the message of a lint that fires
    let message = format!("{sort} `{name}` should have an upper case name");
    let mut diag = Diag::lint("non_upper_case_globals", message, span.clone()); // clone: the span of a lint that fires
    if name != uc {
        let applicability = match usages.is_some() {
            true => Applicability::MachineApplicable,
            false => Applicability::MaybeIncorrect,
        };
        diag.suggestion(span, "convert the identifier to upper case", uc.clone(), applicability); // clone: a lint that fires
        if let Some(usages) = usages {
            for usage in usages() {
                diag.suggestion_with_style(
                    vec![(usage, uc.clone())], // clone: one rename of a lint that fires
                    "convert the identifier to upper case",
                    Applicability::MachineApplicable,
                    SuggestionStyle::CompletelyHidden,
                );
            }
        }
    } else {
        diag.span_label(span, "should have an UPPER_CASE name");
    }
    for (fact, what) in read() {
        diag.fact(fact, what);
    }
    Some(Found::lint(diag, Phase::Late, at))
}

/// The test of rustc's `check_upper_case`: no character changes when upper-cased.
fn is_upper_case(name: &str) -> bool {
    name.chars().all(|c| c.to_uppercase().eq([c]))
}

/// rustc's `is_snake_case` of `NonSnakeCase::check_snake_case`.
pub(super) fn is_snake_case(ident: &str) -> bool {
    if ident.is_empty() {
        return true;
    }
    let ident = ident.trim_start_matches('\'');
    let ident = ident.trim_matches('_');
    if ident.contains("__") {
        return false;
    }
    ident.chars().all(|c| c.to_lowercase().eq([c]))
}

/// rustc's `NonSnakeCase::to_snake_case`, only for a name a lint fires for.
pub(super) fn to_snake_case(mut name: &str) -> String {
    let mut words = vec![]; // alloc: the suggestion of a lint that fires
    name = name.trim_start_matches(|c: char| {
        if c == '_' {
            words.push(String::new()); // alloc: empty, no allocation
            true
        } else {
            false
        }
    });
    for s in name.split('_') {
        let mut last_upper = false;
        let mut buf = String::new(); // alloc: the suggestion of a lint that fires
        if s.is_empty() {
            continue;
        }
        for ch in s.chars() {
            if !buf.is_empty() && buf != "'" && (ch.is_uppercase() || is_titlecase(ch)) && !last_upper {
                words.push(buf.to_lowercase());
                buf = String::new(); // alloc: the suggestion of a lint that fires
            }
            last_upper = ch.is_uppercase() || is_titlecase(ch);
            buf.push(ch);
        }
        words.push(buf.to_lowercase());
    }
    words.join("_")
}

/// `char::is_titlecase`, which is unstable: the letters of Unicode's general category `Lt`.
fn is_titlecase(c: char) -> bool {
    matches!(
        c as u32,
        0x01c5 | 0x01c8 | 0x01cb | 0x01f2 | 0x1f88..=0x1f8f | 0x1f98..=0x1f9f | 0x1fa8..=0x1faf | 0x1fbc | 0x1fcc | 0x1ffc
    )
}

/// rustc's `Symbol::is_reserved`: a special identifier or a keyword of `edition`.
fn is_reserved(name: &str, edition: Edition) -> bool {
    const ALWAYS: &[&str] = &[
        "$crate", "{{root}}", "_", "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self",
        "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while", "abstract", "become",
        "box", "do", "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield",
    ];
    ALWAYS.contains(&name)
        || (matches!(name, "async" | "await" | "dyn" | "try") && edition >= Edition::Edition2018)
        || (name == "gen" && edition >= Edition::Edition2024)
}

/// rustc's `Symbol::can_be_raw`.
fn can_be_raw(name: &str) -> bool {
    !matches!(name, "" | "_" | "super" | "self" | "Self" | "crate" | "{{root}}" | "$crate")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_is_rustcs() {
        assert!(is_snake_case("foo_bar"));
        assert!(is_snake_case("_foo"));
        assert!(is_snake_case("'a"));
        assert!(is_snake_case("x86_64"));
        assert!(!is_snake_case("foo__bar"));
        assert!(!is_snake_case("FooBar"));
        assert!(!is_snake_case("'FooBar"));
        assert_eq!(to_snake_case("FooBar"), "foo_bar");
        assert_eq!(to_snake_case("'FooBar"), "'foo_bar");
        assert_eq!(to_snake_case("__FooBar"), "__foo_bar");
        assert_eq!(to_snake_case("HTTPServer"), "httpserver");
        assert_eq!(to_snake_case("NonSnakeCase"), "non_snake_case");
        assert_eq!(to_snake_case("x").to_uppercase(), "X");
    }

    #[test]
    fn reserved_names_follow_the_edition() {
        assert!(is_reserved("fn", Edition::Edition2015));
        assert!(!is_reserved("async", Edition::Edition2015));
        assert!(is_reserved("async", Edition::Edition2018));
        assert!(!is_reserved("gen", Edition::Edition2021));
        assert!(is_reserved("gen", Edition::Edition2024));
        assert!(can_be_raw("fn"));
        assert!(!can_be_raw("self"));
    }
}
