//! What rustc's `LintLevelsBuilder` says, worded: the front end's builder
//! (`hir_lint_levels::Levels`) hands what it finds wrong with the command line and the
//! attributes to [`Said`], which makes rustc's diagnostic of each at the level of the lint it
//! is of where the builder is, and keeps them for when rustc gives them.
//!
//! Follows rustc's `compiler/rustc_lint/src/levels.rs` (`add_command_line`, `insert_spec`,
//! `add`, `check_gated_lint`), `compiler/rustc_lint/src/lints.rs` and
//! `compiler/rustc_lint/src/errors.rs` (the texts) and `compiler/rustc_session/src/parse.rs`
//! (`add_feature_diagnostics_for_issue`) (MIT or Apache-2.0).

use blaze_ids::bits::BitSet;
use hir::InFile;
use hir_lint_levels::{
    Event, LevelCx, Levels, Malformed, SetId, Sink, Source,
    attrs as front,
    builder::{COMMAND_LINE, ExpectId},
};
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use super::attrs;
use crate::{
    cx::Cx,
    lints::registry::{self, Checked, Level},
    lower::mutable_ref_binding::BUILT_ON,
    model::{Applicability, Code, Diag, MultiSpan, Span},
};

/// Where rustc's walk of the crate comes to a node: [`attrs::place`].
type Place = Vec<(u32, u32)>;

/// Which of rustc's builders gives a diagnostic of lint levels. Each says what is wrong on the
/// command line; the pre-expansion one and the early one also say what is wrong with an
/// attribute that is not a lint (E0452, E0710), the pre-expansion one only of the crate's
/// root file as parsed; only the early one gives the lints of the attributes
/// (`lint_added_lints`). Written in full (`-Z deduplicate-diagnostics=no`), each is a copy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum When {
    /// rustc's pre-expansion lint pass, after the parser.
    Pre,
    /// The early lint pass, after resolution, as it comes to the node.
    Early,
    /// The HIR's levels of the crate's root, first asked for as the analysis starts.
    Hir,
}

/// A diagnostic of lint levels, and when rustc gives it.
pub(super) struct Emitted {
    pub(super) when: When,
    /// Where the walk of the crate comes to the node; nothing for the command line's.
    pub(super) place: Place,
    pub(super) diag: Diag,
}

/// What the builders of a crate's check say, and which `#[expect]`s a lint fulfilled.
pub(super) struct Said<'a, 'b, 'db> {
    pub(super) cx: &'a Cx<'b, 'db>,
    /// What the early builder says, as the pre-expansion pass and as the early one.
    pub(super) early: Vec<Emitted>,
    /// What the HIR's builder says.
    pub(super) hir: Vec<Emitted>,
    /// Bit `i`: whether a lint at its level fulfilled the expectation of index `i`.
    fulfilled: BitSet,
}

impl<'a, 'b, 'db> Said<'a, 'b, 'db> {
    pub(super) fn new(cx: &'a Cx<'b, 'db>) -> Self {
        // alloc: empty, allocates nothing
        Said { cx, early: Vec::new(), hir: Vec::new(), fulfilled: BitSet::default() }
    }

    pub(super) fn fulfill(&mut self, id: ExpectId) {
        self.fulfilled.insert_growing(id.index());
    }

    pub(super) fn is_fulfilled(&self, id: ExpectId) -> bool {
        self.fulfilled.contains(id.index())
    }

    fn place(&self, node: InFile<ast::Node<'_>>) -> Place {
        attrs::place(&self.cx.vsema, node)
    }

    /// `diag` of a lint at its level in `set` as it is now (rustc's `emit_span_lint` from the
    /// builder), with `code` for its code when it has one of its own.
    fn lint(&mut self, levels: &Levels<'db>, lcx: &LevelCx<'_, 'db>, set: SetId, pre: bool, place: Place, diag: Diag, code: Option<&str>) {
        let name = diag.lint_name().expect("a lint's diagnostic");
        let lint = registry::lint(name).unwrap_or_else(|| panic!("no lint `{name}`"));
        let spec = levels.spec(lcx, lint, set);
        if let Some(mut diag) = self.finish(lcx, lint, spec, diag) {
            if let Some(code) = code {
                // alloc: a diagnostic's code
                diag.code = Some(Code::Error(code.to_owned()));
            }
            self.push(levels, pre, place, diag);
        }
    }

