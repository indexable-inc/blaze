//! What rustc's attribute parser says of the builtin attributes of each node as it parses
//! them, in the order it says it: for each attribute, its safety (`unsafe_attr_outside_unsafe`,
//! `unsafe_code`), what its parser says of its input (an empty list, an ill-formed input, a
//! second one where one is used, an unknown crate type, a derive helper named as a builtin
//! attribute), then its target (`unused_attributes` or `misplaced_diagnostic_attributes`: an
//! attribute where it does nothing; a crate-level attribute below the crate root).
//!
//! A parser takes an attribute, and so counts a second one as unused, when the attribute has
//! one of the forms of its template (a parser of no arguments takes any); the parsers that
//! lint their input themselves (`inline`, `link`, `ignore`, `used`, `macro_use`, `crate_type`,
//! `proc_macro_derive`, `repr`, the lists of `feature`, `target_feature`, `register_tool`)
//! are ported. Its errors (E0565, E0538, an unsafe attribute without `unsafe(..)` from the
//! edition it became unsafe, a target that is an error) are `diagnostics-items`' `checks/
//! attrs.rs`; `#[doc]`'s and `#[diagnostic::..]`'s own lints are `doc`'s and `diagnostic`'s;
//! `useless_deprecated` (`#[deprecated]` where it does nothing) is `diagnostics-lints-names`'.
//!
//! rustc lints no deprecated builtin attribute: `deprecated_attr` is removed
//! (`compiler/rustc_lint/src/lib.rs`).
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/interface.rs` (`parse_attribute_list`),
//! `safety.rs`, `target_checking.rs`, `context.rs` (`warn_unused_duplicate`,
//! `warn_empty_attribute`, `warn_ill_formed_attribute_input`), `attributes/mod.rs`
//! (`Single`, `Combine`), `attributes/{inline,link_attrs,test_attrs,codegen_attrs,
//! macro_attrs,crate_level,proc_macro_attrs,repr}.rs`, and the texts of `diagnostics.rs` and
//! `session_diagnostics.rs` (MIT or Apache-2.0).

use blaze_hash::HashMap;
use hir::InFile;
use smallvec::SmallVec;
use stdx::edit_distance::find_best_match_for_name_with_substrings;
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange,
    view::{self as ast, AstNode},
};

use super::{
    Args, Attr, AttrKind, Owner, Target, lint_at, parsed,
    table::{self, Allowed, Builtin, Kind, OnDuplicate, Policy, Safety, Targets},
};
use crate::{
    cx::{Cx, item_range_view},
    lints::{
        registry::LintId,
        syntax::{Emitted, Walk, from_expansion, source_fact},
    },
    model::{Applicability, Diag, SuggestionStyle},
};
use stdx::list::Join;

const PREVIOUSLY_ACCEPTED: &str = "this was previously accepted by the compiler but is being phased out; it will become a hard error in a future release!";

/// What the crate says that the attribute parser reads: its root file and its features.
pub(crate) struct Krate {
    root: Option<hir_expand::EditionedFileId>,
    root_name: String,
    fn_delegation: bool,
    stmt_expr_attributes: bool,
    extern_types: bool,
    pub(crate) fn_align: bool,
    pub(crate) static_align: bool,
}

impl Krate {
    pub(crate) fn of(cx: &Cx<'_, '_>, owners: &[Owner<'_>]) -> Krate {
        let root = cx.krate.root_module(cx.db).as_source_file_id(cx.db);
        let root_name = root.map(|it| cx.file_name(it.file_id(cx.db))).unwrap_or_default();
        let mut krate = Krate {
            root,
            root_name,
            fn_delegation: false,
            stmt_expr_attributes: false,
            extern_types: false,
            fn_align: false,
            static_align: false,
        };
        let crate_attrs = owners.iter().filter(|it| it.target == Target::Crate).flat_map(|it| &it.attrs);
        for attr in crate_attrs {
            let (true, Args::List(tt)) = (attr.is("feature"), attr.args()) else { continue };
            for item in items(tt) {
                match word(&item) {
                    Some("fn_delegation") => krate.fn_delegation = true,
                    Some("stmt_expr_attributes") => krate.stmt_expr_attributes = true,
                    Some("extern_types") => krate.extern_types = true,
                    Some("fn_align") => krate.fn_align = true,
                    Some("static_align") => krate.static_align = true,
                    _ => {}
                }
            }
        }
        krate
    }
}

/// What the parsers of one node have taken so far.
#[derive(Default)]
struct Seen {
    /// The attribute each single parser uses, by its path.
    single: HashMap<&'static [&'static str], InFile<TextRange>>,
    /// `#[used(compiler)]`, `#[used(linker)]` and `#[used]`.
    used: [Option<InFile<TextRange>>; 3],
    macro_use: MacroUse,
}

/// `MacroUseParser`'s state: whether a `#[macro_use]` without arguments was seen, the first
/// one, and those with arguments.
#[derive(Default)]
struct MacroUse {
    all: bool,
    first: Option<InFile<TextRange>>,
    specific: Vec<InFile<TextRange>>,
}

