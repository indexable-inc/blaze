//! The complexity lints (docs/lints.md, "Complexity"): how hard a function, a type, a file or
//! a crate's modules are to hold in one's head, by the measures that predict defects and
//! effort best, each refused past a bound. Twelve lints over fourteen measures ([`Measure`]):
//!
//! - `function_weight`: cognitive complexity by Campbell's rules ([`weight`]);
//! - `path_count`: acyclic paths through the function's MIR ([`paths`]);
//! - `live_state`: the most mutable places live across one call (MIR liveness);
//! - `effect_breadth`: the kinds of effect its own calls into other crates bring;
//! - `exits`: early exits of a function that holds a value whose drop finishes something;
//! - `long_function`: lines of code, and MIR statements;
//! - `long_file`: lines of code of a file, its tests left out;
//! - `many_params`: parameters, `self` not counted;
//! - `wide_type`: fields of a struct, variants of an enum with data;
//! - `deep_nesting`: control-flow depth ([`weight`]);
//! - `fan_out`: distinct functions called, the standard library's left out;
//! - `module_cycle`: modules of one crate that import from each other ([`modules`]).
//!
//! Each finding names the measure, the value, the bound and one concrete way down (the part
//! that, moved into its own function, brings the value under the bound, by its lines and share).
//! An item over its bound that the held values ([`crate::baseline::items`], `Conf::items`) hold
//! at or above its value is not a finding: existing code may not grow, new code meets the
//! bound. Each finding's note is its held line, which a bless reads. Test code is not measured.
//! Bounds are `clippy.toml` keys ([`Measure::key`]); docs/lints.md says where each default
//! comes from.

mod function;
mod modules;
mod paths;
mod weight;

#[cfg(test)]
mod tests;

use std::alloc::Global;

use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Finding, Lint, facts};
use crate::{
    baseline::items::{self, Key},
    check::Cx,
    conf::Conf,
};

/// The lints of this module.
pub(super) const LINTS: [Lint; 12] = [
    Lint::FunctionWeight,
    Lint::PathCount,
    Lint::LiveState,
    Lint::EffectBreadth,
    Lint::Exits,
    Lint::LongFunction,
    Lint::LongFile,
    Lint::ManyParams,
    Lint::WideType,
    Lint::DeepNesting,
    Lint::FanOut,
    Lint::ModuleCycle,
];

/// One measure: what a held line names, what bound it is held to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Measure {
    Weight,
    Paths,
    Live,
    Effects,
    Exits,
    FunctionLines,
    FunctionStatements,
    FileLines,
    Params,
    Fields,
    Variants,
    Nesting,
    FanOut,
    Cycle,
}

impl Measure {
    pub(crate) const ALL: [Measure; 14] = [
        Measure::Weight,
        Measure::Paths,
        Measure::Live,
        Measure::Effects,
        Measure::Exits,
        Measure::FunctionLines,
        Measure::FunctionStatements,
        Measure::FileLines,
        Measure::Params,
        Measure::Fields,
        Measure::Variants,
        Measure::Nesting,
        Measure::FanOut,
        Measure::Cycle,
    ];