    /// `diag` as each builder rustc has for this one gives it: the HIR's once; the early
    /// one's also as the pre-expansion pass when it is `pre`, of the command line or of the
    /// root file as parsed.
    fn push(&mut self, levels: &Levels<'_>, pre: bool, place: Place, diag: Diag) {
        if !levels.is_early() {
            self.hir.push(Emitted { when: When::Hir, place, diag });
            return;
        }
        if pre {
            // clone: a diagnostic both passes give, once per diagnostic
            self.early.push(Emitted { when: When::Pre, place: place.clone(), diag: diag.clone() });
        }
        self.early.push(Emitted { when: When::Early, place, diag });
    }

    /// What rustc says of a name on the command line, `given` (`tool::` and `name`).
    fn command_line(&mut self, levels: &Levels<'db>, lcx: &LevelCx<'_, 'db>, given: &str, tool: Option<&str>, name: &str, level: Level, checked: &Checked) {
        // alloc: a diagnostic's note, made only for one
        let requested = || format!("requested on the command line with `{} {given}`", level.to_cmd_flag());
        let lint = |name: &str, message: String| {
            // alloc: a diagnostic's code
            Diag::new(crate::model::Level::Warning, Some(Code::Lint(name.to_owned())), message)
        };
        match checked {
            Checked::Renamed(new) => {
                // alloc: a diagnostic's message
                let mut diag = lint("renamed_and_removed_lints", format!("lint `{given}` has been renamed to `{new}`"));
                // alloc: as above
                diag.help(format!("use the new name `{new}`"));
                diag.note(requested());
                // alloc: empty, allocates nothing
                self.lint(levels, lcx, COMMAND_LINE, true, Vec::new(), diag, None);
            }
            Checked::Removed(reason) => {
                // alloc: a diagnostic's message
                let mut diag = lint("renamed_and_removed_lints", format!("lint `{given}` has been removed: {reason}"));
                diag.note(requested());
                // alloc: empty, allocates nothing
                self.lint(levels, lcx, COMMAND_LINE, true, Vec::new(), diag, None);
            }
            Checked::NoLint { suggest } => {
                // alloc: a diagnostic's message
                let mut diag = lint("unknown_lints", format!("unknown lint: `{given}`"));
                if *suggest && let Some((replace, from_rustc)) = registry::suggestion(&lcx.store, name, tool) {
                    diag.help(match from_rustc {
                        // alloc: as above
                        true => format!("a lint with a similar name exists in `rustc` lints: `{replace}`"),
                        // alloc: as above
                        false => format!("did you mean: `{replace}`"),
                    });
                }
                diag.note(requested());
                // alloc: empty, allocates nothing
                self.lint(levels, lcx, COMMAND_LINE, true, Vec::new(), diag, Some("E0602"));
            }
            Checked::Tool(_, Some(new)) => {
                let mut diag = lint(
                    "renamed_and_removed_lints",
                    // alloc: a diagnostic's message
                    format!("lint name `{given}` is deprecated and may not have an effect in the future"),
                );
                // alloc: as above
                diag.help(format!("change it to {new}"));
                diag.note(requested());
                // alloc: empty, allocates nothing
                self.lint(levels, lcx, COMMAND_LINE, true, Vec::new(), diag, None);
            }
            Checked::NoTool => {
                let tool = tool.unwrap_or_default();
                let mut diag = Diag::new(
                    crate::model::Level::Error,
                    // alloc: an error's code
                    Some(Code::Error("E0602".to_owned())),
                    // alloc: an error's message
                    format!("unknown lint tool: `{tool}`"),
                );
                diag.note(requested());
                // alloc: empty, allocates nothing
                self.push(levels, true, Vec::new(), diag);
            }
            Checked::Ok(_) | Checked::Tool(_, None) | Checked::MissingTool => {}
        }
    }

