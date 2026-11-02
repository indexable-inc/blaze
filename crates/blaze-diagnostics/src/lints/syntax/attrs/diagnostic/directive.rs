//! The options of `#[diagnostic::on_unimplemented]` and the rest that take `message`,
//! `label` and `note`: rustc's `Directive`, and the lints parsing it gives.
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/diagnostic/mod.rs`
//! (`parse_list`, `parse_directive_items`, `merge_directives`, `parse_filter`,
//! `parse_predicate`, `parse_flag`, `parse_name`, `parse_filter_format`),
//! `compiler/rustc_hir/src/attrs/diagnostic.rs` (`Directive::visit_params`) and the texts of
//! `compiler/rustc_attr_parsing/src/diagnostics.rs` (MIT or Apache-2.0).

use hir::InFile;
use syntax::TextRange;

use super::{
    Mode, Sink,
    format::{self, Warning},
    meta::{Args, Item, Lit},
};
use crate::model::Diag;

const MALFORMED: &str = "malformed_diagnostic_attributes";
const FORMAT_LITERALS: &str = "malformed_diagnostic_format_literals";
const FILTERS: &str = "malformed_diagnostic_filters";

/// A format string: the generic parameters it names, each where rustc says it is.
#[derive(Clone, Debug, Default)]
pub(super) struct Format {
    params: Vec<(String, InFile<TextRange>)>,
}

/// rustc's `Directive`, as far as the checks of its parameters see it.
#[derive(Clone, Debug)]
pub(super) struct Directive {
    pub(super) is_rustc_attr: bool,
    /// Each `on(..)`: the parameters its filter names, where it is, and what it gives.
    filters: Vec<(Vec<String>, InFile<TextRange>, Directive)>,
    message: Option<(InFile<TextRange>, Format)>,
    label: Option<(InFile<TextRange>, Format)>,
    notes: Vec<Format>,
    parent_label: Option<(InFile<TextRange>, Format)>,
}

impl Directive {
    /// rustc's `Directive::visit_params`: each parameter it names, in order.
    pub(super) fn visit_params(&self, visit: &mut impl FnMut(&str, InFile<TextRange>)) {
        for (params, at, directive) in &self.filters {
            for param in params {
                visit(param, *at);
            }
            directive.visit_params(visit);
        }
        let formats = self.message.iter().chain(&self.label).map(|(_, it)| it);
        let formats = formats.chain(&self.notes).chain(self.parent_label.iter().map(|(_, it)| it));
        for format in formats {
            for (param, at) in &format.params {
                visit(param, *at);
            }
        }
    }
}

/// rustc's `parse_list`: the options of an attribute that has some, else its lint.
pub(super) fn parse_list<'a, 't>(
    sink: &mut Sink<'_, '_, '_>,
    args: &'a Args<'t>,
    attr_range: TextRange,
    mode: Mode,
) -> Option<&'a [Item<'t>]> {
    match args {
        Args::List(list) if !list.items.is_empty() => return Some(&list.items),
        Args::List(list) => {
            let mut diag = sink.lint(MALFORMED, "expected a literal or missing delimiter", list.range);
            diag.help(
                "only literals are allowed as values for the `message`, `note` and `label` options. These options must be separated by a comma",
            );
            sink.parsed(diag);
        }
        Args::NoArgs => {
            // alloc: the diagnostic's text
            let message = format!("missing options for `{}` attribute", mode.as_str());
            let mut diag = sink.lint(MALFORMED, message, attr_range);
            diag.help(mode.expected_options());
            sink.parsed(diag);
        }
        Args::NameValue { .. } => malformed(sink, mode, attr_range),
    }
    None
}

/// rustc's `MalFormedDiagnosticAttributeLint`.
fn malformed(sink: &mut Sink<'_, '_, '_>, mode: Mode, range: TextRange) {
    // alloc: the diagnostic's text
    let mut diag = sink.lint(MALFORMED, format!("malformed `{}` attribute", mode.as_str()), range);
    diag.span_label(sink.span(range), "invalid option found here");
    diag.help(mode.allowed_options());
    sink.parsed(diag);
}

/// rustc's `IgnoredDiagnosticOption`.
fn ignored(sink: &mut Sink<'_, '_, '_>, option: &str, first: InFile<TextRange>, later: InFile<TextRange>) {
    let later = sink.cx.span(later);
    // alloc: the diagnostic's text and span, as is all of `ignored`'s
    let message = format!("`{option}` is ignored due to previous definition of `{option}`");
    // clone: as above
    let mut diag = Diag::lint(MALFORMED, message, later.clone());
    // alloc: as above
    diag.span_label(sink.cx.span(first), format!("`{option}` is first declared here"));
    // alloc: as above
    diag.span_label(later, format!("`{option}` is later redundantly declared here"));
    sink.parsed(diag);
}

/// rustc's `parse_directive_items`: the directive `items` give, each lint as it comes to it.
pub(super) fn parse_items(sink: &mut Sink<'_, '_, '_>, mode: Mode, items: &[Item<'_>], is_root: bool) -> Directive {
    let mut directive = Directive {
        is_rustc_attr: mode == Mode::RustcOnUnimplemented,
        // alloc: empty; a directive is of a diagnostic attribute, which `diagnostic::lints` alone reads
        filters: Vec::new(),
        message: None,
        label: None,
        // alloc: as above
        notes: Vec::new(),
        parent_label: None,
    };
    for item in items {
        let span = item.range();
        let Item::Meta(meta) = item else {
            malformed(sink, mode, span);
            continue;
        };
        let Some(name) = meta.ident() else {
            malformed(sink, mode, span);
            continue;
        };
        let value = match &meta.args {
            Args::NameValue { value, .. } => match &value.str {
                Some(str) => Some((value, &**str)),
                None => {
                    malformed(sink, mode, span);
                    continue;
                }
            },
            _ => None,
        };
        let at = sink.at(span);
        match (mode, name) {
            (Mode::DiagnosticOnTypeError, "message" | "label") => malformed(sink, mode, span),
            (_, "message" | "label") => {
                let Some((lit, str)) = value else {
                    malformed(sink, mode, span);
                    continue;
                };
                let slot = if name == "message" { &directive.message } else { &directive.label };
                if let Some((first, _)) = slot {
                    ignored(sink, name, *first, at);
                } else {
                    let format = format_string(sink, lit, str, mode);
                    let slot = if name == "message" { &mut directive.message } else { &mut directive.label };
                    *slot = Some((at, format));
                }
            }
            (_, "note") => {
                let Some((lit, str)) = value else {
                    malformed(sink, mode, span);
                    continue;
                };
                let format = format_string(sink, lit, str, mode);
                directive.notes.push(format);
            }
            (Mode::RustcOnUnimplemented, "parent_label") => {
                let Some((lit, str)) = value else {
                    malformed(sink, mode, span);
                    continue;
                };
                if let Some((first, _)) = &directive.parent_label {
                    ignored(sink, name, *first, at);
                } else {
                    let format = format_string(sink, lit, str, mode);
                    directive.parent_label = Some((sink.at(lit.range()), format));
                }
            }
            (Mode::RustcOnUnimplemented, "on") if is_root => {
                let Args::List(list) = &meta.args else {
                    malformed(sink, mode, span);
                    continue;
                };
                let Some(first) = list.items.first() else {
                    let mut diag = sink.lint(FILTERS, "empty `on`-clause in `#[rustc_on_unimplemented]`", span);
                    diag.span_label(sink.span(span), "empty `on`-clause here");
                    sink.parsed(diag);
                    continue;
                };
                let filter = filter(first);
                if list.items.len() < 2 {
                    malformed(sink, mode, span);
                    continue;
                }
                match filter {
                    Ok(params) => {
                        let nested = parse_items(sink, mode, &list.items[1..], false);
                        directive.filters.push((params, sink.at(first.range()), nested));
                    }
                    Err(invalid) => invalid.emit(sink),
                }
            }
            _ => malformed(sink, mode, span),
        }
    }
    directive
}

