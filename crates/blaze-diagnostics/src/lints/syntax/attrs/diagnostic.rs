//! The lints of the `#[diagnostic::..]` attributes (and `#[rustc_on_unimplemented]`,
//! `#[rustc_diagnostic_opaque]`, which share their parsers): what parsing their options gives
//! (`malformed_diagnostic_attributes`, `malformed_diagnostic_format_literals`,
//! `malformed_diagnostic_filters`, `unused_attributes` of a second `opaque`), what
//! `check_mod_attrs` says of the parameters they name and of `on_const` on a const impl, and
//! name resolution's `unknown_diagnostic_attributes`. Where they are not allowed and their
//! duplicates are `parse`'s.
//!
//! - [`meta`]: their arguments as the attribute parser reads them.
//! - [`format`]: the format strings of `message`, `label` and `note`.
//! - [`directive`]: their options.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/diagnostic/{mod,
//! on_unimplemented,on_const,on_move,on_type_error,on_unknown,on_unmatched_args,
//! do_not_recommend,opaque}.rs`, `compiler/rustc_passes/src/check_attr.rs`
//! (`check_diagnostic_on_unimplemented`, `check_diagnostic_on_const`,
//! `check_diagnostic_on_move`, `check_diagnostic_on_type_error`) and
//! `compiler/rustc_resolve/src/macros.rs` (`DIAGNOSTIC_ATTRIBUTES`, `UnknownDiagnosticAttribute`)
//! (MIT or Apache-2.0).

mod directive;
mod format;
mod meta;

use hir::{HirFileId, InFile, PathResolution};
use span::Edition;
use stdx::edit_distance::find_best_match_for_name;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode, HasGenericParams, HasName},
};

use self::directive::Directive;
use super::super::{Emitted, Pass, Walk, source_fact};
use super::{Attr, Owner, Target, checked, lint_at, parsed};
use crate::{
    cx::{Cx, def_range_view},
    lower::Phase,
    model::{Applicability, Diag, Span},
};

/// Which attribute a directive is of: rustc's `Mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    RustcOnUnimplemented,
    DiagnosticOnUnimplemented,
    DiagnosticOnConst,
    DiagnosticOnMove,
    DiagnosticOnUnknown,
    DiagnosticOnUnmatchedArgs,
    DiagnosticOnTypeError,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::RustcOnUnimplemented => "rustc_on_unimplemented",
            Mode::DiagnosticOnUnimplemented => "diagnostic::on_unimplemented",
            Mode::DiagnosticOnConst => "diagnostic::on_const",
            Mode::DiagnosticOnMove => "diagnostic::on_move",
            Mode::DiagnosticOnUnknown => "diagnostic::on_unknown",
            Mode::DiagnosticOnUnmatchedArgs => "diagnostic::on_unmatched_args",
            Mode::DiagnosticOnTypeError => "diagnostic::on_type_error",
        }
    }

    fn expected_options(self) -> &'static str {
        match self {
            Mode::RustcOnUnimplemented => RUSTC_DEV_GUIDE,
            Mode::DiagnosticOnTypeError => "at least a single `note` option is expected",
            _ => "at least one of the `message`, `note` and `label` options are expected",
        }
    }

    fn allowed_options(self) -> &'static str {
        match self {
            Mode::RustcOnUnimplemented => RUSTC_DEV_GUIDE,
            Mode::DiagnosticOnTypeError => "only `note` is allowed as option for `diagnostic::on_type_error`",
            _ => "only `message`, `note` and `label` are allowed as options",
        }
    }

    fn allowed_format_arguments(self) -> &'static str {
        match self {
            Mode::RustcOnUnimplemented => {
                "see <https://rustc-dev-guide.rust-lang.org/diagnostics.html#rustc_on_unimplemented> for allowed format arguments"
            }
            Mode::DiagnosticOnUnimplemented => "only `Self` and generics of the trait are allowed as a format argument",
            Mode::DiagnosticOnConst => {
                "only `Self` and generics of the implementation are allowed as a format argument"
            }
            Mode::DiagnosticOnMove => "only `This`, `Self` and generics of the type are allowed as a format argument",
            Mode::DiagnosticOnUnknown => "only `This` is allowed as a format argument, referring to the failed import",
            Mode::DiagnosticOnUnmatchedArgs => {
                "only `This` is allowed as a format argument, referring to the macro's name"
            }
            Mode::DiagnosticOnTypeError => "only `note` is allowed as option for `diagnostic::on_type_error`",
        }
    }
}