    /// What rustc says of a name in a lint attribute of `node`, at `at`.
    fn name(
        &mut self,
        levels: &Levels<'db>,
        lcx: &LevelCx<'_, 'db>,
        set: SetId,
        node: InFile<ast::Node<'db>>,
        at: InFile<syntax::TextRange>,
        tool: Option<(&str, syntax::TextRange)>,
        name: &str,
        checked: &Checked,
        pre: bool,
    ) {
        let cx = self.cx;
        let span = || cx.span(at);
        // The name as written, with its tool: for a diagnostic's message.
        let full = || match tool {
            // alloc: a diagnostic's message
            Some((tool, _)) => format!("{tool}::{name}"),
            // alloc: as above
            None => name.to_owned(),
        };
        let lint_diag = |lint: &str, message: String| Diag::lint(lint, message, span());
        match checked {
            Checked::Tool(_, Some(new)) => {
                let mut diag = lint_diag(
                    "renamed_and_removed_lints",
                    // alloc: a diagnostic's message
                    format!("lint name `{name}` is deprecated and may not have an effect in the future"),
                );
                // clone: the diagnostic's suggestion
                diag.suggestion(span(), "change it to", new.clone(), Applicability::MachineApplicable);
                self.lint(levels, lcx, set, false, self.place(node), diag, None);
            }
            Checked::NoTool => {
                let (tool, range) = tool.expect("a tool's lint");
                let mut diag = Diag::error(
                    Some("E0710"),
                    // alloc: a diagnostic's message
                    format!("unknown tool name `{tool}` found in scoped lint: `{tool}::{name}`"),
                    cx.span(InFile::new(at.file_id, range)),
                );
                // alloc: as above
                diag.help(format!("add `#![register_tool({tool})]` to the crate root"));
                self.push(levels, pre, self.place(node), diag);
            }
            Checked::Renamed(new) => {
                // alloc: a diagnostic's message
                let mut diag = lint_diag("renamed_and_removed_lints", format!("lint `{}` has been renamed to `{new}`", full()));
                diag.suggestion(span(), "use the new name", *new, Applicability::MachineApplicable);
                self.lint(levels, lcx, set, false, self.place(node), diag, None);
            }
            Checked::Removed(why) => {
                // alloc: a diagnostic's message
                let diag = lint_diag("renamed_and_removed_lints", format!("lint `{}` has been removed: {why}", full()));
                self.lint(levels, lcx, set, false, self.place(node), diag, None);
            }
            Checked::NoLint { suggest } => {
                // alloc: a diagnostic's message
                let mut diag = lint_diag("unknown_lints", format!("unknown lint: `{}`", full()));
                if *suggest
                    && let Some((replace, from_rustc)) = registry::suggestion(&lcx.store, name, tool.map(|(tool, _)| tool))
                {
                    let message = match from_rustc {
                        true => "a lint with a similar name exists in `rustc` lints",
                        false => "did you mean",
                    };
                    diag.suggestion(span(), message, replace, Applicability::MaybeIncorrect);
                }
                self.lint(levels, lcx, set, false, self.place(node), diag, None);
            }
            Checked::Ok(_) | Checked::Tool(_, None) | Checked::MissingTool => {}
        }
    }
}