    /// Its name in a held line.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Measure::Weight => "function_weight",
            Measure::Paths => "path_count",
            Measure::Live => "live_state",
            Measure::Effects => "effect_breadth",
            Measure::Exits => "exits",
            Measure::FunctionLines => "function_lines",
            Measure::FunctionStatements => "function_statements",
            Measure::FileLines => "file_lines",
            Measure::Params => "params",
            Measure::Fields => "fields",
            Measure::Variants => "variants",
            Measure::Nesting => "nesting",
            Measure::FanOut => "fan_out",
            Measure::Cycle => "module_cycle",
        }
    }

    pub(crate) const fn lint(self) -> Lint {
        match self {
            Measure::Weight => Lint::FunctionWeight,
            Measure::Paths => Lint::PathCount,
            Measure::Live => Lint::LiveState,
            Measure::Effects => Lint::EffectBreadth,
            Measure::Exits => Lint::Exits,
            Measure::FunctionLines | Measure::FunctionStatements => Lint::LongFunction,
            Measure::FileLines => Lint::LongFile,
            Measure::Params => Lint::ManyParams,
            Measure::Fields | Measure::Variants => Lint::WideType,
            Measure::Nesting => Lint::DeepNesting,
            Measure::FanOut => Lint::FanOut,
            Measure::Cycle => Lint::ModuleCycle,
        }
    }

    /// The `clippy.toml` key of its bound (conf.rs's `COUNTS` has each).
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Measure::Weight => "function-weight-threshold",
            Measure::Paths => "path-count-threshold",
            Measure::Live => "live-state-threshold",
            Measure::Effects => "effect-breadth-threshold",
            Measure::Exits => "exits-threshold",
            Measure::FunctionLines => "function-lines-threshold",
            Measure::FunctionStatements => "function-statements-threshold",
            Measure::FileLines => "file-lines-threshold",
            Measure::Params => "params-threshold",
            Measure::Fields => "struct-fields-threshold",
            Measure::Variants => "enum-variants-threshold",
            Measure::Nesting => "nesting-threshold",
            Measure::FanOut => "fan-out-threshold",
            Measure::Cycle => "module-cycle-threshold",
        }
    }

    /// Its bound when `clippy.toml` sets none: the largest value that is not a finding.
    /// docs/lints.md's "Complexity" table says where each number comes from.
    pub(crate) const fn default_bound(self) -> u64 {
        match self {
            // SonarSource's default for rule S3776 (Campbell's measure), all languages.
            Measure::Weight => 15,
            // NPATH's threshold as Nejmeh proposed it, and PMD's default.
            Measure::Paths => 200,
            // Working memory holds about four chunks (Cowan 2001), seven at most (Miller 1956).
            Measure::Live => 6,
            // Of six kinds; allocation and panics come with most calls, so this is two of IO,
            // blocking, syncing and locking together.
            Measure::Effects => 4,
            Measure::Exits => 5,
            // SonarSource's S138 default.
            Measure::FunctionLines => 100,
            // About four MIR statements per line of code at the line bound (estimate).
            Measure::FunctionStatements => 400,
            // SonarSource's S104 default.
            Measure::FileLines => 1000,
            // clippy's `too_many_arguments` and SonarSource's S107 default.
            Measure::Params => 7,
            Measure::Fields => 20,
            Measure::Variants => 50,
            // The tree's 95th percentile (estimate); SonarSource's S134 default is 3.
            Measure::Nesting => 4,
            Measure::FanOut => 30,
            // No two modules of a crate in a cycle.
            Measure::Cycle => 1,
        }
    }
}

/// `measure`'s bound under `conf`.
pub(crate) fn bound_of(conf: &Conf, measure: Measure) -> u64 {
    let default = i64::try_from(measure.default_bound()).unwrap_or(i64::MAX);
    u64::try_from(conf.int(measure.key(), default)).unwrap_or(0)
}

/// Whether `value` is a finding: over `bound`, and over what it is held at if it is held.
pub(crate) fn fires(value: u64, bound: u64, held: Option<u64>) -> bool {
    value > bound && held.is_none_or(|it| value > it)
}

/// What a held line names an item by.
#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub(crate) path: String,
    pub(crate) fingerprint: u64,
}

/// A measure over its bound.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Over {
    pub(crate) measure: Measure,
    pub(crate) value: u64,
    pub(crate) bound: u64,
    pub(crate) held: Option<u64>,
}

/// `value` of `measure` for `item`, when it is a finding and its lint is on.
pub(crate) fn judge(cx: &Cx<'_, '_>, measure: Measure, item: &Item, value: u64) -> Option<Over> {
    if !cx.on_blaze(measure.lint()) {
        return None;
    }
    let bound = bound_of(cx.conf(), measure);
    let held = cx
        .conf()
        .items
        .as_ref()
        .and_then(|it| it.items.held(measure.name(), &item.path, item.fingerprint));
    fires(value, bound, held).then_some(Over { measure, value, bound, held })
}

/// A finding before it is reported: where, what it says, and where the way down is.
pub(crate) struct Pending {
    pub(crate) at: TextRange,
    pub(crate) item: Item,
    pub(crate) over: Over,
    /// What is measured, with its value: "`fn f` weighs 41 by cognitive complexity".
    pub(crate) said: String,
    /// The way down: the part to change, and what to do there.
    pub(crate) way: Option<(TextRange, String)>,
    /// The way down when it has no one place.
    pub(crate) help: Option<String>,
}