const RUSTC_DEV_GUIDE: &str = "see <https://rustc-dev-guide.rust-lang.org/diagnostics.html#rustc_on_unimplemented>";

/// The attributes of the `diagnostic` tool rustc knows, each with the feature it needs:
/// rustc_resolve's `DIAGNOSTIC_ATTRIBUTES`.
const KNOWN: [(&str, Option<&str>); 8] = [
    ("on_unimplemented", None),
    ("do_not_recommend", None),
    ("on_move", Some("diagnostic_on_move")),
    ("on_const", Some("diagnostic_on_const")),
    ("on_unknown", Some("diagnostic_on_unknown")),
    ("on_unmatched_args", Some("diagnostic_on_unmatched_args")),
    ("on_type_error", Some("diagnostic_on_type_error")),
    ("opaque", Some("diagnostic_opaque")),
];

/// An attribute with a parser here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Directive(Mode),
    DoNotRecommend,
    Opaque,
}

impl Kind {
    /// The parser of `diagnostic::{name}`, where `KNOWN` has it.
    fn of_known(index: usize) -> Kind {
        match KNOWN[index].0 {
            "on_unimplemented" => Kind::Directive(Mode::DiagnosticOnUnimplemented),
            "do_not_recommend" => Kind::DoNotRecommend,
            "on_move" => Kind::Directive(Mode::DiagnosticOnMove),
            "on_const" => Kind::Directive(Mode::DiagnosticOnConst),
            "on_unknown" => Kind::Directive(Mode::DiagnosticOnUnknown),
            "on_unmatched_args" => Kind::Directive(Mode::DiagnosticOnUnmatchedArgs),
            "on_type_error" => Kind::Directive(Mode::DiagnosticOnTypeError),
            _ => Kind::Opaque,
        }
    }
}

/// Where the lints of one owner's attributes go, and the file of the attribute being parsed.
struct Sink<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    owner: &'a Owner<'db>,
    file: HirFileId,
    out: &'a mut Vec<Emitted>,
}

impl Sink<'_, '_, '_> {
    fn at(&self, range: TextRange) -> InFile<TextRange> {
        InFile::new(self.file, range)
    }

    fn span(&self, range: TextRange) -> Span {
        self.cx.span(self.at(range))
    }

    fn lint(&self, lint: &str, message: impl Into<String>, range: TextRange) -> Diag {
        Diag::lint(lint, message, self.span(range))
    }

    /// A lint parsing the attributes gives.
    fn parsed(&mut self, mut diag: Diag) {
        source_fact(self.cx, &mut diag, lint_at(self.owner));
        self.out.push(parsed(diag, self.owner));
    }

    /// A lint `check_mod_attrs` gives.
    fn checked(&mut self, mut diag: Diag) {
        source_fact(self.cx, &mut diag, self.owner.at());
        self.out.push(checked(diag, self.owner));
    }
}

/// What the parsers of one owner's attributes keep: each one's directive, merged.
#[derive(Default)]
struct State {
    on_unimplemented: Option<Directive>,
    /// Where the last `on_const`'s path is, and its directive.
    on_const: Option<(InFile<TextRange>, Option<Directive>)>,
    on_move: Option<Directive>,
    on_type_error: Option<Directive>,
    on_unknown: Option<Directive>,
    on_unmatched_args: Option<Directive>,
    /// Where the first `opaque` is.
    opaque: Option<InFile<TextRange>>,
}