impl<'db> Sink<'db> for Said<'_, '_, 'db> {
    fn event(&mut self, levels: &Levels<'db>, lcx: &LevelCx<'_, 'db>, set: SetId, event: Event<'_, 'db>) {
        let cx = self.cx;
        match event {
            Event::ForceWarnWarnings => {
                let mut diag = Diag::new(
                    crate::model::Level::Error,
                    // alloc: an error's code
                    Some(Code::Error("E0602".to_owned())),
                    "`warnings` lint group is not supported with ´--force-warn´",
                );
                diag.span = MultiSpan::default();
                // alloc: empty, allocates nothing
                self.push(levels, true, Vec::new(), diag);
            }
            Event::CommandLine { given, tool, name, level, checked } => {
                self.command_line(levels, lcx, given, tool, name, level, checked);
            }
            Event::Malformed { node, at, what, pre } => {
                let label = match what {
                    Malformed::ReasonNotString => "reason must be a string literal",
                    Malformed::BadArgument => "bad attribute argument",
                    Malformed::ReasonNotLast => "reason in lint attribute must come last",
                };
                let diag = malformed(cx.span(at), label);
                self.push(levels, pre, self.place(node), diag);
            }
            Event::Name { node, at, tool, name, checked, pre } => {
                self.name(levels, lcx, set, node, at, tool, name, checked, pre);
            }
            Event::Overruled { node, lint, new, old, groups } => {
                let Source::Node { name, at, .. } = lcx.source(new.source) else {
                    unreachable!("the builder says only an attribute's level is overruled")
                };
                let span = cx.span(*at);
                // alloc: an error's message
                let message = format!("{}({name}) incompatible with previous forbid", new.level.as_str());
                let mut diag = match groups {
                    // clone: the error's span, used twice
                    true => Diag::lint("forbidden_lint_groups", message, span.clone()),
                    // clone: as above
                    false => Diag::error(Some("E0453"), message, span.clone()),
                };
                diag.span_label(span, "overruled by previous forbid");
                match lcx.source(old.source) {
                    Source::Default => {
                        // alloc: an error's note
                        diag.note(format!("`forbid` lint level is the default for {}", lint.name));
                    }
                    Source::Node { at, reason, .. } => {
                        diag.span_label(cx.span(*at), "`forbid` level set here");
                        if let Some(reason) = reason {
                            diag.note(reason.as_str());
                        }
                    }
                    Source::CommandLine { name, .. } => {
                        // alloc: an error's note
                        diag.note(format!("`forbid` lint level was set on command line (`-F {name}`)"));
                    }
                }
                match groups {
                    true => self.lint(levels, lcx, set, false, self.place(node), diag, None),
                    false => self.push(levels, false, self.place(node), diag),
                }
            }
            Event::Gated { node, lint, feature, at } => {
                let mut diag = Diag::new(
                    crate::model::Level::Warning,
                    // alloc: a warning's code
                    Some(Code::Lint("unknown_lints".to_owned())),
                    // alloc: a warning's message
                    format!("unknown lint: `{}`", lint.name),
                );
                if let Some(at) = at {
                    diag.span = MultiSpan::of(cx.span(at));
                }
                // alloc: a warning's note
                diag.note(format!("the `{}` lint is unstable", lint.name));
                if let Some(issue) = feature.issue {
                    // alloc: as above
                    diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
                }
                diag.help(match at {
                    // alloc: a warning's help
                    None => format!("add `-Zcrate-attr=\"feature({})\"` to the command-line options to enable", feature.name),
                    // alloc: as above
                    Some(_) => format!("add `#![feature({})]` to the crate attributes to enable", feature.name),
                });
                // alloc: a warning's note
                diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
                // alloc: empty for the command line's, allocates nothing
                let place = node.map_or_else(Vec::new, |node| self.place(node));
                self.lint(levels, lcx, set, false, place, diag, None);
            }
            Event::CrateLevelOnly { node, level, name, at } => {
                // alloc: a warning's message
                let message = format!("{}({name}) is ignored unless specified at crate level", level.as_str());
                let diag = Diag::lint("unused_attributes", message, cx.span(at));
                self.lint(levels, lcx, set, false, self.place(node), diag, None);
            }
        }
    }
}

/// E0452 "malformed lint attribute input" with its label.
fn malformed(at: Span, label: &str) -> Diag {
    // clone: the error's span, used twice
    let mut diag = Diag::error(Some("E0452"), "malformed lint attribute input", at.clone());
    diag.span_label(at, label);
    diag
}

/// Where the HIR has the `#[expect]`s of `node`, for the order rustc checks them in
/// (`ModuleItems::owners`): the crate, then free items, trait items, impl items and foreign
/// items, each in order; the owner is the item `node` is in.
pub(super) fn owner_rank<'db>(cx: &Cx<'_, 'db>, lcx: &LevelCx<'_, 'db>, node: InFile<ast::Node<'db>>) -> (u8, Place) {
    let sema = &cx.vsema;
    let up = |at| front::parent(lcx.db, lcx.krate, at);
    let mut at = node;
    loop {
        let kind = at.value.kind();
        if kind == SyntaxKind::SOURCE_FILE && at.file_id == lcx.root {
            return (0, attrs::place(sema, at));
        }
        let around = up(at);
        if ast::Item::can_cast(kind) && kind != SyntaxKind::MACRO_CALL {
            let rank = match around.map(|it| it.value.kind()) {
                Some(SyntaxKind::ASSOC_ITEM_LIST) => match around.and_then(up).map(|it| it.value.kind()) {
                    Some(SyntaxKind::TRAIT) => 2,
                    _ => 3,
                },
                Some(SyntaxKind::EXTERN_ITEM_LIST) => 4,
                _ => 1,
            };
            return (rank, attrs::place(sema, at));
        }
        match around {
            Some(around) => at = around,
            None => return (0, attrs::place(sema, at)),
        }
    }
}
