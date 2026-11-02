//! `unexpected_cfgs`: each cfg name and value rustc reads that `--check-cfg` and the names
//! rustc knows do not expect, as expansion reads them: the `#[cfg]`s and `#[cfg_attr]`s of every
//! node it configures, in order, up to the first that is false (a `cfg_attr`'s own attributes
//! when it holds), the predicate of each `cfg!` and of each arm of a `cfg_select!`.
//!
//! The lint is buffered for the node rustc's expansion is in when it reads the cfg: the nearest
//! node around the attribute's that has an id, as a node's cfgs are read before it is given one
//! (so `#[allow(unexpected_cfgs)]` on the node itself does not reach them).
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/cfg.rs` (`parse_cfg_entry`,
//! `parse_cfg_entry_target`, `parse_name_value`), `attributes/cfg_select.rs`
//! (`parse_cfg_select`), `compiler/rustc_attr_parsing/src/check_cfg.rs` (`unexpected_cfg_name`,
//! `unexpected_cfg_value`), the messages of `compiler/rustc_attr_parsing/src/diagnostics.rs`
//! (`UnexpectedCfgName`, `UnexpectedCfgValue` and their parts),
//! `compiler/rustc_session/src/config/cfg.rs` (`fill_well_known`, `ExpectedValues`) and
//! `compiler/rustc_expand/src/expand.rs` (`InvocationCollector`: `take_first_attr`,
//! `expand_cfg_true`, `expand_cfg_attr`, `assign_id!`) (MIT or Apache-2.0).

use std::ops::ControlFlow;

use blaze_hash::{HashMap, HashSet};
use blaze_ids::radix;
use cfg::{CfgExpr, CfgOptions};
use hir::{HirFileId, InFile, MacroCallId};
use hir_expand::{MacroCallKind, MacroDefKind, builtin::BuiltinFnLikeExpander};
use intern::Symbol;
use stdx::edit_distance::find_best_match_for_name;
use syntax::{
    Edition, NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode as _, AstToken as _, HasLoopBody as _},
};

use super::{
    super::{Emitted, Pass, Stage, Tree, Walk, source_fact},
    CheckCfg, Values, meta, well_known,
};
use crate::{
    cx::{Cx, derive_paths_view},
    lower::Phase,
    model::{Applicability, Diag, MultiSpan, Span, Sublevel},
};

/// rustc's `MAX_CHECK_CFG_NAMES_OR_VALUES`.
const MAX_SHOWN: usize = 35;

/// The well-known names that also get the values of the target being built for.
const TARGET_NAMES: &[&str] = &[
    "target_abi",
    "target_arch",
    "target_endian",
    "target_env",
    "target_family",
    "target_os",
    "target_pointer_width",
    "target_vendor",
    "target_object_format",
];

/// What rustc's `CheckCfg` expects once `fill_well_known` ran: the names given, in the order
/// given, then the well-known ones, each with its values (`None`: any).
struct Expected<'c> {
    names: Vec<&'c str>,
    index: HashMap<&'c str, usize>,
    values: Vec<Option<Set<'c>>>,
    well_known: HashSet<&'static str>,
    exhaustive_names: bool,
}

/// The values of a name, `None` for the name alone, in the order first added.
#[derive(Default)]
struct Set<'c> {
    order: Vec<Option<&'c str>>,
    has: HashSet<Option<&'c str>>,
}

impl<'c> Set<'c> {
    fn insert(&mut self, value: Option<&'c str>) {
        if self.has.insert(value) {
            self.order.push(value);
        }
    }

    fn contains(&self, value: Option<&str>) -> bool {
        self.has.contains(&value)
    }
}

impl<'c> Expected<'c> {
    fn new(check: &'c CheckCfg, cfg: &'c CfgOptions) -> Expected<'c> {
        let mut expected = Expected {
            // alloc: setup, once per check, the expected names
            names: Vec::new(),
            index: HashMap::default(),
            // alloc: setup, as above
            values: Vec::new(),
            // alloc: setup, as above
            well_known: well_known::WELL_KNOWN.iter().map(|&(name, ..)| name).collect(),
            exhaustive_names: check.exhaustive_names(),
        };
        for (name, values) in check.expecteds() {
            let values = match values {
                Values::Any => None,
                Values::Some(values) => {
                    let mut set = Set::default();
                    values.iter().for_each(|it| set.insert(it.as_deref()));
                    Some(set)
                }
            };
            expected.push(name, values);
        }
        for &(name, start, values) in well_known::WELL_KNOWN {
            let at = match expected.index.get(name) {
                Some(&at) => at,
                None => {
                    let mut set = Set::default();
                    if start == well_known::Start::NoValue {
                        set.insert(None);
                    }
                    expected.push(name, Some(set))
                }
            };
            if let Some(set) = &mut expected.values[at] {
                values.iter().for_each(|&it| set.insert(Some(it)));
            }
        }
        for name in TARGET_NAMES {
            if let Some(&at) = expected.index.get(name)
                && let Some(set) = &mut expected.values[at]
            {
                cfg.get_cfg_values(name).for_each(|it| set.insert(Some(it.as_str())));
            }
        }
        expected
    }

    fn push(&mut self, name: &'c str, values: Option<Set<'c>>) -> usize {
        let at = self.names.len();
        self.names.push(name);
        self.index.insert(name, at);
        self.values.push(values);
        at
    }

    /// rustc's `expecteds.get(name)`: `None` when the name is not expected, `Some(None)` when
    /// any value is.
    fn get(&self, name: &str) -> Option<Option<&Set<'c>>> {
        self.index.get(name).map(|&it| self.values[it].as_ref())
    }

    /// rustc's `ExpectedValues::contains`.
    fn has_value(&self, name: &str, value: Option<&str>) -> bool {
        self.get(name).is_some_and(|it| it.is_none_or(|it| it.contains(value)))
    }
}

/// A cfg name rustc reads, with its value: rustc's `parse_name_value`'s arguments.
struct Check {
    name: Symbol,
    /// Written `r#name`.
    raw: bool,
    name_range: TextRange,
    value: Option<(Symbol, TextRange)>,
    /// The predicate's (`name = "value"`).
    range: TextRange,
}