pub(crate) fn report(cx: &mut Cx<'_, '_>, pending: impl IntoIterator<Item = Pending>) {
    for it in pending {
        let Over { measure, value, bound, held } = it.over;
        let held_at = match held {
            Some(was) => format!(", and over the {was} it is held at"),
            None => String::new(),
        };
        let message = format!("{}, over the bound of {bound}{held_at}", it.said);
        let key = Key { measure: measure.name().to_owned(), path: it.item.path, fingerprint: it.item.fingerprint };
        let note = items::note(&key, value);
        let way = it.way.map(|(range, text)| (cx.span(range), text));
        let finding = Finding { lint: measure.lint(), range: it.at, message, note: Some(note) };
        let diagnostic = facts::report_one(cx, finding);
        if let Some((span, text)) = way {
            diagnostic.span_help(span, text);
        }
        if let Some(text) = it.help {
            diagnostic.help(text);
        }
    }
}

/// The complexity lints of the body or file the walk is at.
pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !LINTS.iter().any(|&it| cx.on_blaze(it)) || cx.expansion().is_some() {
        return;
    }
    let root = cx.root();
    if let Some(f) = ast::Fn::cast(root) {
        function::check(cx, f);
        return;
    }
    if root.kind() == SyntaxKind::SOURCE_FILE {
        file(cx, root);
        if cx.at_crate_root() {
            modules::check(cx);
        }
    }
}

/// The fingerprint of `node`'s tokens, trivia and the tokens of its own name left out.
pub(crate) fn fingerprint_of(node: ast::Node<'_>, name: Option<TextRange>) -> u64 {
    let tokens = node.descendants_with_tokens().filter_map(|it| match it {
        NodeOrToken::Token(token) => Some(token),
        NodeOrToken::Node(_) => None,
    });
    let kept = tokens
        .filter(|it| !it.kind().is_trivia())
        .filter(|it| name.is_none_or(|name| !name.contains_range(it.text_range())))
        .map(|it| it.text());
    items::fingerprint(kept)
}

/// The text of `range` of `text`.
pub(crate) fn slice(text: &str, range: TextRange) -> &str {
    text.get(usize::from(range.start())..usize::from(range.end())).unwrap_or_default()
}

/// The lines of code of `text`: those with something but a `//` comment on them.
pub(crate) fn code_lines(text: &str) -> u64 {
    text.lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty() && !line.starts_with("//")
        })
        .count() as u64
}

/// The line `at` is on, counted from the line `from` is on as 1.
pub(crate) fn line_from(text: &str, from: TextSize, at: TextSize) -> usize {
    let (from, at) = (usize::from(from), usize::from(at));
    if at <= from {
        return 1;
    }
    1 + text.get(from..at).map_or(0, |it| it.bytes().filter(|&b| b == b'\n').count())
}

/// A share of `total` as a whole percent.
pub(crate) fn percent(part: u64, total: u64) -> u64 {
    part.saturating_mul(100) / total.max(1)
}

/// `long_file`, over the file's own tree: its lines of code, items under `#[cfg(test)]` left
/// out. Named by the module the file is: its first item's.
fn file<'t>(cx: &mut Cx<'_, 't>, root: ast::Node<'t>) {
    if !cx.in_file() || !cx.on_blaze(Lint::LongFile) {
        return;
    }
    let text = cx.text();
    let mut lines = code_lines(text);
    let mut module = None;
    let mut parts: Vec<(TextRange, u64), Global> = Vec::new_in(Global);
    for child in root.children() {
        if !ast::Item::can_cast(child.kind()) {
            continue;
        }
        let it = code_lines(slice(text, child.text_range()));
        if cx.is_in_test(child) {
            lines = lines.saturating_sub(it);
            continue;
        }
        parts.push((child.text_range(), it));
        if module.is_none() {
            module = cx.def(child).and_then(|it| it.module(cx.db()));
        }
    }
    let Some(module) = module else { return };
    let path = super::sans_io_core::module_path(cx.db(), module);
    let item = Item { path, fingerprint: items::fingerprint([text]) };
    let Some(over) = judge(cx, Measure::FileLines, &item, lines) else { return };
    let way = weight::way_down(lines, over.bound, &parts).map(|at| {
        let (range, part) = parts[at];
        let from = line_from(text, 0.into(), range.start());
        let to = line_from(text, 0.into(), range.end());
        let text = format!(
            "move this item (lines {from}-{to}, {part} of the {lines} lines, {}%) into a module of its own",
            percent(part, lines)
        );
        (range, text)
    });
    let at = root.first_token().map_or(TextRange::empty(0.into()), |it| it.text_range());
    let said = format!("this file (`{}`) is {lines} lines of code, its tests left out", item.path);
    report(cx, [Pending { at, item, over, said, way, help: None }]);
}