/// The lints of the diagnostic attributes of `owners`.
pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, _walk: &Walk<'db>, owners: &[Owner<'db>]) -> Vec<Emitted> {
    // alloc: empty, allocates nothing until a lint
    let mut out = Vec::new();
    let mut enabled: Option<[bool; KNOWN.len()]> = None;
    for owner in owners {
        let mut state = State::default();
        let mut any = false;
        for attr in &owner.attrs {
            let Some(path) = attr.meta().and_then(|it| it.path()) else { continue };
            let Some(kind) = classify(cx, &mut enabled, owner, attr, path, &mut out) else { continue };
            any = true;
            let mut sink = Sink { cx, owner, file: attr.file, out: &mut out };
            parse(&mut sink, &mut state, kind, attr, path);
        }
        if any {
            let mut sink = Sink { cx, owner, file: owner.node.file_id, out: &mut out };
            check(&mut sink, state);
        }
    }
    out
}

/// The parser of the attribute at `path`, if it has one here and its feature is enabled; an
/// unknown `diagnostic::..` linted.
fn classify<'db>(
    cx: &Cx<'_, 'db>,
    enabled: &mut Option<[bool; KNOWN.len()]>,
    owner: &Owner<'_>,
    attr: &Attr<'_>,
    path: ast::Path<'db>,
    out: &mut Vec<Emitted>,
) -> Option<Kind> {
    let mut segments = path.segments();
    let first = segments.next()?;
    if first.coloncolon_token().is_some() {
        return None;
    }
    let first_name = first.name_ref()?;
    let second = segments.next();
    match (unraw(first_name.text()), &second) {
        ("rustc_on_unimplemented", None) => return Some(Kind::Directive(Mode::RustcOnUnimplemented)),
        ("rustc_diagnostic_opaque", None) => return Some(Kind::Opaque),
        ("diagnostic", Some(_)) => {}
        _ => return None,
    }
    let second = second?;
    let name_ref = second.name_ref()?;
    let name = unraw(name_ref.text());
    let enabled = *enabled.get_or_insert_with(|| {
        KNOWN.map(|(_, feature)| {
            feature.is_none_or(|it| cx.krate.is_unstable_feature_enabled(cx.db, &intern::Symbol::intern(it)))
        })
    });
    let known = KNOWN.iter().position(|(it, _)| *it == name);
    if !known.is_some_and(|it| enabled[it]) {
        unknown(cx, owner, attr, path, second, &enabled, out);
        return None;
    }
    if segments.next().is_some() {
        return None;
    }
    known.map(Kind::of_known)
}

/// rustc_resolve's `UnknownDiagnosticAttribute` of `path`, `diagnostic::` and `second`.
fn unknown<'db>(
    cx: &Cx<'_, 'db>,
    owner: &Owner<'_>,
    attr: &Attr<'_>,
    path: ast::Path<'db>,
    second: ast::PathSegment<'_>,
    enabled: &[bool; KNOWN.len()],
    out: &mut Vec<Emitted>,
) {
    let Some(name_ref) = second.name_ref() else { return };
    let name = unraw(name_ref.text());
    if !matches!(cx.vsema.resolve_path(path), Some(PathResolution::ToolModule(_))) {
        return;
    }
    let span = cx.span(attr.in_file(name_ref.syntax().text_range()));
    // clone: the diagnostic's span, as is all below
    let mut diag = Diag::lint("unknown_diagnostic_attributes", "unknown diagnostic attribute", span.clone());
    if let Some((_, Some(feature))) = KNOWN.iter().find(|(it, feature)| *it == name && feature.is_some()) {
        // alloc: as above
        diag.help(format!("add `#![feature({feature})]` to the crate attributes to enable"));
    } else {
        // alloc: as above
        let candidates: Vec<&str> = KNOWN.iter().zip(enabled).filter(|(_, on)| **on).map(|((it, _), _)| *it).collect();
        if let Some(typo) = find_best_match_for_name(&candidates, name, None).copied() {
            let message = "an attribute with a similar name exists";
            diag.suggestion_verbose(span, message, typo, Applicability::MachineApplicable);
        }
    }
    source_fact(cx, &mut diag, owner.at());
    out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, owner.at()));
}