/// The lints attribute parsing gives of `owners`' builtin attributes.
pub(crate) fn lints(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner<'_>]) -> Vec<Emitted> {
    let krate = Krate::of(cx, owners);
    // alloc: empty, allocates nothing until a lint
    let mut emitted = Vec::new();
    // alloc: as above; then one buffer for the crate
    let mut found = Vec::new();
    for owner in owners {
        let on_call = owner.target == Target::MacroCall;
        let mut seen = Seen::default();
        for (i, attr) in owner.attrs.iter().enumerate() {
            let AttrKind::Meta { .. } = &attr.kind else { continue };
            // A macro call's attributes are parsed one at a time, so none is a second one.
            if on_call {
                seen = Seen::default();
            }
            // alloc: an inline `SmallVec`, nothing on the heap for a path of up to two segments
            let segments: smallvec::SmallVec<[&str; 2]> = attr.segments().collect();
            match table::builtin(&segments) {
                Some(builtin) => {
                    safety(cx, attr, builtin, &mut found);
                    if parses(attr, builtin) {
                        accept(cx, &krate, owner, attr, builtin, &mut seen, &mut found);
                        match builtin.targets {
                            Targets::ManuallyChecked if attr.is("repr") => {
                                repr_targets(cx, &krate, owner, attr, &mut found)
                            }
                            Targets::ManuallyChecked => {}
                            targets => check_target(cx, &krate, owner, attr, targets, "", &mut found),
                        }
                    }
                }
                None => {
                    // `check_attributes` skips an attribute a doc string follows.
                    if on_call
                        && !owner.attrs.get(i + 1).is_some_and(is_doc_str)
                        && let [name] = segments.as_slice()
                        && !matches!(*name, "cfg" | "cfg_attr")
                        && table::is_builtin_attr_name(name)
                    {
                        found.push(unused_builtin(cx, owner, attr, name));
                    }
                }
            }
            for mut diag in found.drain(..) {
                source_fact(cx, &mut diag, lint_at(owner));
                emitted.push(parsed(diag, owner));
            }
        }
    }
    emitted
}

/// Whether rustc's `ArgParser::from_attr_args` parses the attribute's arguments; when it does
/// not, it gives an error (`diagnostics-items`') and the attribute is not parsed further. A
/// `= value` must be an unsuffixed literal; a list must be in parentheses and of meta items, but
/// for `diagnostic::` attributes and `rustc_dummy`, whose lists are always taken.
fn parses(attr: &Attr<'_>, builtin: &Builtin) -> bool {
    match attr.args() {
        Args::Word => true,
        Args::NameValue(value) => is_unsuffixed_literal(value),
        Args::List(_) if matches!(builtin.path, ["diagnostic", ..] | ["rustc_dummy"]) => true,
        Args::List(tt) => {
            tt.left_delimiter_token().is_some_and(|it| it.kind() == T!['('])
                && items(tt).iter().all(|item| is_meta_item(item))
        }
    }
}

/// rustc's `expr_to_lit` accepting `value`: a literal without a suffix.
fn is_unsuffixed_literal(value: Option<ast::Expr<'_>>) -> bool {
    let Some(ast::Expr::Literal(literal)) = value else { return false };
    let token = literal.token();
    let text = token.text();
    match literal.kind() {
        ast::LiteralKind::IntNumber(it) => it.suffix().is_none(),
        ast::LiteralKind::FloatNumber(it) => it.suffix().is_none(),
        ast::LiteralKind::String(_) | ast::LiteralKind::ByteString(_) | ast::LiteralKind::CString(_) => {
            text.ends_with('"') || text.ends_with('#')
        }
        ast::LiteralKind::Char(_) | ast::LiteralKind::Byte(_) => text.ends_with('\''),
        ast::LiteralKind::Bool(_) => true,
    }
}

/// Whether rustc's meta item list parser takes `item`: a literal, or a path with nothing after
/// it, a parenthesized list or `= value`.
fn is_meta_item(item: &[ast::Element<'_>]) -> bool {
    let is_path_token = |it: &ast::Element<'_>| {
        matches!(it, NodeOrToken::Token(it) if matches!(it.kind(), SyntaxKind::IDENT | T![::] | T![self] | T![super] | T![crate] | T![Self]) || it.kind().is_keyword(span::Edition::CURRENT))
    };
    match item {
        [] => false,
        [NodeOrToken::Token(token)] if token.kind().is_literal() || matches!(token.kind(), T![true] | T![false]) => true,
        _ => {
            let path = item.iter().take_while(|it| is_path_token(it)).count();
            if path == 0 {
                return false;
            }
            match &item[path..] {
                [] => true,
                [NodeOrToken::Node(list)] => {
                    list.kind() == SyntaxKind::TOKEN_TREE
                        && list.first_child_or_token().and_then(NodeOrToken::into_token).is_some_and(|it| it.kind() == T!['('])
                }
                [NodeOrToken::Token(eq), value @ ..] => eq.kind() == T![=] && !value.is_empty(),
                _ => false,
            }
        }
    }
}

/// rustc's `doc_str`: a doc comment or `#[doc = "..."]`.
fn is_doc_str(attr: &Attr<'_>) -> bool {
    attr.is_doc_comment()
        || attr.is("doc") && matches!(attr.args(), Args::NameValue(value) if with_string(value, |_| ()).is_some())
}

