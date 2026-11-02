//! The level of `dead_code` at a node, as the dead-code pass asks for it (`lint_level_spec_at_node`):
//! the front end's HIR levels (`hir_lint_levels`), with the lint flags the crate is checked
//! with, kept for the pass. What the attributes set is memoized at each node with any, as
//! rustc's `ShallowLintLevelMap` keeps it per owner: a level walks up only to the nearest node
//! already seen.
//!
//! Follows `compiler/rustc_middle/src/lint.rs` (`lint_level_at_node`) and
//! `compiler/rustc_builtin_macros/src/deriving/generic/mod.rs` (`create_derived_impl`, the
//! attributes copied) (MIT or Apache-2.0).

use std::cell::RefCell;

use hir::InFile;
use hir_lint_levels::{LevelCx, Spec, builder::ExpectId};
use syntax::view as ast;

use crate::{
    cx::Cx,
    lints::registry::{Level, LintId},
};

/// rustc's level and the expectation it fulfills.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LevelPlus {
    pub(super) level: Level,
    pub(super) expect: Option<ExpectId>,
}

impl LevelPlus {
    pub(super) fn is_allow_or_expect(self) -> bool {
        matches!(self.level, Level::Allow | Level::Expect)
    }

    fn of(spec: Spec) -> LevelPlus {
        LevelPlus { level: spec.level, expect: spec.expect }
    }
}

/// The crate's HIR levels, made the first time a level is asked.
pub(super) struct Levels<'a, 'db> {
    built: RefCell<Option<(LevelCx<'a, 'db>, hir_lint_levels::Levels<'db>)>>,
}

impl<'a, 'db> Levels<'a, 'db> {
    pub(super) fn new() -> Levels<'a, 'db> {
        Levels { built: RefCell::new(None) }
    }

    /// The level of `dead_code` at `node`.
    pub(super) fn at(&self, cx: &Cx<'a, 'db>, node: InFile<ast::Node<'db>>) -> LevelPlus {
        self.with(cx, |lcx, levels| {
            let set = levels.set_at(lcx, &mut (), node);
            LevelPlus::of(levels.spec(lcx, LintId::DEAD_CODE.lint(), set))
        })
    }

    /// The level of `dead_code` at an impl a builtin derive of `adt` makes with no tokens:
    /// rustc's `create_derived_impl` puts it beside the ADT and copies the ADT's `allow`,
    /// `warn`, `deny` and `forbid` onto it (not `expect`, and not the ADT's other attributes).
    pub(super) fn at_derived(&self, cx: &Cx<'a, 'db>, adt: InFile<ast::Node<'db>>) -> LevelPlus {
        self.with(cx, |lcx, levels| {
            let set = levels.set_of_derived(lcx, &mut (), adt);
            LevelPlus::of(levels.spec(lcx, LintId::DEAD_CODE.lint(), set))
        })
    }

    fn with<T>(&self, cx: &Cx<'a, 'db>, ask: impl FnOnce(&mut LevelCx<'a, 'db>, &mut hir_lint_levels::Levels<'db>) -> T) -> T {
        let mut built = self.built.borrow_mut();
        let (lcx, levels) = built.get_or_insert_with(|| {
            let mut lcx = LevelCx::new(cx.db, cx.krate.base(), &cx.lint_flags().lints);
            let levels = hir_lint_levels::Levels::new(&mut lcx, &mut (), false, false);
            (lcx, levels)
        });
        ask(lcx, levels)
    }
}