/// The parser of `attr`, a `kind` attribute.
fn parse(sink: &mut Sink<'_, '_, '_>, state: &mut State, kind: Kind, attr: &Attr<'_>, path: ast::Path<'_>) {
    let Some(args) = meta::args(attr.args(), path.qualifier().is_some()) else { return };
    let attr_range = attr.range();
    let mode = match kind {
        Kind::Directive(mode) => mode,
        Kind::DoNotRecommend => {
            if !matches!(args, meta::Args::NoArgs) {
                let diag = sink.lint(
                    "malformed_diagnostic_attributes",
                    "`#[diagnostic::do_not_recommend]` does not expect any arguments",
                    attr_range,
                );
                sink.parsed(diag);
            }
            return;
        }
        Kind::Opaque => {
            let at = sink.at(attr_range);
            if let Some(earlier) = state.opaque {
                let span = sink.span(attr_range);
                // clone: the diagnostic's span
                let mut diag = Diag::lint("unused_attributes", "unused attribute", span.clone());
                diag.suggestion(span, "remove this attribute", "", Applicability::MachineApplicable);
                diag.span_note(sink.cx.span(earlier), "attribute also specified here");
                sink.parsed(diag);
            }
            state.opaque = Some(at);
            if !matches!(args, meta::Args::NoArgs) {
                let diag = sink.lint(
                    "malformed_diagnostic_attributes",
                    "`#[diagnostic::opaque]` does not expect any arguments",
                    attr_range,
                );
                sink.parsed(diag);
            }
            return;
        }
    };
    if mode == Mode::DiagnosticOnConst {
        let path_range = sink.at(attr.path_range());
        state.on_const.get_or_insert((path_range, None)).0 = path_range;
    }
    let Some(items) = directive::parse_list(sink, &args, attr_range, mode) else { return };
    let directive = directive::parse_items(sink, mode, items, true);
    let slot = match mode {
        Mode::RustcOnUnimplemented | Mode::DiagnosticOnUnimplemented => &mut state.on_unimplemented,
        Mode::DiagnosticOnConst => match &mut state.on_const {
            Some((_, directive)) => directive,
            None => return,
        },
        Mode::DiagnosticOnMove => &mut state.on_move,
        Mode::DiagnosticOnUnknown => &mut state.on_unknown,
        Mode::DiagnosticOnUnmatchedArgs => &mut state.on_unmatched_args,
        Mode::DiagnosticOnTypeError => &mut state.on_type_error,
    };
    directive::merge(sink, slot, directive);
}