/// Where rustc's expansion is when it reads a cfg: `node` of `file`, itself or the node around
/// it (`include`).
#[derive(Clone, Copy)]
struct Anchor<'db> {
    file: HirFileId,
    node: ast::Node<'db>,
    include: bool,
}

/// Of a cfg a macro made: whether it is of another crate's or a proc macro's
/// (`Span::in_external_macro`), and for another crate's, what to say of it.
#[derive(Default)]
struct Origin {
    external: bool,
    macro_help: Option<(&'static str, String)>,
}

struct Lints<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    walk: &'a Walk<'db>,
    check: &'a CheckCfg,
    expected: Expected<'a>,
    root: HirFileId,
    build_script: bool,
    emitted: Vec<Emitted>,
    /// What of a tree is read, by index ([`Lints::tree`]): one buffer for every tree.
    read: Vec<(u32, Read)>,
    /// The scratch of `read`'s sort: one buffer for every tree.
    scratch: Vec<(u32, Read)>,
    /// The checks of one predicate: one buffer for every predicate.
    checks: Vec<Check>,
}

pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, check: &CheckCfg, walk: &Walk<'db>) -> Vec<Emitted> {
    let mut lints = Lints {
        cx,
        walk,
        check,
        expected: Expected::new(check, walk.cfg),
        root: cx.krate.root_module(cx.db).definition_source_file_id(cx.db),
        build_script: cx
            .krate
            .display_name(cx.db)
            .is_some_and(|it| it.crate_name().symbol().as_str() == "build_script_build"),
        // alloc: empty, allocates nothing
        emitted: Vec::new(),
        // alloc: empty; then one buffer for the check
        read: Vec::new(),
        // alloc: as above
        scratch: Vec::new(),
        // alloc: as above
        checks: Vec::new(),
    };
    for tree in &walk.trees {
        lints.tree(tree);
    }
    lints.emitted
}

/// What of a tree's node rustc reads cfgs of.
#[derive(Clone, Copy)]
enum Read {
    /// Its attributes.
    Attrs,
    /// Its attributes before the attribute macro that replaces it.
    Replaced,
    /// The predicate of the `cfg!` it is the expansion of.
    Predicate,
}

