//! rustc's builtin attribute parsers (`ATTRIBUTE_PARSERS`), one module per group of rustc's
//! `compiler/rustc_attr_parsing/src/attributes/` files, each giving the errors of its parsers:
//!
//! - [`codegen`]: `codegen_attrs.rs`, `inline.rs`, `link_attrs.rs`, `instruction_set.rs`,
//!   `repr.rs`, `cfi_encoding.rs`, `rustc_allocator.rs`, `unroll.rs`, `splat.rs`,
//!   `autodiff.rs`, `prototype.rs`, `loop_match.rs`, `body.rs`.
//! - [`stability`]: `stability.rs`, `deprecation.rs`, `allow_unstable.rs`, `rustc_dump.rs`,
//!   `confusables.rs`, `lint_helpers.rs`, `traits.rs`, `transparency.rs`, `semantics.rs`,
//!   `pin_v2.rs`, `dummy.rs`, `must_not_suspend.rs`, `no_implicit_prelude.rs`.
//! - [`internal`]: `rustc_internal.rs`.
//! - [`misc`]: `crate_level.rs`, `macro_attrs.rs`, `test_attrs.rs`, `proc_macro_attrs.rs`,
//!   `debugger.rs`, `path.rs`, `must_use.rs`, `no_link.rs`, `non_exhaustive.rs`, `prelude.rs`,
//!   and expansion's checks of the attributes no parser takes (`validate_attr.rs`).
//! - [`doc`]: `doc.rs` and `diagnostic/`.
//!
//! A module takes the attributes of its paths ([`codegen::PATHS`], ..): a single parser's
//! conversion is [`Accepted::Single`], and `checks/attrs.rs` keeps the first and says what a
//! second one is; a parser that combines its attributes keeps them in the module's [`Node`]
//! state and gives what they are once all of the node's are seen (`finalize`), after which its
//! cross-attribute checks run (`check`, rustc's `finalize_check`).
//!
//! Follows rustc's `compiler/rustc_attr_parsing/src/attributes/mod.rs` (`AttributeParser`,
//! `Single`, `Combine`, `OnDuplicate`) and `compiler/rustc_attr_parsing/src/context.rs`
//! (`ATTRIBUTE_PARSERS`, `FinalizeContext`, `FinalizeCheckContext`) (MIT or Apache-2.0).

pub(crate) mod codegen;
pub(crate) mod doc;
pub(crate) mod internal;
pub(crate) mod misc;
pub(crate) mod stability;

use hir::InFile;
use hir_def::attrs::stability::RustcVersion;
use syntax::TextRange;

use super::{
    accept::{Accept, Session},
    args::{ArgParser, Lit, NameValue},
    parsed::{Parsed, ParsedAttr},
};
use crate::{
    cx::{Cx, item_range_view as item_range},
    lints::syntax::attrs::{Attr, Owner, Target, table::index},
    model::{Diag, Span},
};

/// What a parser made of an attribute.
pub(crate) enum Accepted {
    /// A single parser's conversion (rustc's `SingleAttributeParser::convert`), `None` where it
    /// gave none.
    Single(Option<Parsed>),
    /// Taken into the state of the group `group` (rustc's `AttributeParser` of several
    /// attributes); what they are comes from the module's `finalize`.
    Group(&'static str),
}

/// A parser group: a single parser, by its path, or a group a module names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupId {
    Single(&'static [&'static str]),
    Named(&'static str),
}

/// The parsers' state of one node.
#[derive(Default)]
pub(crate) struct Node {
    pub(crate) codegen: codegen::State,
    pub(crate) stability: stability::State,
    pub(crate) internal: internal::State,
    pub(crate) misc: misc::State,
    pub(crate) doc: doc::State,
}

/// The module whose parser takes `path`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Module {
    Codegen,
    Stability,
    Internal,
    Misc,
    Doc,
}

pub(crate) fn module(path: &[&str]) -> Option<Module> {
    MODULES[index::of(path)?]
}

/// Each module's paths, in the order a path is looked for in them.
const TAKES: [(&[&[&str]], Module); 5] = [
    (codegen::PATHS, Module::Codegen),
    (stability::PATHS, Module::Stability),
    (internal::PATHS, Module::Internal),
    (misc::PATHS, Module::Misc),
    (doc::PATHS, Module::Doc),
];