/// rustc's `check_attribute_safety`: an attribute that must be `unsafe(..)` and is not,
/// before the edition it must be, and `unsafe_code` of every unsafe attribute.
fn safety(cx: &Cx<'_, '_>, attr: &Attr<'_>, builtin: &Builtin, found: &mut Vec<Diag>) {
    let Safety::Unsafe { note, unsafe_since } = builtin.safety else { return };
    let outside = cx.lint_may_fire(LintId::named("unsafe_attr_outside_unsafe"));
    let code = cx.lint_may_fire(LintId::named("unsafe_code"));
    if !outside && !code {
        return;
    }
    let inner = cx.span(attr.in_file(attr.inner_range()));
    if outside && !attr.is_unsafe() {
        let edition = attr.file.edition(cx.db);
        let is_error = unsafe_since.is_none_or(|since| edition >= since);
        if !is_error {
            let path = cx.span(attr.in_file(attr.path_range()));
            // clone: the diagnostic's span
            let mut diag = Diag::lint("unsafe_attr_outside_unsafe", "unsafe attribute used without unsafe", path.clone());
            diag.span_label(path, "usage of unsafe attribute");
            if !hides_wrapping(cx, attr) {
                diag.multipart_suggestion(
                    "wrap the attribute in `unsafe(...)`",
                    // alloc: the diagnostic's text
                    vec![(inner.shrink_to_lo(), "unsafe(".to_owned()), (inner.shrink_to_hi(), ")".to_owned())],
                    Applicability::MachineApplicable,
                );
            }
            found.push(diag);
        }
    }
    if !code {
        return;
    }
    // alloc: the diagnostic's text
    let mut diag = Diag::lint("unsafe_code", format!("usage of the unsafe `{}` attribute", attr.name()), inner);
    diag.note(note);
    found.push(diag);
}

/// Whether rustc leaves out the suggestion to wrap the attribute in `unsafe(..)`: it is from an
/// expansion and its text is a macro call or a whole attribute (`safety.rs`'s
/// `not_from_proc_macro`).
fn hides_wrapping(cx: &Cx<'_, '_>, attr: &Attr<'_>) -> bool {
    let AttrKind::Meta { written, .. } = attr.kind else { return false };
    if !from_expansion(cx, InFile::new(attr.file, written.syntax())) {
        return false;
    }
    let span = cx.span(attr.in_file(attr.inner_range()));
    let Some(snippet) = cx.snippet(&span) else { return false };
    // alloc: once per `unsafe_attr_outside_unsafe` emitted, its suggestion's test
    let snippet: String = snippet.chars().filter(|it| !it.is_whitespace()).collect();
    snippet.contains("!(") || snippet.starts_with("#[") && snippet.ends_with(']')
}

/// What an attribute's parser says of its input and of a second one.
fn accept(
    cx: &Cx<'_, '_>,
    krate: &Krate,
    owner: &Owner<'_>,
    attr: &Attr<'_>,
    builtin: &'static Builtin,
    seen: &mut Seen,
    found: &mut Vec<Diag>,
) {
    let this = attr.in_file(attr.range());
    let args = attr.args();
    let path = builtin.path;
    match (path, args) {
        (["inline"], Args::NameValue(_)) => found.push(ill_formed(cx, attr, builtin, None)),
        (["link"], Args::NameValue(value)) if with_string(value, |it| it == "dl") == Some(true) => {
            found.push(ill_formed(cx, attr, builtin, None))
        }
        (["ignore"], Args::NameValue(value)) if with_string(value, |_| ()).is_none() => {
            found.push(ill_formed(cx, attr, builtin, None))
        }
        (["ignore"], Args::List(tt)) => {
            let items = items(tt);
            let help = match items.as_slice() {
                [item] => path_of(item),
                _ => None,
            };
            found.push(ill_formed(cx, attr, builtin, help));
        }
        (["macro_use"], _) => macro_use(cx, attr, builtin, &args, &mut seen.macro_use, found),
        (["used"], _) => used(cx, attr, &args, &mut seen.used, found),
        (["repr"] | ["target_feature"] | ["force_target_feature"] | ["feature"] | ["register_tool"], Args::List(tt))
            if items(tt).is_empty() =>
        {
            if attr.is("repr") {
                let targets = Targets::AllowList(&[
                    Policy::Allow(Target::Struct),
                    Policy::Allow(Target::Enum),
                    Policy::Allow(Target::Union),
                    Policy::Warn(Target::MacroCall),
                ]);
                check_target(cx, krate, owner, attr, targets, "()", found);
            }
            found.push(empty_list(cx, attr, builtin, this));
        }
        (["crate_type"], Args::NameValue(value)) if owner.target == Target::Crate => {
            if let Some(diag) = unknown_crate_type(cx, attr, value) {
                found.push(diag);
            }
        }
        (["proc_macro_derive"], Args::List(tt)) => found.extend(ambiguous_helpers(cx, attr, tt)),
        _ => {}
    }
    let Kind::Single(on_duplicate) = builtin.kind else { return };
    if !takes(builtin, attr, &args) {
        return;
    }
    match seen.single.get(path) {
        Some(&used) => match on_duplicate {
            OnDuplicate::Warn => found.push(unused_duplicate(cx, used, this, false)),
            OnDuplicate::WarnButFutureError => found.push(unused_duplicate(cx, used, this, true)),
            OnDuplicate::Error | OnDuplicate::Ignore => {}
        },
        None => {
            seen.single.insert(path, this);
        }
    }
}