/// `wide_type` of a struct: its fields.
pub(super) fn check_struct<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(Lint::WideType) || cx.expansion().is_some() || cx.is_in_test(item.syntax()) {
        return;
    }
    let mut names: Vec<String, Global> = Vec::new_in(Global);
    let fields = match item.kind() {
        ast::StructKind::Record(list) => {
            for field in list.fields() {
                if let Some(name) = field.name() {
                    names.push(name.syntax().text().to_owned());
                }
            }
            list.fields().count()
        }
        ast::StructKind::Tuple(list) => list.fields().count(),
        ast::StructKind::Unit => 0,
    };
    let help = group_of(&names).map(|(prefix, count)| {
        format!(
            "the {count} fields named `{prefix}_..` travel together: make them one struct, a field of this one"
        )
    });
    wide(cx, item.syntax(), item.name(), Measure::Fields, fields as u64, help);
}

/// `wide_type` of an enum: its variants, when one has data (a list of names, a fieldless
/// enum, is read one at a time).
pub(super) fn check_enum<'t>(cx: &mut Cx<'_, 't>, item: ast::Enum<'t>) {
    if !cx.on_blaze(Lint::WideType) || cx.expansion().is_some() || cx.is_in_test(item.syntax()) {
        return;
    }
    let Some(list) = item.variant_list() else { return };
    if list.variants().all(|it| it.field_list().is_none()) {
        return;
    }
    let variants = list.variants().count() as u64;
    let help = Some(
        "group the variants that are handled together into an enum of their own, one variant here that holds it"
            .to_owned(),
    );
    wide(cx, item.syntax(), item.name(), Measure::Variants, variants, help);
}

/// `wide_type` of the struct or enum `node`: `value` of `measure` (its fields or variants).
fn wide<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    name: Option<ast::Name<'t>>,
    measure: Measure,
    value: u64,
    help: Option<String>,
) {
    let (kind, what) = match measure {
        Measure::Variants => ("enum", "variants"),
        _ => ("struct", "fields"),
    };
    let Some(name) = name else { return };
    let Some(def) = cx.def(node) else { return };
    let Some(module) = def.module(cx.db()) else { return };
    let shown = name.syntax().text();
    let path = format!("{}::{shown}", super::sans_io_core::module_path(cx.db(), module));
    let item = Item { path, fingerprint: fingerprint_of(node, Some(name.syntax().text_range())) };
    let Some(over) = judge(cx, measure, &item, value) else { return };
    let Some(at) = cx.range(name.syntax()) else { return };
    let said = format!("`{kind} {shown}` has {value} {what}");
    report(cx, [Pending { at, item, over, said, way: None, help }]);
}

/// The commonest prefix (before the first `_`) of `names` that at least three share, with how
/// many do; the first in name order of equal counts.
pub(crate) fn group_of(names: &[String]) -> Option<(&str, usize)> {
    let mut prefixes: Vec<&str, Global> = Vec::with_capacity_in(names.len(), Global);
    prefixes.extend(names.iter().filter_map(|it| it.split_once('_').map(|(prefix, _)| prefix)));
    prefixes.sort_unstable();
    let mut best: Option<(&str, usize)> = None;
    for run in prefixes.chunk_by(|a, b| a == b) {
        if run.len() >= 3 && best.is_none_or(|(_, count)| run.len() > count) {
            best = Some((run[0], run.len()));
        }
    }
    best
}