/// The module of each builtin parser, by its number: the first of [`TAKES`] with its path.
static MODULES: [Option<Module>; index::COUNT] = {
    let mut out = [None; index::COUNT];
    let mut at = 0;
    while at < index::COUNT {
        let mut module = 0;
        while module < TAKES.len() && out[at].is_none() {
            let (paths, takes) = TAKES[module];
            let mut i = 0;
            while i < paths.len() {
                if index::same_path(paths[i], index::path(at)) {
                    out[at] = Some(takes);
                    break;
                }
                i += 1;
            }
            module += 1;
        }
        at += 1;
    }
    out
};

/// The parser of the attribute `cx.attr`, whose path is `cx.builtin.path`, given its arguments.
pub(crate) fn accept(node: &mut Node, module: Module, cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Accepted {
    match module {
        Module::Codegen => codegen::accept(&mut node.codegen, cx, args),
        Module::Stability => stability::accept(&mut node.stability, cx, args),
        Module::Internal => internal::accept(&mut node.internal, cx, args),
        Module::Misc => misc::accept(&mut node.misc, cx, args),
        Module::Doc => doc::accept(&mut node.doc, cx, args),
    }
}

/// What the group `group` of `module` makes of the node's attributes it took: rustc's
/// `AttributeParser::finalize`.
pub(crate) fn finalize(node: &mut Node, module: Module, group: &'static str, fx: &mut Finalize<'_, '_, '_>) -> Option<Parsed> {
    match module {
        Module::Codegen => codegen::finalize(&mut node.codegen, group, fx),
        Module::Stability => stability::finalize(&mut node.stability, group, fx),
        Module::Internal => internal::finalize(&mut node.internal, group, fx),
        Module::Misc => misc::finalize(&mut node.misc, group, fx),
        Module::Doc => doc::finalize(&mut node.doc, group, fx),
    }
}

/// The cross-attribute checks of the group `group`, once every attribute of the node is
/// parsed: rustc's `finalize_check` (of a single parser, only when it converted one). `first`
/// is the group's first attribute (for a single parser, the one it uses).
pub(crate) fn check(module: Module, group: GroupId, first: usize, fx: &mut Finalize<'_, '_, '_>, parsed: &[ParsedAttr]) {
    match module {
        Module::Codegen => codegen::check(group, first, fx, parsed),
        Module::Stability => stability::check(group, first, fx, parsed),
        Module::Internal => internal::check(group, first, fx, parsed),
        Module::Misc => misc::check(group, first, fx, parsed),
        Module::Doc => doc::check(group, first, fx, parsed),
    }
}

/// What a group is given once all of a node's attributes are seen: rustc's `FinalizeContext`
/// and `FinalizeCheckContext`.
pub(crate) struct Finalize<'a, 'c, 'db> {
    pub(crate) cx: &'a Cx<'c, 'db>,
    pub(crate) session: &'a Session,
    pub(crate) owner: &'a Owner<'db>,
    errors: &'a mut Vec<Diag>,
}

impl<'a, 'c, 'db> Finalize<'a, 'c, 'db> {
    pub(crate) fn new(
        cx: &'a Cx<'c, 'db>,
        session: &'a Session,
        owner: &'a Owner<'db>,
        errors: &'a mut Vec<Diag>,
    ) -> Finalize<'a, 'c, 'db> {
        Finalize { cx, session, owner, errors }
    }

    pub(crate) fn attr(&self, index: usize) -> &'a Attr<'db> {
        &self.owner.attrs[index]
    }

    /// `range` of the file the node's attribute `index` is written in.
    pub(crate) fn span(&self, index: usize, range: TextRange) -> Span {
        self.cx.span(self.attr(index).in_file(range))
    }

    /// rustc's `attr_span` of the node's attribute `index`.
    pub(crate) fn attr_span(&self, index: usize) -> Span {
        let attr = self.attr(index);
        self.span(index, attr.range())
    }

    pub(crate) fn target(&self) -> Target {
        self.owner.target
    }

    /// rustc's `target_span`.
    pub(crate) fn target_span(&self) -> Span {
        let node = self.owner.node;
        self.cx.span(InFile::new(node.file_id, item_range(node.value)))
    }

    pub(crate) fn feature(&self, name: &str) -> bool {
        self.session.feature(name)
    }

    /// rustc's `feature_err(..).emit()` of `feature` at `span`, unless the crate enables it.
    pub(crate) fn gate(&mut self, feature: &str, span: Span, explain: impl Into<String>) {
        if !self.feature(feature) {
            let diag = self.session.feature_err(feature, span, explain);
            self.emit_err(diag);
        }
    }

    pub(crate) fn emit_err(&mut self, diag: Diag) {
        self.errors.push(diag);
    }
}