/// Whether a single parser converts the attribute, so that a second one is unused: a parser
/// of no arguments always does, the others when the input is of one of their forms, as
/// their conversions check.
fn takes(builtin: &Builtin, attr: &Attr<'_>, args: &Args<'_>) -> bool {
    let template = &builtin.template;
    let no_args = template.word && template.list.is_none() && template.one_of.is_empty() && template.name_value_str.is_none();
    if no_args {
        return true;
    }
    match args {
        Args::Word => template.word,
        Args::List(tt) => {
            let items = items(*tt);
            template.list.is_some()
                || matches!(items.as_slice(), [item] if word(item).is_some_and(|it| template.one_of.contains(&it)))
        }
        Args::NameValue(value) => {
            template.name_value_str.is_some() && (with_string(*value, |_| ()).is_some() || attr.is_doc_comment())
        }
    }
}

/// `MacroUseParser::ATTRIBUTES`: a second `#[macro_use]`, a `#[macro_use(..)]` with one without
/// arguments, an empty list.
fn macro_use(cx: &Cx<'_, '_>, attr: &Attr<'_>, builtin: &Builtin, args: &Args<'_>, state: &mut MacroUse, found: &mut Vec<Diag>) {
    let span = attr.in_file(attr.range());
    let first = *state.first.get_or_insert(span);
    match args {
        Args::Word if state.all => found.push(unused_duplicate(cx, first, span, false)),
        Args::Word => {
            state.all = true;
            state.first = Some(span);
            for specific in state.specific.drain(..) {
                found.push(unused_duplicate(cx, span, specific, false));
            }
        }
        Args::List(tt) if items(*tt).is_empty() => {
            found.push(empty_list(cx, attr, builtin, attr.in_file(tt.syntax().text_range())));
        }
        Args::List(_) if state.all => found.push(unused_duplicate(cx, first, span, false)),
        Args::List(_) => state.specific.push(span),
        Args::NameValue(_) => {}
    }
}

/// `UsedParser::ATTRIBUTES`: one `#[used(compiler)]`, and one of `#[used(linker)]` and `#[used]`.
fn used(cx: &Cx<'_, '_>, attr: &Attr<'_>, args: &Args<'_>, used: &mut [Option<InFile<TextRange>>; 3], found: &mut Vec<Diag>) {
    const COMPILER: usize = 0;
    const LINKER: usize = 1;
    const DEFAULT: usize = 2;
    let by = match args {
        Args::Word => DEFAULT,
        Args::List(tt) => match items(*tt).as_slice() {
            [item] => match word(item) {
                Some("compiler") => COMPILER,
                Some("linker") => LINKER,
                _ => return,
            },
            _ => return,
        },
        Args::NameValue(_) => return,
    };
    let span = attr.in_file(attr.range());
    let other = match by {
        LINKER => used[DEFAULT],
        DEFAULT => used[LINKER],
        _ => None,
    };
    if let Some(prev) = other {
        found.push(unused_duplicate(cx, prev, span, false));
        return;
    }
    match used[by] {
        Some(prev) => found.push(unused_duplicate(cx, prev, span, false)),
        None => used[by] = Some(span),
    }
}

/// `invalid `crate_type` value`, with the stable crate type nearest it.
fn unknown_crate_type(cx: &Cx<'_, '_>, attr: &Attr<'_>, value: Option<ast::Expr<'_>>) -> Option<Diag> {
    const KNOWN: &[&str] = &["bin", "dylib", "staticlib", "cdylib", "rlib", "lib", "proc-macro", "sdylib"];
    const STABLE: &[&str] = &["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro", "bin"];
    let value_node = value?;
    let text = string(Some(value_node))?;
    if KNOWN.contains(&&*text) {
        return None;
    }
    let span = cx.span(attr.in_file(value_node.syntax().text_range()));
    // clone: the diagnostic's span
    let mut diag = Diag::lint("unknown_crate_types", "invalid `crate_type` value", span.clone());
    if let Some(candidate) = find_best_match_for_name_with_substrings(STABLE, &text, Some(5)).copied() {
        // alloc: the diagnostic's text
        diag.suggestion(span, "did you mean", format!("\"{candidate}\""), Applicability::MaybeIncorrect);
    }
    Some(diag)
}