impl<'db> Lints<'_, '_, 'db> {
    fn tree(&mut self, tree: &Tree<'db>) {
        // Each node read is an index: into `nodes`, then `off`, then `replaced`.
        let (nodes, off) = (tree.nodes.len(), tree.off.len());
        let node = |at: u32| -> ast::Node<'db> {
            let at = at as usize;
            match at {
                _ if at < nodes => tree.nodes[at],
                _ if at < nodes + off => tree.off[at - nodes],
                _ => tree.replaced[at - nodes - off],
            }
        };
        let index = |at: usize| u32::try_from(at).expect("a tree has fewer than 2^32 nodes");
        let mut read = std::mem::take(&mut self.read);
        read.clear();
        for (at, (&kind, &attrs)) in tree.kinds.iter().zip(&tree.attrs).enumerate() {
            if attrs && ast::AnyHasAttrs::can_cast(kind) {
                read.push((index(at), Read::Attrs));
            }
            if matches!(kind, SyntaxKind::CFG_PRED_EXPR | SyntaxKind::CFG_PRED_PAT) {
                read.push((index(at), Read::Predicate));
            }
        }
        read.extend((0..off).map(|at| (index(nodes + at), Read::Attrs)));
        read.extend((0..tree.replaced.len()).map(|at| (index(nodes + off + at), Read::Replaced)));
        radix::sort_copy_by_key_in(
            &mut read,
            |&(at, _)| {
                let range = node(at).text_range();
                radix::pack_32(range.start().into(), !u32::from(range.end()))
            },
            &mut self.scratch,
        );
        for &(at, what) in &read {
            let node = node(at);
            match what {
                Read::Attrs => self.attrs(tree, node, None),
                Read::Replaced => {
                    let invoked = ast::Item::cast(node).and_then(|it| self.invoking_meta(it));
                    self.attrs(tree, node, Some(invoked))
                }
                Read::Predicate => {
                    let predicate = ast::CfgPredExpr::cast(node)
                        .and_then(|it| it.cfg_predicate())
                        .or_else(|| ast::CfgPredPat::cast(node).and_then(|it| it.cfg_predicate()));
                    if let Some(predicate) = predicate {
                        let anchor = Anchor { file: tree.file, node, include: false };
                        self.predicate(tree.file, predicate, &anchor);
                    }
                }
            }
        }
        self.read = read;
        if let Some(call) = tree.call {
            self.cfg_select(call);
        }
    }

    /// The `#[cfg]`s and `#[cfg_attr]`s of `owner`, outer then inner, in order, up to the first
    /// that is false; for an item an attribute macro replaces (`stop`), up to that macro's.
    fn attrs(&mut self, tree: &Tree<'db>, owner: ast::Node<'db>, stop: Option<Option<ast::Meta<'db>>>) {
        let Some(attrs) = ast::AnyHasAttrs::cast(owner) else { return };
        let anchor = Anchor { file: tree.file, node: owner, include: false };
        for attr in ast::attrs_including_inner(&attrs) {
            let Some(meta) = attr.meta() else { continue };
            if self.meta(tree.file, meta, stop, &anchor).is_break() {
                return;
            }
        }
    }

    fn meta(
        &mut self,
        file: HirFileId,
        meta: ast::Meta<'db>,
        stop: Option<Option<ast::Meta<'db>>>,
        anchor: &Anchor<'db>,
    ) -> ControlFlow<()> {
        if let Some(stop) = stop
            && stop.is_none_or(|it| it == meta)
        {
            return ControlFlow::Break(());
        }
        let cfg = self.walk.cfg;
        match meta {
            ast::Meta::UnsafeMeta(it) => match it.meta() {
                Some(meta) => self.meta(file, meta, stop, anchor),
                None => ControlFlow::Continue(()),
            },
            ast::Meta::CfgMeta(it) => {
                let Some(predicate) = it.cfg_predicate() else { return ControlFlow::Continue(()) };
                self.predicate(file, predicate, anchor);
                match cfg.check(&CfgExpr::parse_from_view(predicate)) == Some(false) {
                    true => ControlFlow::Break(()),
                    false => ControlFlow::Continue(()),
                }
            }
            ast::Meta::CfgAttrMeta(it) => {
                let Some(predicate) = it.cfg_predicate() else { return ControlFlow::Continue(()) };
                self.predicate(file, predicate, anchor);
                if cfg.check(&CfgExpr::parse_from_view(predicate)) != Some(false) {
                    for meta in it.metas() {
                        self.meta(file, meta, stop, anchor)?;
                    }
                }
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Continue(()),
        }
    }

    /// The attribute of `item` that invokes the attribute macro replacing it.
    fn invoking_meta(&self, item: ast::Item<'db>) -> Option<ast::Meta<'db>> {
        let db = self.cx.db;
        let call = self.cx.vsema.expand_attr_macro(item)?.value.file_id.macro_file()?;
        let loc = call.loc(db);
        let MacroCallKind::Attr { ast_id, censored_attr_ids, .. } = &loc.kind else { return None };
        Some(censored_attr_ids.invoc_attr().find_attr_range_view(db, loc.krate, *ast_id).1)
    }

    fn predicate(&mut self, file: HirFileId, predicate: ast::CfgPredicate<'_>, anchor: &Anchor<'db>) {
        let mut checks = std::mem::take(&mut self.checks);
        ast_checks(predicate, &mut checks);
        for check in checks.drain(..) {
            self.lint(file, check, anchor);
        }
        self.checks = checks;
    }

    /// The arms of a `cfg_select!` that `call` is the expansion of, as rustc's
    /// `parse_cfg_select` reads them, up to the first it cannot.
    fn cfg_select(&mut self, call: MacroCallId) {
        let db = self.cx.db;
        let loc = call.loc(db);
        if !matches!(loc.def.kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::CfgSelect)) {
            return;
        }
        let node = loc.to_node_view(db);
        let Some(tt) = ast::MacroCall::cast(node.value).and_then(|it| it.token_tree()) else { return };
        let toks: Vec<meta::Tok<'_>> = tt
            .syntax()
            .descendants_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|it| !it.kind().is_trivia())
            .map(|it| meta::Tok { kind: it.kind(), text: it.text(), range: it.text_range() })
            // alloc: once per `cfg_select!`, its tokens to parse
            .collect();
        let Some(inner) = toks.get(1..toks.len().saturating_sub(1)) else { return };
        let anchor = Anchor { file: node.file_id, node: node.value, include: true };
        // alloc: once per `cfg_select!`, its checks
        let mut checks = Vec::new();
        let mut parser = meta::Parser::new(inner, self.cx.edition);
        while !parser.at_end() {
            while parser.eat_attr() {}
            if !parser.eat_underscore() {
                let Some(predicate) = parser.inner() else { break };
                meta_checks(&predicate, &mut checks);
            }
            if !parser.eat_fat_arrow() || !parser.skip_delimited() {
                break;
            }
        }
        for check in checks {
            self.lint(anchor.file, check, &anchor);
        }
    }

    /// rustc's `parse_name_value` once the name and value are read: the lint, if the cfg is
    /// not expected.
    fn lint(&mut self, file: HirFileId, check: Check, anchor: &Anchor<'db>) {
        let value = check.value.as_ref().map(|(it, _)| it.as_str());
        let mut diag = match self.expected.get(check.name.as_str()) {
            Some(Some(values)) if !values.contains(value) => self.value_diag(file, &check),
            None if self.expected.exhaustive_names => self.name_diag(file, &check),
            _ => return,
        };
        let at = self.at(anchor);
        source_fact(self.cx, &mut diag, at);
        self.emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at).buffered(Stage::Collected));
    }

    fn span(&self, file: HirFileId, range: TextRange) -> Span {
        self.cx.span(InFile::new(file, range))
    }

    /// rustc's `unexpected_cfg_name`.
    fn name_diag(&self, file: HirFileId, check: &Check) -> Diag {
        let edition = self.edition_at(file, check.name_range);
        let expected = &self.expected;
        let name = check.name.as_str();
        let name_span = self.span(file, check.name_range);
        let value = check.value.as_ref().map(|(it, range)| (it.as_str(), self.span(file, *range)));
        let origin = self.origin(file, check.name_range);
        let mut diag = Diag::lint(
            "unexpected_cfgs",
            // alloc: the diagnostic's text, as is all of `name_diag`'s
            format!("unexpected `cfg` condition name: `{}`", meta::shown(name, edition)),
            self.span(file, check.range),
        );

        let mut is_feature = name == "feature";
        let boolean = match name {
            _ if name.eq_ignore_ascii_case("false") => Some(false),
            _ if name.eq_ignore_ascii_case("true") => Some(true),
            _ => None,
        };
        if is_feature && self.check.cargo {
            diag.help("consider defining some features in `Cargo.toml`");
        } else if let Some((_, value_span)) = &value
            && name == "version"
        {
            let between = name_span.shrink_to_hi().with_hi(u32::from(value_span.range().start()));
            diag.multipart_suggestion(
                "there is a similar config predicate: `version(\"..\")`",
                // alloc: the diagnostic's text
                vec![(between, "(".to_owned()), (value_span.shrink_to_hi(), ")".to_owned())],
                Applicability::MachineApplicable,
            );
        } else if value.is_none()
            && let Some(boolean) = boolean
            && !check.raw
        {
            diag.suggestion_verbose(
                // clone: the diagnostic's span and text
                name_span.clone(),
                // alloc: the diagnostic's text
                format!(
                    "you may have meant to use `{boolean}` (notice the capitalization). Doing so makes this predicate evaluate to `{boolean}` unconditionally"
                ),
                // alloc: the diagnostic's text
                boolean.to_string(),
                Applicability::MachineApplicable,
            );
        } else if let Some(best) = find_best_match_for_name(&expected.names, name, None).copied() {
            is_feature |= best == "feature";
            let shown = meta::shown(best, edition);
            let maybe = Applicability::MaybeIncorrect;
            match expected.get(best).flatten() {
                Some(best_values) => {
                    // alloc: the diagnostic's list of values
                    let mut possibilities: Vec<&str> = best_values.order.iter().flatten().copied().collect();
                    possibilities.sort_unstable();
                    let expected_values = |diag: &mut Diag| {
                        if !possibilities.is_empty() {
                            // alloc: the diagnostic's text
                            diag.help(format!("expected values for `{best}` are: {}", listed(&possibilities, |it| it)));
                        }
                    };
                    match &value {
                        Some((value, value_span)) => {
                            let both = name_span.to(value_span);
                            if best_values.contains(Some(*value)) {
                                diag.suggestion(
                                    // clone: the diagnostic's span
                                    name_span.clone(),
                                    "there is a config with a similar name and value",
                                    shown,
                                    maybe,
                                );
                            } else if best_values.contains(None) {
                                diag.suggestion(both, "there is a config with a similar name and no value", shown, maybe);
                            } else {
                                expected_values(&mut diag);
                                let code = match possibilities.first() {
                                    // alloc: the diagnostic's text
                                    Some(first) => format!("{shown} = \"{first}\""),
                                    None => shown.into_owned(),
                                };
                                diag.suggestion(both, "there is a config with a similar name and different values", code, maybe);
                            }
                        }
                        None => {
                            expected_values(&mut diag);
                            // clone: the diagnostic's span
                            diag.suggestion(name_span.clone(), "there is a config with a similar name", shown, maybe);
                        }
                    }
                }
                None => {
                    // clone: the diagnostic's span
                    diag.suggestion(name_span.clone(), "there is a config with a similar name", shown, maybe);
                }
            }
        } else {
            let mut similar: Vec<&str> = match value {
                // alloc: empty, allocates nothing
                Some(_) => Vec::new(),
                None => expected
                    .names
                    .iter()
                    .zip(&expected.values)
                    .filter(|(_, values)| values.as_ref().is_some_and(|it| it.contains(Some(name))))
                    .map(|(it, _)| *it)
                    // alloc: the diagnostic's list of names
                    .collect(),
            };
            if !similar.is_empty() && similar.len() <= 3 {
                similar.sort_unstable();
                for it in similar {
                    diag.suggestion(
                        // clone: the diagnostic's span and text
                        name_span.clone(),
                        "found config with similar value",
                        // alloc: the diagnostic's text
                        format!("{it} = \"{name}\""),
                        Applicability::MaybeIncorrect,
                    );
                }
            }
            let mut names: Vec<&str> =
                // alloc: the diagnostic's list of names
                expected.names.iter().copied().filter(|it| !expected.well_known.contains(it)).collect();
            names.sort_unstable();
            let shown = names.len().min(MAX_SHOWN);
            let and_more = expected.names.len() - shown;
            names.truncate(shown);
            if !names.is_empty() {
                let more = match and_more {
                    // alloc: empty, allocates nothing
                    0 => String::new(),
                    // alloc: the diagnostic's text
                    n => format!(" and {n} more"),
                };
                let listed = listed(&names, |it| meta::shown(it, edition));
                // alloc: the diagnostic's text
                diag.sub(Sublevel::Help, format!("expected names are: {listed}{more}"), MultiSpan::default(), true);
            }
        }

        let arg = |quotes| check_cfg_arg(&meta::shown(name, edition), value.as_ref().map(|(it, _)| *it), quotes);
        if self.check.cargo {
            self.macro_help(&mut diag, &origin);
            if !is_feature && !origin.external {
                self.cargo_help(&mut diag, &arg);
            }
            diag.note(CARGO_NOTE);
        } else {
            self.macro_help(&mut diag, &origin);
            // alloc: the diagnostic's text
            diag.help(format!("to expect this configuration use `--check-cfg={}`", arg(false)));
            diag.note(RUSTC_NOTE);
        }
        diag
    }

    /// rustc's `unexpected_cfg_value`.
    fn value_diag(&self, file: HirFileId, check: &Check) -> Diag {
        let edition = self.edition_at(file, check.name_range);
        let expected = &self.expected;
        let name = check.name.as_str();
        let name_span = self.span(file, check.name_range);
        let value = check.value.as_ref().map(|(it, range)| (it.as_str(), self.span(file, *range)));
        let origin = self.origin(file, check.name_range);
        let message = match &value {
            // alloc: the diagnostic's text, as is all of `value_diag`'s
            Some((value, _)) => format!("unexpected `cfg` condition value: `{value}`"),
            // alloc: as above
            None => "unexpected `cfg` condition value: (none)".to_owned(),
        };
        let mut diag = Diag::lint("unexpected_cfgs", message, self.span(file, check.range));
        let Some(Some(values)) = expected.get(name) else { return diag };
        let have_none = values.contains(None);
        // alloc: the diagnostic's list of values
        let possibilities: Vec<&str> = values.order.iter().flatten().copied().collect();
        let is_well_known = expected.well_known.contains(name);
        let maybe = Applicability::MaybeIncorrect;

        let valid_names: Vec<&str> = match &value {
            Some((value, _)) if is_well_known => {
                let mut names: Vec<&str> = well_known::WELL_KNOWN
                    .iter()
                    .map(|&(it, ..)| it)
                    .filter(|it| expected.has_value(it, Some(*value)))
                    // alloc: the diagnostic's list of names
                    .collect();
                names.sort_unstable();
                names
            }
            // alloc: empty, allocates nothing
            _ => Vec::new(),
        };
        if let Some((value, _)) = &value
            && !valid_names.is_empty()
        {
            for it in valid_names.into_iter().take(3) {
                diag.suggestion_verbose(
                    // clone: the diagnostic's span
                    name_span.clone(),
                    // alloc: the diagnostic's text
                    format!("`{value}` is an expected value for `{it}`"),
                    it,
                    maybe,
                );
            }
        } else if !possibilities.is_empty() {
            // clone: the diagnostic's list of values, sorted
            let mut sorted = possibilities.clone();
            sorted.sort_unstable();
            let shown = sorted.len().min(MAX_SHOWN);
            let and_more = sorted.len() - shown;
            sorted.truncate(shown);
            let none = if have_none { "(none), " } else { "" };
            let more = match and_more {
                // alloc: empty, allocates nothing
                0 => String::new(),
                // alloc: the diagnostic's text
                n => format!(" and {n} more"),
            };
            // alloc: the diagnostic's text
            diag.note(format!("expected values for `{name}` are: {none}{}{more}", listed(&sorted, |it| it)));
            match &value {
                Some((value, value_span)) => {
                    if let Some(best) = find_best_match_for_name(&possibilities, value, None).copied() {
                        diag.suggestion(
                            // clone: the diagnostic's span
                            value_span.clone(),
                            "there is a expected value with a similar name",
                            // alloc: the diagnostic's text
                            format!("\"{best}\""),
                            maybe,
                        );
                    }
                }
                None => {
                    if let [only] = possibilities[..] {
                        // alloc: the diagnostic's text
                        diag.suggestion(name_span.shrink_to_hi(), "specify a config value", format!(" = \"{only}\""), maybe);
                    }
                }
            }
        } else if have_none {
            if let Some((_, value_span)) = &value {
                diag.suggestion(name_span.shrink_to_hi().to(value_span), "remove the value", "", maybe);
            }
            // alloc: the diagnostic's text
            diag.note(format!("no expected value for `{name}`"));
        } else {
            let span = match &value {
                Some((_, value_span)) => name_span.to(value_span),
                // clone: the diagnostic's span
                None => name_span.clone(),
            };
            diag.suggestion(span, "remove the condition", "", maybe);
            // alloc: the diagnostic's text
            diag.note(format!("no expected values for `{name}`"));
        }

        let can_add_value = !is_well_known;
        let arg = |quotes| check_cfg_arg(&meta::shown(name, edition), value.as_ref().map(|(it, _)| *it), quotes);
        if self.check.cargo {
            if name == "feature" && !origin.external {
                match &value {
                    // alloc: the diagnostic's text
                    Some((value, _)) => diag.help(format!("consider adding `{value}` as a feature in `Cargo.toml`")),
                    None => diag.help("consider defining some features in `Cargo.toml`"),
                };
            } else if can_add_value && !origin.external {
                self.cargo_help(&mut diag, &arg);
            }
            self.macro_help(&mut diag, &origin);
            diag.note(CARGO_NOTE);
        } else {
            if can_add_value {
                // alloc: the diagnostic's text
                diag.help(format!("to expect this configuration use `--check-cfg={}`", arg(false)));
            }
            self.macro_help(&mut diag, &origin);
            diag.note(RUSTC_NOTE);
        }
        diag
    }

    /// rustc's `cargo_help_sub`: no `build.rs` help in a build script.
    fn cargo_help(&self, diag: &mut Diag, arg: &impl Fn(bool) -> String) {
        let unescaped = arg(false);
        diag.help("consider using a Cargo feature instead");
        // alloc: the diagnostic's text, as is all of `cargo_help`'s and `macro_help`'s
        diag.help(format!(
            "or consider adding in `Cargo.toml` the `check-cfg` lint config for the lint:\n [lints.rust]\n unexpected_cfgs = {{ level = \"warn\", check-cfg = ['{unescaped}'] }}"
        ));
        if !self.build_script {
            // alloc: as above
            diag.help(format!(
                "or consider adding `println!(\"cargo::rustc-check-cfg={}\");` to the top of the `build.rs`",
                arg(true)
            ));
        }
    }

    /// rustc's `rustc_macro_help` and `cargo_macro_help`, which say the same.
    fn macro_help(&self, diag: &mut Diag, origin: &Origin) {
        if let Some((kind, path)) = &origin.macro_help {
            // alloc: as above
            diag.note(format!(
                "using a cfg inside a {kind} will use the cfgs from the destination crate and not the ones from the defining crate"
            ));
            // alloc: as above
            diag.help(format!("try referring to `{path}` crate for guidance on how handle this unexpected cfg"));
        }
    }

    /// The edition of the token at `range` of `file`, as rustc has it of the token's span: the
    /// names are shown (raw or not) in it.
    fn edition_at(&self, file: HirFileId, range: TextRange) -> Edition {
        match file.macro_file() {
            None => self.cx.edition,
            Some(call) => call.expansion_span_map(self.cx.db).span_at(range.start()).ctx.edition(self.cx.db),
        }
    }

    /// Of the name at `range` of `file`: whether a macro made it, and which.
    fn origin(&self, file: HirFileId, range: TextRange) -> Origin {
        let db = self.cx.db;
        let Some(call) = file.macro_file() else { return Origin::default() };
        let written = InFile::new(file, range)
            .original_node_file_range_opt(db)
            .is_some_and(|(_, ctxt)| ctxt.is_root());
        if written {
            return Origin::default();
        }
        let loc = call.loc(db);
        let other_crate = hir::Crate::from(loc.def.krate) != self.cx.krate;
        let (kind, bang) = match &loc.kind {
            MacroCallKind::FnLike { .. } => ("macro", true),
            MacroCallKind::Derive { .. } => ("derive macro", false),
            MacroCallKind::Attr { .. } => ("attribute macro", false),
        };
        let macro_help = match other_crate {
            true => self.macro_path(call).map(|it| (kind, it)),
            false => None,
        };
        Origin { external: !bang || other_crate, macro_help }
    }

    /// The path of the macro `call` calls, as written (rustc's name of an `ExpnKind::Macro`).
    fn macro_path(&self, call: MacroCallId) -> Option<String> {
        let db = self.cx.db;
        let loc = call.loc(db);
        let node = loc.to_node_view(db).value;
        // alloc: the diagnostic's text, a macro's path
        let path = |it: ast::Path<'_>| -> String { it.syntax().text().split_whitespace().collect() };
        match &loc.kind {
            MacroCallKind::FnLike { .. } => ast::MacroCall::cast(node)?.path().map(path),
            MacroCallKind::Derive { derive_index, .. } => {
                let meta = ast::Meta::cast(node)?;
                derive_paths_view(meta).into_iter().nth(*derive_index as usize).map(|(_, it)| it)
            }
            MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                censored_attr_ids.invoc_attr().find_attr_range_view(db, loc.krate, *ast_id).1.path().map(path)
            }
        }
    }

    /// The node the lint of a cfg read at `anchor` is buffered for: the nearest that has an id
    /// when rustc reads it, through the macro call an expansion came from and the `mod` item a
    /// file is the module of.
    fn at(&self, anchor: &Anchor<'db>) -> InFile<SyntaxNodePtr> {
        let cx = self.cx;
        let Anchor { mut file, mut node, mut include } = *anchor;
        loop {
            let start = if include { Some(node) } else { node.parent() };
            let mut top = node;
            for it in std::iter::successors(start, |it| it.parent()) {
                if has_lint_id(it) {
                    return InFile::new(file, ast::ptr(it));
                }
                top = it;
            }
            if let Some(call) = file.macro_file() {
                let loc = call.loc(cx.db);
                let called = loc.to_node_view(cx.db);
                include = !matches!(loc.kind, MacroCallKind::Attr { .. });
                (file, node) = (called.file_id, called.value);
                continue;
            }
            let declared = match file == self.root {
                true => None,
                false => cx
                    .vsema
                    .hir_file_to_module_defs(file)
                    .find(|it| it.krate(cx.db) == cx.krate)
                    .and_then(|it| it.declaration_source_view(cx.db)),
            };
            let Some(declared) = declared else { return InFile::new(file, ast::ptr(top)) };
            (file, node, include) = (declared.file_id, declared.value.syntax(), false);
        }
    }
}