/// rustc's `merge_directives`: `later`, of a later attribute, into what the ones before gave.
pub(super) fn merge(sink: &mut Sink<'_, '_, '_>, first: &mut Option<Directive>, later: Directive) {
    let Some(existing) = first.as_mut() else {
        *first = Some(later);
        return;
    };
    let Directive { is_rustc_attr, filters, message, label, notes, parent_label } = later;
    existing.is_rustc_attr |= is_rustc_attr;
    existing.filters.extend(filters);
    merge_option(sink, &mut existing.message, message, "message");
    merge_option(sink, &mut existing.label, label, "label");
    existing.notes.extend(notes);
    merge_option(sink, &mut existing.parent_label, parent_label, "parent_label");
}

fn merge_option(
    sink: &mut Sink<'_, '_, '_>,
    first: &mut Option<(InFile<TextRange>, Format)>,
    later: Option<(InFile<TextRange>, Format)>,
    option: &str,
) {
    let Some(later) = later else { return };
    match first.as_ref() {
        Some((at, _)) => ignored(sink, option, *at, later.0),
        None => *first = Some(later),
    }
}

/// The format string `value` of the literal `lit`, its lints given.
fn format_string(sink: &mut Sink<'_, '_, '_>, lit: &Lit<'_>, value: &str, mode: Mode) -> Format {
    match format::parse(value, lit.text(), lit.range(), mode) {
        Ok(parsed) => {
            for warning in parsed.warnings {
                let mut diag = match warning {
                    Warning::Positional(range) => {
                        let mut diag = sink.lint(
                            FORMAT_LITERALS,
                            "positional arguments are not permitted in diagnostic attributes",
                            range,
                        );
                        diag.span_label(sink.span(range), "remove this format argument");
                        diag.help("you can print empty braces by escaping them");
                        diag
                    }
                    Warning::Indexed(range) => {
                        let mut diag = sink.lint(
                            FORMAT_LITERALS,
                            "indexed format arguments are not permitted in diagnostic attributes",
                            range,
                        );
                        diag.span_label(sink.span(range), "remove this format argument");
                        diag
                    }
                    Warning::Specifier(range) => {
                        let message = "format specifiers are not permitted in diagnostic attributes";
                        let mut diag = sink.lint(FORMAT_LITERALS, message, range);
                        diag.span_label(sink.span(range), "remove this format specifier");
                        diag
                    }
                    Warning::Disallowed(range) => {
                        // alloc: the diagnostic's text
                        let message = format!("this format argument is not allowed in `#[{}]`", mode.as_str());
                        let mut diag = sink.lint(FORMAT_LITERALS, message, range);
                        diag.span_label(sink.span(range), "remove this format argument");
                        diag.note(mode.allowed_format_arguments());
                        diag
                    }
                };
                sink.parsed(diag);
            }
            // alloc: the parameters of a diagnostic attribute's format string
            Format { params: parsed.params.into_iter().map(|(name, range)| (name, sink.at(range))).collect() }
        }
        Err(failed) => {
            let mut diag = sink.lint(FORMAT_LITERALS, failed.description, lit.range());
            diag.span_label(sink.span(failed.at), failed.label);
            sink.parsed(diag);
            Format::default()
        }
    }
}