/// `check_mod_attrs`' checks of what the parsers kept.
fn check(sink: &mut Sink<'_, '_, '_>, state: State) {
    let owner = sink.owner;
    let node = owner.node.value;
    if let Some(directive) = &state.on_unimplemented
        && owner.target == Target::Trait
        && let Some(item) = ast::Trait::cast(node)
    {
        let params = generic_params(&item);
        let edition = owner.node.file_id.edition(sink.cx.db);
        let name = item.name().map(|it| printable(it.text(), edition)).unwrap_or_default();
        directive.visit_params(&mut |param, at| {
            if params.iter().any(|it| it == param) {
                return;
            }
            let mut diag = Diag::lint(
                "malformed_diagnostic_format_literals",
                // alloc: the diagnostic's text
                format!("there is no parameter `{}` on trait `{name}`", printable(param, sink.cx.edition)),
                sink.cx.span(at),
            );
            if !directive.is_rustc_attr {
                diag.help("expect either a generic argument name or `{Self}` as format argument");
            }
            sink.checked(diag);
        });
    }
    if let Some((path_range, directive)) = &state.on_const
        && owner.target == (Target::Impl { of_trait: true })
        && let Some(item) = ast::Impl::cast(node)
    {
        if let Some(directive) = directive {
            unknown_params(sink, directive, &generic_params(&item), &[], SELF_HELP);
        }
        if item.const_token().is_some() {
            let mut diag = Diag::lint(
                "misplaced_diagnostic_attributes",
                "the `diagnostic::on_const` attribute can only be applied to non-const trait implementations",
                sink.cx.span(*path_range),
            );
            let item_span = sink.cx.span(InFile::new(owner.node.file_id, def_range_view(node)));
            diag.span_label(item_span, "this is a const trait implementation");
            sink.checked(diag);
        }
    }
    let adt = matches!(owner.target, Target::Struct | Target::Enum | Target::Union)
        .then(|| ast::Adt::cast(node))
        .flatten();
    let Some(adt) = adt else { return };
    if let Some(directive) = &state.on_move {
        unknown_params(sink, directive, &generic_params(&adt), &[], SELF_HELP);
    }
    if let Some(directive) = &state.on_type_error {
        let params = generic_params(&adt);
        if params.len() != 1 {
            let range = match adt.generic_param_list() {
                Some(list) => list.syntax().text_range(),
                None => adt.name().map_or(def_range_view(node), |it| TextRange::empty(it.syntax().text_range().end())),
            };
            let diag = Diag::lint(
                "malformed_diagnostic_attributes",
                // alloc: the diagnostic's text
                format!(
                    "`#[diagnostic::on_type_error]` only supports exactly one ADT generic parameter, but found `{}`",
                    params.len()
                ),
                sink.cx.span(InFile::new(owner.node.file_id, range)),
            );
            sink.checked(diag);
        }
        unknown_params(
            sink,
            directive,
            &params,
            &["Expected", "Found"],
            "expect either a generic argument name, `{This}`, `{Expected}` or `{Found}` as format argument",
        );
    }
}

const SELF_HELP: &str = "expect either a generic argument name or `{Self}` as format argument";

/// rustc's `OnConstMalformedFormatLiterals`, `OnMoveMalformedFormatLiterals` and
/// `OnTypeErrorMalformedFormatLiterals`: each parameter `directive` names that is not one of
/// `params` nor of `allowed`.
fn unknown_params(sink: &mut Sink<'_, '_, '_>, directive: &Directive, params: &[String], allowed: &[&str], help: &str) {
    directive.visit_params(&mut |param, at| {
        if params.iter().any(|it| it == param) || allowed.contains(&param) {
            return;
        }
        // alloc: the diagnostic's text
        let message = format!("unknown parameter `{}`", printable(param, sink.cx.edition));
        let mut diag = Diag::lint("malformed_diagnostic_format_literals", message, sink.cx.span(at));
        diag.help(help);
        sink.checked(diag);
    });
}

/// The names of the type and const parameters of `item`.
fn generic_params<'t>(item: &impl HasGenericParams<'t>) -> Vec<String> {
    // alloc: empty, allocates nothing
    let Some(list) = item.generic_param_list() else { return Vec::new() };
    // alloc: of an item with a diagnostic attribute that names parameters
    list.type_or_const_params().filter_map(|it| Some(unraw(it.name()?.text()).to_owned())).collect()
}

fn unraw(text: &str) -> &str {
    text.strip_prefix("r#").unwrap_or(text)
}

/// An identifier as rustc prints it: raw where it is a keyword of `edition` that can be.
fn printable(text: &str, edition: Edition) -> String {
    let name = unraw(text);
    let can_be_raw = !matches!(name, "_" | "crate" | "self" | "Self" | "super");
    match can_be_raw && SyntaxKind::from_keyword(name, edition).is_some() {
        // alloc: a diagnostic's text
        true => format!("r#{name}"),
        // alloc: as above
        false => name.to_owned(),
    }
}