const RUSTC_NOTE: &str = "see <https://doc.rust-lang.org/nightly/rustc/check-cfg.html> for more information about checking conditional configuration";
const CARGO_NOTE: &str = "see <https://doc.rust-lang.org/nightly/rustc/check-cfg/cargo-specifics.html> for more information about checking conditional configuration";

/// rustc's `to_check_cfg_arg`, the value's quotes escaped for `build.rs` (`quotes`).
fn check_cfg_arg(name: &str, value: Option<&str>, quotes: bool) -> String {
    match value {
        Some(value) => {
            // alloc: the diagnostic's text, as is all of `check_cfg_arg`'s
            let value = value.escape_debug().to_string();
            let values = match quotes {
                // alloc: as above
                true => format!("\\\"{}\\\"", value.replace('"', "\\\\\\\\\"")),
                // alloc: as above
                false => format!("\"{value}\""),
            };
            // alloc: as above
            format!("cfg({name}, values({values}))")
        }
        // alloc: as above
        None => format!("cfg({name})"),
    }
}

/// rustc's `DiagSymbolList`: each in backticks, joined by `and` (with commas from three).
fn listed<'s, S: std::fmt::Display>(items: &[&'s str], show: impl Fn(&'s str) -> S) -> String {
    // alloc: the diagnostic's text, as is all of `listed`'s
    let items: Vec<String> = items.iter().map(|&it| format!("`{}`", show(it))).collect();
    match items.as_slice() {
        // alloc: empty, allocates nothing
        [] => String::new(),
        // clone: as above
        [one] => one.clone(),
        // alloc: as above
        [a, b] => format!("{a} and {b}"),
        // alloc: as above
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Whether rustc's expansion has given `node` an id by the time it reads the cfgs in it: an
/// item, a statement with a `;`, an expression, a pattern, a type and the other nodes
/// `assign_id!` numbers; not a macro call, nor a block that is not an expression (a function's
/// body, a loop's, the `then` of an `if`, the `else` of a `let`).
fn has_lint_id(node: ast::Node<'_>) -> bool {
    use SyntaxKind::*;
    let kind = node.kind();
    match kind {
        MACRO_CALL | MACRO_EXPR | MACRO_PAT | MACRO_TYPE | MACRO_ITEMS | MACRO_STMTS | SOURCE_FILE => false,
        BLOCK_EXPR => !bare_block(node),
        VARIANT | WHERE_PRED | RECORD_FIELD | TUPLE_FIELD | RECORD_PAT_FIELD | RECORD_EXPR_FIELD | PARAM | SELF_PARAM
        | TYPE_PARAM | LIFETIME_PARAM | CONST_PARAM | MATCH_ARM | LET_STMT => true,
        EXPR_STMT => ast::ExprStmt::cast(node).is_some_and(|it| it.semicolon_token().is_some()),
        _ => ast::Item::can_cast(kind) || ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind) || ast::Type::can_cast(kind),
    }
}

/// Whether the block `node` is one rustc's AST has as a block, not as an expression.
fn bare_block(node: ast::Node<'_>) -> bool {
    let Some(parent) = node.parent() else { return false };
    let body = match parent.kind() {
        SyntaxKind::FN | SyntaxKind::LET_ELSE => return true,
        SyntaxKind::IF_EXPR => ast::IfExpr::cast(parent).and_then(|it| it.then_branch()),
        SyntaxKind::LOOP_EXPR => ast::LoopExpr::cast(parent).and_then(|it| it.loop_body()),
        SyntaxKind::WHILE_EXPR => ast::WhileExpr::cast(parent).and_then(|it| it.loop_body()),
        SyntaxKind::FOR_EXPR => ast::ForExpr::cast(parent).and_then(|it| it.loop_body()),
        _ => None,
    };
    body.is_some_and(|it| it.syntax() == node)
}

/// rustc's `parse_cfg_entry` of a predicate as the AST has it: each name it checks, in order.
/// What rustc refuses (`not` of other than one, a list it does not know, a value that is no
/// string) checks nothing in it.
fn ast_checks(predicate: ast::CfgPredicate<'_>, out: &mut Vec<Check>) {
    match predicate {
        ast::CfgPredicate::CfgAtom(atom) => {
            if let Some(check) = atom_check(atom, None) {
                out.push(check);
            }
        }
        ast::CfgPredicate::CfgComposite(composite) => {
            let Some(keyword) = composite.keyword() else { return };
            match keyword.text() {
                "any" | "all" => composite.cfg_predicates().for_each(|it| ast_checks(it, out)),
                "not" => {
                    let mut predicates = composite.cfg_predicates();
                    if let (Some(only), None) = (predicates.next(), predicates.next()) {
                        ast_checks(only, out);
                    }
                }
                "target" => {
                    for it in composite.cfg_predicates() {
                        let ast::CfgPredicate::CfgAtom(atom) = it else { continue };
                        if atom.eq_token().is_none() {
                            continue;
                        }
                        if let Some(ident) = atom.ident_token()
                            && meta::is_path_segment_keyword(ident.text())
                        {
                            return;
                        }
                        if let Some(check) = atom_check(atom, Some("target_")) {
                            out.push(check);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The check of `name` or `name = "value"`, its name prefixed in `target(..)`, where the
/// whole is the name's span too.
fn atom_check(atom: ast::CfgAtom<'_>, prefix: Option<&str>) -> Option<Check> {
    let ident = atom.ident_token()?;
    let text = ident.text();
    let (name, raw) = match text.strip_prefix("r#") {
        Some(name) => (name, true),
        None => (text, false),
    };
    if meta::is_path_segment_keyword(name) {
        return None;
    }
    let value = match atom.eq_token() {
        None => None,
        Some(_) => {
            let token = atom.string_token()?;
            let value = Symbol::intern(&ast::String::cast(token)?.value().ok()?);
            Some((value, token.text_range()))
        }
    };
    let range = atom.syntax().text_range();
    Some(match prefix {
        Some(prefix) => Check { name: Symbol::intern_concat(&[prefix, name]), raw, name_range: range, value, range },
        None => Check { name: Symbol::intern(name), raw, name_range: ident.text_range(), value, range },
    })
}

/// rustc's `parse_cfg_entry` of a predicate it parsed from tokens (`cfg_select!`'s).
fn meta_checks(inner: &meta::Inner<'_>, out: &mut Vec<Check>) {
    let Some(item) = inner.meta() else { return };
    match &item.kind {
        meta::Kind::List(list) => match item.ident().map(|it| it.name) {
            Some("any" | "all") => list.iter().for_each(|it| meta_checks(it, out)),
            Some("not") => {
                if let [only] = list.as_slice() {
                    meta_checks(only, out);
                }
            }
            Some("target") => {
                for sub in list {
                    let Some(meta) = sub.meta() else { continue };
                    let (Some(ident), meta::Kind::NameValue(lit)) = (meta.ident(), &meta.kind) else { continue };
                    if meta::is_path_segment_keyword(ident.name) {
                        return;
                    }
                    let Some(value) = lit.value_str() else { continue };
                    out.push(Check {
                        name: Symbol::intern_concat(&["target_", ident.name]),
                        raw: ident.raw,
                        name_range: sub.range(),
                        value: Some((Symbol::intern(&value), lit.range)),
                        range: sub.range(),
                    });
                }
            }
            _ => {}
        },
        meta::Kind::Word | meta::Kind::NameValue(_) => {
            let Some(ident) = item.ident().filter(|it| !meta::is_path_segment_keyword(it.name)) else { return };
            let value = match &item.kind {
                meta::Kind::NameValue(lit) => match lit.value_str() {
                    Some(value) => Some((Symbol::intern(&value), lit.range)),
                    None => return,
                },
                _ => None,
            };
            out.push(Check {
                name: Symbol::intern(ident.name),
                raw: ident.raw,
                name_range: item.path_range(),
                value,
                range: item.range,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_cfg_args_are_written_as_rustc_writes_them() {
        assert_eq!(check_cfg_arg("foo", None, false), "cfg(foo)");
        assert_eq!(check_cfg_arg("foo", Some("bar"), false), r#"cfg(foo, values("bar"))"#);
        assert_eq!(check_cfg_arg("foo", Some("bar"), true), r#"cfg(foo, values(\"bar\"))"#);
        assert_eq!(check_cfg_arg("quote", Some("quote\""), false), r#"cfg(quote, values("quote\""))"#);
        assert_eq!(check_cfg_arg("quote", Some("quote\""), true), r#"cfg(quote, values(\"quote\\\\\"\"))"#);
    }

    /// Reading the cfgs of a tree again allocates nothing: the read list, its sort's scratch and
    /// the checks' buffer are kept, the views are `Copy`, and an expected cfg makes no text.
    #[test]
    fn reading_a_trees_cfgs_again_allocates_nothing() {
        use std::fmt::Write as _;

        for fns in [4, 400] {
            let mut fixture = String::from("//- /lib.rs crate:main edition:2021\n#![cfg_attr(foo, allow(unused))]\n");
            for at in 0..fns {
                writeln!(fixture, "#[cfg_attr(foo, inline)]\n#[cfg(foo)]\npub fn f_{at}() {{}}\npub fn g_{at}(x: u32) -> u32 {{ x }}")
                    .unwrap();
            }
            let checked = crate::tests::checked(&fixture);
            let db = &checked.db;
            hir::attach_db(db, || {
                let krate = hir::check::workspace_crates(db)[0];
                let frame = crate::tests::frame();
                let cx = Cx::new(db, krate, checked.files(), &frame);
                let walk = cx.walk();
                let mut check = CheckCfg::default();
                check.add("cfg(foo)").unwrap();
                let mut lints = Lints {
                    cx: &cx,
                    walk,
                    check: &check,
                    expected: Expected::new(&check, walk.cfg),
                    root: cx.krate.root_module(cx.db).definition_source_file_id(cx.db),
                    build_script: false,
                    emitted: Vec::new(),
                    read: Vec::new(),
                    scratch: Vec::new(),
                    checks: Vec::new(),
                };
                let tree = walk.files().next().unwrap();
                lints.tree(tree);
                assert!(lints.read.len() > fns, "{} read at {fns} functions", lints.read.len());
                blaze_alloc_count::assert_at_most(0, "reading a tree's cfgs again", || lints.tree(tree));
                assert!(lints.emitted.is_empty(), "an expected cfg is not linted");
            });
        }
    }

    #[test]
    fn lists_are_joined_as_rustc_joins_them() {
        assert_eq!(listed(&["a"], |it| it), "`a`");
        assert_eq!(listed(&["a", "b"], |it| it), "`a` and `b`");
        assert_eq!(listed(&["a", "b", "c"], |it| it), "`a`, `b`, and `c`");
    }
}