/// `ambiguous_derive_helpers`: a helper of `#[proc_macro_derive(Trait, attributes(..))]` named
/// as a builtin attribute.
fn ambiguous_helpers(cx: &Cx<'_, '_>, attr: &Attr<'_>, tt: ast::TokenTree<'_>) -> Vec<Diag> {
    let items = items(tt);
    // alloc: empty, allocates nothing, as each `Vec::new()` returned below
    let [trait_name, helpers] = items.as_slice() else { return Vec::new() };
    if word(trait_name).is_none() {
        // alloc: as above
        return Vec::new();
    }
    // alloc: as above
    let [NodeOrToken::Token(name), NodeOrToken::Node(list)] = helpers.as_slice() else { return Vec::new() };
    // alloc: as above
    let Some(list) = ast::TokenTree::cast(*list) else { return Vec::new() };
    if name.text() != "attributes" {
        // alloc: as above
        return Vec::new();
    }
    // alloc: as above, until a lint
    let mut found = Vec::new();
    for helper in self::items(list) {
        let [NodeOrToken::Token(ident)] = helper.as_slice() else { return found };
        if ident.kind() != SyntaxKind::IDENT {
            return found;
        }
        if table::is_builtin_attr_name(ident.text()) {
            let span = cx.span(attr.in_file(ident.text_range()));
            found.push(Diag::lint("ambiguous_derive_helpers", "there exists a built-in attribute with the same name", span));
        }
    }
    found
}

/// The lints of `repr`'s targets: each hint has its own (`parse_repr`'s `check_target`).
fn repr_targets(cx: &Cx<'_, '_>, krate: &Krate, owner: &Owner<'_>, attr: &Attr<'_>, found: &mut Vec<Diag>) {
    use Policy::{Allow, Warn};
    const DATA: &[Policy] =
        &[Allow(Target::Struct), Allow(Target::Enum), Allow(Target::Union), Warn(Target::MacroCall)];
    const PACKED: &[Policy] = &[Allow(Target::Struct), Allow(Target::Union), Warn(Target::MacroCall)];
    const SIMD: &[Policy] = &[Allow(Target::Struct)];
    const INT: &[Policy] = &[Allow(Target::Enum), Warn(Target::MacroCall)];
    let Args::List(tt) = attr.args() else { return };
    for item in items(tt) {
        let Some(NodeOrToken::Token(hint)) = item.first() else { continue };
        let (args, targets): (&str, &[Policy]) = match hint.text() {
            "align" => ("(align(...))", DATA),
            "packed" => ("(packed)", PACKED),
            "Rust" => ("(Rust)", DATA),
            "C" => ("(C)", DATA),
            "transparent" => ("(transparent)", DATA),
            "simd" => ("(simd)", SIMD),
            "i8" => ("(i8)", INT),
            "u8" => ("(u8)", INT),
            "i16" => ("(i16)", INT),
            "u16" => ("(u16)", INT),
            "i32" => ("(i32)", INT),
            "u32" => ("(u32)", INT),
            "i64" => ("(i64)", INT),
            "u64" => ("(u64)", INT),
            "i128" => ("(i128)", INT),
            "u128" => ("(u128)", INT),
            "isize" => ("(isize)", INT),
            "usize" => ("(usize)", INT),
            _ => continue,
        };
        check_target(cx, krate, owner, attr, Targets::AllowList(targets), args, found);
    }
}