/// `NoArgsAttributeParser`'s conversion: an error for arguments, and the attribute regardless.
pub(crate) fn no_args(cx: &mut Accept<'_, '_, '_>, args: &ArgParser, parsed: Parsed) -> Accepted {
    let _ = cx.expect_no_args(args);
    Accepted::Single(Some(parsed))
}

/// `util.rs`' `parse_single_integer`: `#[path(N)]`.
pub(crate) fn single_integer(cx: &mut Accept<'_, '_, '_>, args: &ArgParser) -> Option<u128> {
    let single = cx.expect_single_element_list(args, cx.attr_range())?;
    let Some(value) = single.as_lit().and_then(Lit::int) else {
        cx.expected_integer_literal(single.range());
        return None;
    };
    Some(value)
}

/// `util.rs`' `parse_limit_int`: `#![path = "N"]` of a limit.
pub(crate) fn limit(cx: &mut Accept<'_, '_, '_>, nv: &NameValue) -> Option<u64> {
    let limit = cx.expect_string_literal(nv)?;
    let error = match limit.parse::<usize>() {
        Ok(it) => return Some(it as u64),
        Err(e) => match e.kind() {
            std::num::IntErrorKind::PosOverflow => "`limit` is too large",
            std::num::IntErrorKind::Empty => "`limit` must be a non-negative integer",
            _ => "not a valid integer",
        },
    };
    let mut diag = cx.err(None, "`limit` must be a non-negative integer", cx.attr_range());
    diag.span_label(cx.span(nv.value_range()), error);
    cx.emit_err(diag);
    None
}

/// rustc's `parse_version` (hir-def's [`RustcVersion::parse`]), as the tuple the parsed
/// attributes keep.
pub(crate) fn parse_version(text: &str) -> Option<(u16, u16, u16)> {
    RustcVersion::parse(text).map(|it| (it.major, it.minor, it.patch))
}

#[cfg(test)]
mod tests {
    use crate::lints::syntax::attrs::table;

    #[test]
    fn every_builtin_parser_is_in_one_module() {
        for builtin in table::builtins() {
            let path = builtin.path;
            let modules = [
                super::codegen::PATHS,
                super::stability::PATHS,
                super::internal::PATHS,
                super::misc::PATHS,
                super::doc::PATHS,
            ];
            let owning = modules.iter().filter(|it| it.contains(&path)).count();
            assert_eq!(owning, 1, "{path:?} is taken by {owning} modules");
        }
        for paths in [
            super::codegen::PATHS,
            super::stability::PATHS,
            super::internal::PATHS,
            super::misc::PATHS,
            super::doc::PATHS,
        ] {
            for path in paths {
                assert!(table::builtin(path).is_some(), "{path:?} is not a builtin parser's path");
            }
        }
    }

    fn by_scan(path: &[&str]) -> Option<super::Module> {
        super::TAKES.iter().find(|(paths, _)| paths.contains(&path)).map(|(_, module)| *module)
    }

    #[test]
    fn modules_are_as_by_a_scan() {
        let mut paths: Vec<Vec<&str>> = table::builtins().iter().map(|it| it.path.to_vec()).collect();
        for (module_paths, _) in super::TAKES {
            paths.extend(module_paths.iter().map(|it| it.to_vec()));
        }
        paths.extend([vec![], vec!["diagnostic"], vec!["no_such_attribute"], vec!["inline", "x"], vec!["Inline"]]);
        for path in &paths {
            assert_eq!(super::module(path), by_scan(path), "{path:?}");
        }
        for builtin in table::builtins() {
            assert!(super::module(builtin.path).is_some(), "{:?}", builtin.path);
        }
    }
}