/// rustc's `InvalidOnClause`, each with its own primary span.
enum Invalid {
    Literal(TextRange),
    ExpectedIdentifier(TextRange, String),
    Predicate(TextRange, String),
    NotOne(TextRange),
    Flag(TextRange, String),
}

impl Invalid {
    fn emit(self, sink: &mut Sink<'_, '_, '_>) {
        let (range, message, label) = match self {
            Invalid::Literal(range) => {
                // alloc: the diagnostic's text, as is each label below
                (range, "literals inside `on`-clauses are not supported", "unexpected literal here".to_owned())
            }
            Invalid::ExpectedIdentifier(range, path) => (
                range,
                "expected an identifier inside this `on`-clause",
                // alloc: as above
                format!("expected an identifier here, not `{path}`"),
            ),
            Invalid::Predicate(range, name) => (
                range,
                "this predicate is invalid",
                // alloc: as above
                format!("expected one of `any`, `all` or `not` here, not `{name}`"),
            ),
            Invalid::NotOne(range) => {
                // alloc: as above
                (range, "expected a single predicate in `not(..)`", "unexpected quantity of predicates here".to_owned())
            }
            Invalid::Flag(range, name) => (
                range,
                "invalid flag in `on`-clause",
                // alloc: as above
                format!("expected one of the `crate_local`, `direct` or `from_desugaring` flags, not `{name}`"),
            ),
        };
        let mut diag = sink.lint(FILTERS, message, range);
        diag.span_label(sink.span(range), label);
        sink.parsed(diag);
    }
}

/// rustc's `parse_filter`: the parameters the filter `item` names, in `visit_params`' order.
fn filter(item: &Item<'_>) -> Result<Vec<String>, Invalid> {
    // alloc: the parameters of a diagnostic attribute's `on(..)` filter
    let mut params = Vec::new();
    predicate(item, &mut params)?;
    Ok(params)
}

/// rustc's `parse_predicate`.
fn predicate(item: &Item<'_>, params: &mut Vec<String>) -> Result<(), Invalid> {
    let Item::Meta(meta) = item else { return Err(Invalid::Literal(item.range())) };
    let Some(name) = meta.ident() else {
        return Err(Invalid::ExpectedIdentifier(meta.path_range, meta.path_text()));
    };
    match &meta.args {
        Args::List(list) => match name {
            "any" | "all" => list.items.iter().try_for_each(|it| predicate(it, params)),
            "not" => match &list.items[..] {
                [single] => predicate(single, params),
                _ => Err(Invalid::NotOne(list.range)),
            },
            // alloc: the diagnostic's text
            _ => Err(Invalid::Predicate(meta.path_range, name.to_owned())),
        },
        Args::NameValue { range, value } => {
            let Some(str) = &value.str else { return Err(Invalid::Literal(*range)) };
            if !matches!(name, "Self" | "from_desugaring" | "cause") {
                // alloc: as `filter`'s parameters
                params.push(name.to_owned());
            }
            format::filter_params(str, params);
            Ok(())
        }
        Args::NoArgs => match name {
            "crate_local" | "direct" | "from_desugaring" => Ok(()),
            // alloc: the diagnostic's text
            _ => Err(Invalid::Flag(meta.path_range, name.to_owned())),
        },
    }
}