/// rustc's `check_target` of an attribute with arguments `args` (empty for most: those
/// whose target does not depend on them).
fn check_target(cx: &Cx<'_, '_>, krate: &Krate, owner: &Owner<'_>, attr: &Attr<'_>, targets: Targets, args: &str, found: &mut Vec<Diag>) {
    if let Some(warn) = targets.crate_level() {
        if warn && owner.target != Target::Crate {
            found.push(crate_level(cx, krate, owner, attr));
        }
        return;
    }
    if targets.is_allowed(owner.target) != Allowed::Warn {
        return;
    }
    let path = attr.path();
    let is_diagnostic = path.first().is_some_and(|it| it.as_str() == "diagnostic");
    let lint = if attr.is("deprecated")
        && !matches!(
            owner.target,
            Target::Closure | Target::Expression | Target::Statement | Target::Arm | Target::MacroCall
        ) {
        return;
    } else if is_diagnostic {
        "misplaced_diagnostic_attributes"
    } else {
        "unused_attributes"
    };
    let name = attr.name();
    let range = if args.is_empty() { attr.path_range() } else { attr.inner_range() };
    // alloc: the diagnostic's text, as is all below
    let message = format!("the `{name}{args}` attribute cannot be used on {}", owner.target.plural_name());
    let mut diag = Diag::lint(lint, message, cx.span(attr.in_file(range)));
    diag.suggestion_with_style(
        // alloc: as above
        vec![(cx.span(attr.in_file(attr.range())), String::new())],
        "remove the attribute",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    // alloc: as above
    let (applied, only) = applied(krate, targets.allowed().collect(), owner.target);
    let only = if only { "only " } else { "" };
    // alloc: as above
    diag.help(format!("the `{name}{args}` attribute can {only}be applied to {}", stdx::list::list(applied.iter(), Join::And, false)));
    match (path.as_slice(), owner.target) {
        ([it], Target::Static) if it.as_str() == "link_name" => {
            let inner = cx.span(attr.in_file(attr.inner_range()));
            // alloc: as above
            let mut parts = Vec::new();
            if !attr.is_unsafe() {
                // alloc: as above
                parts.push((inner.shrink_to_lo(), "unsafe(".to_owned()));
            }
            // alloc: as above
            parts.push((cx.span(attr.in_file(attr.path_range())), "export_name".to_owned()));
            if !attr.is_unsafe() {
                // alloc: as above
                parts.push((inner.shrink_to_hi(), ")".to_owned()));
            }
            diag.multipart_suggestion("did you mean to use `#[export_name]`?", parts, Applicability::MaybeIncorrect);
        }
        ([it], Target::Fn | Target::Method(_)) if it.as_str() == "repr" && args == "(align(...))" && krate.fn_align => {
            diag.help("use `#[rustc_align(...)]` instead");
        }
        ([it], Target::Static) if it.as_str() == "repr" && args == "(align(...))" && krate.static_align => {
            diag.help("use `#[rustc_align_static(...)]` instead");
        }
        _ => {}
    }
    if !is_diagnostic {
        diag.warn(PREVIOUSLY_ACCEPTED);
    }
    if owner.target == Target::MacroCall {
        diag.note(
            "placing this attribute on a macro invocation does nothing even if the macro expands to what would be a valid target for the attribute",
        );
    }
    found.push(diag);
}

/// rustc's `check_crate_level` where being elsewhere is a lint: `InvalidAttrStyle`.
pub(crate) fn crate_level(cx: &Cx<'_, '_>, krate: &Krate, owner: &Owner<'_>, attr: &Attr<'_>) -> Diag {
    let name = attr.name();
    let message = match attr.inner {
        // alloc: the diagnostic's text, as is all of `crate_level`'s
        true => format!("the `#![{name}]` attribute can only be used at the crate root"),
        // alloc: as above
        false => format!("crate-level attribute should be an inner attribute: add an exclamation mark: `#![{name}]`"),
    };
    let mut diag = Diag::lint("unused_attributes", message, cx.span(attr.in_file(attr.range())));
    if !attr.inner {
        let target = cx.span(InFile::new(owner.node.file_id, target_range(owner.node.value)));
        diag.span_note(
            target,
            // alloc: as above
            format!("this attribute does not have an `!`, which means it is applied to this {}", owner.target.name()),
        );
    }
    if attr.inner && krate.root.is_some_and(|root| attr.file.original_file(cx.db) != root) {
        // alloc: as above
        diag.help(format!("the crate root is at `{}`", krate.root_name));
    }
    diag
}

/// rustc's span of what an attribute is applied to: the node without its attributes; a
/// macro call's ends at its arguments (`MacCall::span`), without a `;`.
fn target_range(node: ast::Node<'_>) -> TextRange {
    let range = item_range_view(node);
    match ast::MacroCall::cast(node).and_then(|it| it.token_tree()) {
        Some(args) => TextRange::new(range.start(), args.syntax().text_range().end()),
        None => range,
    }
}

/// `UnusedBuiltinAttribute`: a builtin attribute no parser takes (a lint attribute) on a macro
/// call (`rustc_expand`'s `check_attributes`).
fn unused_builtin(cx: &Cx<'_, '_>, owner: &Owner<'_>, attr: &Attr<'_>, name: &str) -> Diag {
    let at = cx.span(attr.in_file(attr.range()));
    // alloc: the diagnostic's text and span, as is all of `unused_builtin`'s
    let mut diag = Diag::lint("unused_attributes", format!("unused attribute `{name}`"), at.clone());
    if let Some(path) = ast::MacroCall::cast(owner.node.value).and_then(|it| it.path()) {
        // alloc: as above
        let text: String = path.syntax().text().chars().filter(|it| !it.is_whitespace()).collect();
        diag.span_note(
            cx.span(InFile::new(owner.node.file_id, path.syntax().text_range())),
            // alloc: as above
            format!("the built-in attribute `{name}` will be ignored, since it's applied to the macro invocation `{text}`"),
        );
    }
    diag.suggestion_with_style(
        // alloc: as above
        vec![(at, String::new())],
        "remove the attribute",
        Applicability::MachineApplicable,
        SuggestionStyle::CompletelyHidden,
    );
    diag
}

/// rustc's `allowed_targets_applied`: the targets an attribute can be applied to, those of
/// unstable features the crate has not enabled left out, similar ones named as a group, sorted;
/// with whether there is one.
pub(crate) fn applied(krate: &Krate, mut allowed: Vec<Target>, target: Target) -> (Vec<&'static str>, bool) {
    use super::target::MethodKind::{Inherent, Trait, TraitImpl};
    if !krate.fn_delegation {
        allowed.retain(|it| !matches!(it, Target::Delegation { .. }));
    }
    if !krate.stmt_expr_attributes {
        allowed.retain(|it| !matches!(it, Target::Expression | Target::Statement));
    }
    if !krate.extern_types {
        allowed.retain(|it| !matches!(it, Target::ForeignTy));
    }
    const FUNCTION_LIKE: &[Target] = &[
        Target::Fn,
        Target::Closure,
        Target::ForeignFn,
        Target::Method(Inherent),
        Target::Method(Trait { body: false }),
        Target::Method(Trait { body: true }),
        Target::Method(TraitImpl),
    ];
    const FUNCTION_WITH_BODY_LIKE: &[Target] = &[
        Target::Fn,
        Target::Closure,
        Target::Method(Inherent),
        Target::Method(Trait { body: true }),
        Target::Method(TraitImpl),
    ];
    const METHOD_LIKE: &[Target] = &[
        Target::Method(Inherent),
        Target::Method(Trait { body: false }),
        Target::Method(Trait { body: true }),
        Target::Method(TraitImpl),
    ];
    const IMPL_LIKE: &[Target] = &[Target::Impl { of_trait: false }, Target::Impl { of_trait: true }];
    const ADT_LIKE: &[Target] = &[Target::Struct, Target::Enum, Target::Union];
    // alloc: a diagnostic's help, as is all of `applied`
    let mut groups = Vec::new();
    for (group, name) in [
        (FUNCTION_LIKE, "functions"),
        (FUNCTION_WITH_BODY_LIKE, "functions with a body"),
        (METHOD_LIKE, "methods"),
        (IMPL_LIKE, "impl blocks"),
        (ADT_LIKE, "data types"),
    ] {
        if group.contains(&target) || allowed.iter().filter(|it| group.contains(it)).count() < 2 {
            continue;
        }
        allowed.retain(|it| !group.contains(it));
        groups.push(name);
    }
    // alloc: as above
    let mut names: Vec<&'static str> = groups.into_iter().chain(allowed.iter().map(|it| it.plural_name())).collect();
    names.sort_unstable();
    names.dedup();
    let only = names.len() == 1;
    (names, only)
}

/// `UnusedDuplicate`: `unused` is a second attribute where `used` is used.
fn unused_duplicate(cx: &Cx<'_, '_>, used: InFile<TextRange>, unused: InFile<TextRange>, warning: bool) -> Diag {
    let this = cx.span(unused);
    // clone: the diagnostic's span
    let mut diag = Diag::lint("unused_attributes", "unused attribute", this.clone());
    diag.suggestion(this, "remove this attribute", "", Applicability::MachineApplicable);
    diag.span_note(cx.span(used), "attribute also specified here");
    if warning {
        diag.warn(PREVIOUSLY_ACCEPTED);
    }
    diag
}

/// `EmptyAttributeList` at `span`: the attribute, or for one valid without a list, the list.
fn empty_list(cx: &Cx<'_, '_>, attr: &Attr<'_>, builtin: &Builtin, span: InFile<TextRange>) -> Diag {
    let span = cx.span(span);
    let name = attr.name();
    // clone: the diagnostic's span
    let mut diag = Diag::lint("unused_attributes", "unused attribute", span.clone());
    let (note, remove) = match builtin.template.word {
        // alloc: the diagnostic's text
        true => (format!("using `{name}` with an empty list is equivalent to not using a list at all"), "remove these parentheses"),
        // alloc: the diagnostic's text
        false => (format!("using `{name}` with an empty list has no effect"), "remove this attribute"),
    };
    diag.note(note);
    diag.suggestion(span, remove, "", Applicability::MachineApplicable);
    diag
}

/// `IllFormedAttributeInput` of `ill_formed_attribute_input`, at what is between `#[` and `]`.
fn ill_formed(cx: &Cx<'_, '_>, attr: &Attr<'_>, builtin: &Builtin, help: Option<String>) -> Diag {
    let suggestions: Vec<String> =
        // alloc: the diagnostic's text, as is all of `ill_formed`'s
        builtin.template.suggestions(&attr.name(), attr.is_unsafe()).into_iter().map(|it| format!("`{it}`")).collect();
    let list = stdx::list::list(suggestions.iter(), Join::And, false);
    let message = match suggestions.len() {
        // alloc: as above
        1 => format!("attribute must be of the form {list}"),
        // alloc: as above
        _ => format!("valid forms for the attribute are {list}"),
    };
    let mut diag = Diag::lint("ill_formed_attribute_input", message, cx.span(attr.in_file(attr.inner_range())));
    if let Some(lint) = help {
        // alloc: as above
        diag.help(format!("if you meant to silence a warning, consider using #![allow({lint})] or #![expect({lint})]"));
    }
    diag
}

/// One item of an attribute's list: its elements, inline up to three.
pub(crate) type Item<'t> = SmallVec<[ast::Element<'t>; 3]>;

/// The items of an attribute's list, inline up to four.
pub(crate) type Items<'t> = SmallVec<[Item<'t>; 4]>;

/// The items of an attribute's list, between its commas, each without trivia.
pub(crate) fn items<'t>(tt: ast::TokenTree<'t>) -> Items<'t> {
    let mut items = Items::new();
    let mut item = Item::new();
    // Between the delimiters: the first child and the last are left out.
    let mut children = tt.syntax().children_with_tokens().skip(1).peekable();
    while let Some(child) = children.next() {
        if children.peek().is_none() {
            break;
        }
        let kind = child.kind();
        if kind == T![,] {
            if !item.is_empty() {
                items.push(std::mem::take(&mut item));
            }
        } else if !kind.is_trivia() {
            item.push(child);
        }
    }
    if !item.is_empty() {
        items.push(item);
    }
    items
}

/// The item when it is one word.
pub(crate) fn word<'t>(item: &[ast::Element<'t>]) -> Option<&'t str> {
    match item {
        [NodeOrToken::Token(token)] if token.kind() == SyntaxKind::IDENT || token.kind().is_keyword(span::Edition::CURRENT) => {
            Some(token.text())
        }
        _ => None,
    }
}

/// The item when it is a path with no arguments: `foo`, `clippy::bar`.
fn path_of(item: &[ast::Element<'_>]) -> Option<String> {
    // alloc: the help of an ill-formed `#[ignore(..)]`
    let mut path = String::new();
    for element in item {
        let NodeOrToken::Token(token) = element else { return None };
        match token.kind() {
            SyntaxKind::IDENT | T![::] | T![:] => path.push_str(token.text()),
            _ => return None,
        }
    }
    (!path.is_empty()).then_some(path)
}

/// The value of a string literal.
pub(crate) fn string(value: Option<ast::Expr<'_>>) -> Option<String> {
    // alloc: a `crate_type`'s value, once per crate
    with_string(value, str::to_owned)
}

/// `f` of the value of a string literal, read in place.
fn with_string<R>(value: Option<ast::Expr<'_>>, f: impl FnOnce(&str) -> R) -> Option<R> {
    let ast::Expr::Literal(literal) = value? else { return None };
    match literal.kind() {
        ast::LiteralKind::String(it) => it.value().ok().map(|it| f(&it)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{super::target::MethodKind, *};

    #[test]
    fn a_list_is_joined_with_and() {
        assert_eq!(stdx::list::list(["`a`"], stdx::list::Join::And, false), "`a`");
        assert_eq!(stdx::list::list(["`a`", "`b`"], stdx::list::Join::And, false), "`a` and `b`");
        assert_eq!(stdx::list::list(["`a`", "`b`", "`c`"], stdx::list::Join::And, false), "`a`, `b`, and `c`");
    }

    #[test]
    fn similar_targets_are_named_as_a_group() {
        let krate = Krate {
            root: None,
            root_name: String::new(),
            fn_delegation: false,
            stmt_expr_attributes: false,
            extern_types: false,
            fn_align: false,
            static_align: false,
        };
        let allowed = vec![
            Target::Fn,
            Target::Method(MethodKind::Inherent),
            Target::Method(MethodKind::TraitImpl),
            Target::Static,
            Target::Expression,
        ];
        let (names, only) = applied(&krate, allowed, Target::Struct);
        // The methods are functions: the first group takes them, so none is left for "methods".
        assert_eq!(names, ["functions", "statics"]);
        assert!(!only);
    }

    #[test]
    fn the_builtin_table_is_sorted_and_finds_paths() {
        assert!(table::builtin(&["inline"]).is_some());
        assert!(table::builtin(&["diagnostic", "on_unimplemented"]).is_some());
        assert!(table::builtin(&["no_such_attribute"]).is_none());
        assert!(table::is_builtin_attr_name("must_use"));
        assert!(!table::is_builtin_attr_name("serde"));
        let inline = table::builtin(&["inline"]).unwrap();
        assert_eq!(inline.template.suggestions("inline", false), ["inline", "inline(always)", "inline(never)"]);
    }

    /// A list's items are what is between its commas, without trivia, the empty ones left out.
    #[test]
    fn items_are_the_lists_items() {
        let parse = syntax::SourceFile::parse("#[a(b, c = \"d\", e(f), , 1,)] fn g() {}", span::Edition::CURRENT);
        let list = parse.root().descendants().find_map(ast::TokenTree::cast).unwrap();
        let texts: Vec<String> = items(list)
            .iter()
            .map(|item| {
                item.iter()
                    .map(|it| match it {
                        NodeOrToken::Node(it) => it.text(),
                        NodeOrToken::Token(it) => it.text(),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(texts, ["b", "c=\"d\"", "e(f)", "1"]);
        assert_eq!(word(&items(list)[0]), Some("b"));
        let empty = syntax::SourceFile::parse("#[a()] fn g() {}", span::Edition::CURRENT);
        let list = empty.root().descendants().find_map(ast::TokenTree::cast).unwrap();
        assert!(blaze_alloc_count::assert_at_most(0, "an empty list's items", || items(list)).is_empty());
    }

    /// Reading the paths and string values of the crate's attributes allocates nothing.
    #[test]
    fn reading_an_attribute_allocates_nothing() {
        let fixture = r#"
//- /main.rs crate:main
#[doc = "x"]
#[diagnostic::on_unimplemented(message = "y")]
trait T {}
#[inline]
#[must_use]
fn f() {}
"#;
        crate::lints::with_cx(fixture, &[], |cx| {
            let owners = super::super::owners(cx, cx.walk());
            let attrs: Vec<&Attr<'_>> = owners.iter().flat_map(|it| &it.attrs).collect();
            let read = blaze_alloc_count::assert_at_most(0, "the paths and values of the attributes", || {
                let inline = attrs.iter().filter(|it| it.is("inline")).count();
                let segments: usize = attrs.iter().map(|it| it.segments().count()).sum();
                let docs = attrs.iter().filter(|it| is_doc_str(it)).count();
                (inline, segments, docs)
            });
            // The segments of `feature(no_core)` and `no_core` too, which a fixture crate without
            // a core takes.
            assert_eq!(read, (1, 7, 1));
        });
    }
}
